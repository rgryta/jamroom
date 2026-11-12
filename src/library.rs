//! The music library: indexing, lookup, file placement and tagging.

use crate::{
    db::{Db, LibEntry},
    norm::{artist_match, norm, sanitize},
};
use anyhow::{Result, anyhow};
use lofty::{
    config::WriteOptions,
    file::{AudioFile, TaggedFileExt},
    picture::{MimeType, Picture, PictureType},
    tag::{Accessor, ItemKey, Tag},
};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

const AUDIO_EXTS: [&str; 5] = ["flac", "mp3", "m4a", "ogg", "opus"];

pub fn is_audio(p: &Path) -> bool {
    p.extension().and_then(|e| e.to_str()).is_some_and(|e| AUDIO_EXTS.contains(&e.to_ascii_lowercase().as_str()))
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        match e.file_type() {
            Ok(t) if t.is_dir() => walk(&p, out),
            Ok(t) if t.is_file() && is_audio(&p) => out.push(p),
            _ => {}
        }
    }
}

fn mtime(p: &Path) -> i64 {
    std::fs::metadata(p)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Artist and title for a file with no usable tags, from `<root>/[Non-Album/]<Artist>/.../<Title>.ext`.
fn names_from_path(path: &Path, root: &Path) -> (String, String) {
    let rel = path.strip_prefix(root).unwrap_or(path);
    let parts: Vec<String> = rel.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
    let artist_at = if parts.first().is_some_and(|p| p == "Non-Album") { 1 } else { 0 };
    let artist = if parts.len() > artist_at + 1 { parts[artist_at].clone() } else { String::new() };
    let title = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    (artist, title)
}

pub fn read_entry(path: &Path, root: &Path) -> Option<LibEntry> {
    let tagged = lofty::read_from_path(path).ok()?;
    let duration = tagged.properties().duration().as_secs_f64();
    let tag = tagged.primary_tag().or_else(|| tagged.first_tag());
    let mut artist = tag.and_then(|t| t.artist().map(|c| c.to_string())).unwrap_or_default();
    let mut title = tag.and_then(|t| t.title().map(|c| c.to_string())).unwrap_or_default();
    if artist.trim().is_empty() || title.trim().is_empty() {
        let (a, t) = names_from_path(path, root);
        if artist.trim().is_empty() {
            artist = a;
        }
        if title.trim().is_empty() {
            title = t;
        }
    }
    let tn = norm(&title);
    if tn.is_empty() {
        return None;
    }
    Some(LibEntry {
        path: path.to_string_lossy().into_owned(),
        mtime: mtime(path),
        artist,
        title,
        tn,
        duration: (duration > 0.0).then_some(duration),
    })
}

/// Bring the index in line with the files on disk. Blocking; run it off the async runtime.
pub fn scan(db: &Db, root: &Path) -> Result<(usize, usize)> {
    let known = db.lib_known()?;
    let mut files = Vec::new();
    walk(root, &mut files);
    let seen: HashSet<String> = files.iter().map(|p| p.to_string_lossy().into_owned()).collect();
    let mut batch = Vec::new();
    for p in &files {
        let key = p.to_string_lossy();
        if known.get(key.as_ref()) == Some(&mtime(p)) {
            continue;
        }
        if let Some(e) = read_entry(p, root) {
            batch.push(e);
        }
        if batch.len() >= 500 {
            db.lib_upsert_many(&batch)?;
            batch.clear();
        }
    }
    db.lib_upsert_many(&batch)?;
    let gone: Vec<String> = known.keys().filter(|k| !seen.contains(*k)).cloned().collect();
    db.lib_delete(&gone)?;
    Ok((files.len(), gone.len()))
}

/// A library file that already holds this track: same title, matching artist, length within 3 s.
pub fn find(db: &Db, artist: &str, title: &str, duration: Option<f64>) -> Option<String> {
    let tn = norm(title);
    if tn.is_empty() || artist.trim().is_empty() {
        return None;
    }
    let mut hits: Vec<String> = db
        .lib_by_title(&tn)
        .ok()?
        .into_iter()
        .filter(|(_, a, d)| {
            artist_match(a, artist)
                && match (duration, d) {
                    (Some(want), Some(have)) => (want - have).abs() <= 3.0,
                    _ => true,
                }
        })
        .map(|h| h.0)
        .collect();
    hits.sort_by_key(|p| !p.ends_with(".flac"));
    hits.into_iter().next()
}

/// Where a newly cached track goes: `<Artist>/<Album>/<Title>.ext`, or `Non-Album/<Artist>/<Title>.ext`.
pub fn target_path(root: &Path, dir_artist: &str, album: Option<&str>, title: &str, ext: &str) -> PathBuf {
    let file = format!("{}.{}", sanitize(title), ext);
    match album.map(str::trim).filter(|a| !a.is_empty()) {
        Some(al) => root.join(sanitize(dir_artist)).join(sanitize(al)).join(file),
        None => root.join("Non-Album").join(sanitize(dir_artist)).join(file),
    }
}

pub fn probe_duration(path: &Path) -> Option<f64> {
    let t = lofty::read_from_path(path).ok()?;
    Some(t.properties().duration().as_secs_f64()).filter(|d| *d > 0.0)
}

#[cfg(unix)]
fn set_mode(p: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode));
}

#[cfg(not(unix))]
fn set_mode(_: &Path, _: u32) {}

/// Copy a finished file into the library. Directories are created one level at a time with
/// group-writable permissions, and the file appears only when complete.
pub fn place(src: &Path, dest: &Path, root: &Path) -> Result<()> {
    let parent = dest.parent().ok_or_else(|| anyhow!("bad destination"))?;
    let mut cur = root.to_path_buf();
    for c in parent.strip_prefix(root).unwrap_or(parent).components() {
        cur.push(c);
        if !cur.exists() {
            std::fs::create_dir(&cur)?;
            set_mode(&cur, 0o775);
        }
    }
    let part = dest.with_extension("part");
    std::fs::copy(src, &part)?;
    set_mode(&part, 0o664);
    std::fs::rename(&part, dest)?;
    Ok(())
}

/// Mime type for cover bytes, or `None` for formats players cannot embed (e.g. WebP).
pub fn image_mime(bytes: &[u8]) -> Option<MimeType> {
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some(MimeType::Jpeg)
    } else if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        Some(MimeType::Png)
    } else {
        None
    }
}

pub fn write_tags(path: &Path, artist: &str, album_artist: &str, title: &str, album: Option<&str>, cover: Option<Vec<u8>>) -> Result<()> {
    let mut tagged = lofty::read_from_path(path)?;
    if tagged.primary_tag().is_none() {
        let kind = tagged.primary_tag_type();
        tagged.insert_tag(Tag::new(kind));
    }
    let tag = tagged.primary_tag_mut().ok_or_else(|| anyhow!("file format has no tag support"))?;
    tag.set_artist(artist.to_string());
    tag.set_title(title.to_string());
    if let Some(a) = album.filter(|a| !a.trim().is_empty()) {
        tag.set_album(a.to_string());
    }
    tag.insert_text(ItemKey::AlbumArtist, album_artist.to_string());
    if let Some(bytes) = cover
        && let Some(mime) = image_mime(&bytes)
    {
        tag.push_picture(Picture::unchecked(bytes).pic_type(PictureType::CoverFront).mime_type(mime).build());
    }
    tagged.save_to_path(path, WriteOptions::default())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::LibEntry;

    #[test]
    fn target_path_with_and_without_album() {
        let root = Path::new("/music");
        assert_eq!(
            target_path(root, "The Sequence", Some("In Lights"), "Disturbia", "m4a"),
            Path::new("/music/The Sequence/In Lights/Disturbia.m4a")
        );
        assert_eq!(target_path(root, "AC/DC", None, "Back: In Black", "m4a"), Path::new("/music/Non-Album/AC_DC/Back_ In Black.m4a"));
        assert_eq!(target_path(root, "X", Some("  "), "Y", "opus"), Path::new("/music/Non-Album/X/Y.opus"));
    }

    #[test]
    fn names_come_from_the_path_when_untagged() {
        let root = Path::new("/music");
        let p = Path::new("/music/Non-Album/Viza/Eros.flac");
        assert_eq!(names_from_path(p, root), ("Viza".to_string(), "Eros".to_string()));
        let p = Path::new("/music/Gorillaz/Demon Days/Feel Good Inc.mp3");
        assert_eq!(names_from_path(p, root), ("Gorillaz".to_string(), "Feel Good Inc".to_string()));
        assert_eq!(names_from_path(Path::new("/music/loose.mp3"), root).0, "");
    }

    #[test]
    fn find_matches_title_artist_and_length() {
        let db = Db::memory().unwrap();
        let add = |path: &str, artist: &str, d: f64| {
            db.lib_upsert(&LibEntry {
                path: path.into(),
                mtime: 1,
                artist: artist.into(),
                title: "Feel Good Inc.".into(),
                tn: norm("Feel Good Inc."),
                duration: Some(d),
            })
            .unwrap();
        };
        add("/m/a.mp3", "Gorillaz", 222.0);
        add("/m/b.flac", "Gorillaz", 223.0);
        add("/m/c.mp3", "Someone Else", 223.0);
        assert_eq!(find(&db, "Gorillaz", "Feel Good Inc", Some(223.4)).as_deref(), Some("/m/b.flac"));
        assert_eq!(find(&db, "Gorillaz", "Feel Good Inc", Some(240.0)), None);
        assert_eq!(find(&db, "Gorillaz", "Feel Good Inc", None).as_deref(), Some("/m/b.flac"));
        assert_eq!(find(&db, "", "Feel Good Inc", None), None);
    }

    #[test]
    fn tags_cover_and_placement_work_on_a_real_file() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("in.m4a");
        std::fs::write(&src, include_bytes!("../tests/fixtures/silent.m4a")).unwrap();
        let cover = include_bytes!("../tests/fixtures/cover.jpg").to_vec();
        write_tags(&src, "Artist A, Artist B", "Artist A", "A Title", Some("An Album"), Some(cover)).unwrap();

        let root = dir.path().join("lib");
        std::fs::create_dir(&root).unwrap();
        let dest = target_path(&root, "Artist A", Some("An Album"), "A Title", "m4a");
        place(&src, &dest, &root).unwrap();
        assert!(dest.exists() && !dest.with_extension("part").exists());

        let e = read_entry(&dest, &root).unwrap();
        assert_eq!((e.artist.as_str(), e.title.as_str()), ("Artist A, Artist B", "A Title"));
        assert!((e.duration.unwrap() - 2.0).abs() < 0.3);
        let tagged = lofty::read_from_path(&dest).unwrap();
        let tag = tagged.primary_tag().unwrap();
        assert_eq!(tag.album().as_deref(), Some("An Album"));
        assert_eq!(tag.picture_count(), 1);

        // The scan indexes it, finds it again by name and length, and a rescan changes nothing.
        let db = Db::memory().unwrap();
        assert_eq!(scan(&db, &root).unwrap(), (1, 0));
        assert_eq!(find(&db, "Artist A", "A Title", Some(2.0)).as_deref(), dest.to_str());
        assert_eq!(scan(&db, &root).unwrap(), (1, 0));
        std::fs::remove_file(&dest).unwrap();
        assert_eq!(scan(&db, &root).unwrap(), (0, 1));
    }

    #[test]
    fn only_jpeg_and_png_are_embeddable() {
        assert!(image_mime(&[0xFF, 0xD8, 0xFF, 0xE0]).is_some());
        assert!(image_mime(&[0x89, b'P', b'N', b'G', 0]).is_some());
        assert!(image_mime(b"RIFF....WEBP").is_none());
    }
}
