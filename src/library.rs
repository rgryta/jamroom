//! The music library: indexing, lookup, file placement and tagging.

use crate::{
    db::{Db, LibEntry},
    norm::{OTHER_VERSIONS, artist_match, norm, sanitize},
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

pub fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
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

/// The tags a file already carries.
#[derive(Debug, Default, Clone)]
pub struct FileTags {
    pub artist: Option<String>,
    pub title: Option<String>,
    pub album: Option<String>,
    pub album_artist: Option<String>,
}

pub fn read_tags(path: &Path) -> FileTags {
    let Ok(tagged) = lofty::read_from_path(path) else { return FileTags::default() };
    let Some(tag) = tagged.primary_tag().or_else(|| tagged.first_tag()) else { return FileTags::default() };
    let text = |v: Option<&str>| v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    FileTags {
        artist: text(tag.artist().as_deref()),
        title: text(tag.title().as_deref()),
        album: text(tag.album().as_deref()),
        album_artist: text(tag.get_string(ItemKey::AlbumArtist)),
    }
}

/// Remove the album tag, for songs that only came off a compilation.
pub fn clear_album(path: &Path) -> Result<()> {
    let mut tagged = lofty::read_from_path(path)?;
    if let Some(tag) = tagged.primary_tag_mut() {
        tag.remove_album();
    }
    tagged.save_to_path(path, WriteOptions::default())?;
    Ok(())
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
pub fn scan(db: &Db, roots: &[&Path]) -> Result<(usize, usize)> {
    let known = db.lib_known()?;
    let mut files: Vec<(PathBuf, &Path)> = Vec::new();
    for root in roots {
        let mut found = Vec::new();
        walk(root, &mut found);
        files.extend(found.into_iter().map(|p| (p, *root)));
    }
    let seen: HashSet<String> = files.iter().map(|(p, _)| p.to_string_lossy().into_owned()).collect();
    let mut batch = Vec::new();
    for (p, root) in &files {
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

/// A file that already holds this track: same title, matching artist, length within 3 s. A file
/// outside the cache wins over a cached download, and FLAC wins over the rest.
pub fn find(db: &Db, cache: &Path, artist: &str, title: &str, duration: Option<f64>) -> Option<String> {
    let tn = norm(title);
    if tn.is_empty() || artist.trim().is_empty() {
        return None;
    }
    let mut hits: Vec<String> = db
        .lib_by_title(&tn)
        .ok()?
        .into_iter()
        .filter(|(_, a, d, _)| {
            artist_match(a, artist)
                && match (duration, d) {
                    (Some(want), Some(have)) => (want - have).abs() <= 3.0,
                    _ => true,
                }
        })
        .map(|h| h.0)
        .collect();
    hits.sort_by_key(|p| (Path::new(p).starts_with(cache), !p.ends_with(".flac")));
    hits.into_iter().next()
}

/// The same song in your own library under another length (a video cut, a radio edit, a different
/// master): still the better file to play than a fresh YouTube download. The length may differ by
/// up to 30 s, and a copy whose version words differ ("live", "remix", "acoustic", ...) is refused,
/// because matching ignores bracket text and would otherwise swap a live song for the studio one.
pub fn find_loose(db: &Db, cache: &Path, artist: &str, title: &str, duration: Option<f64>) -> Option<String> {
    let tn = norm(title);
    if tn.is_empty() || artist.trim().is_empty() {
        return None;
    }
    let wanted = title.to_lowercase();
    let mut hits: Vec<(f64, String)> = db
        .lib_by_title(&tn)
        .ok()?
        .into_iter()
        .filter(|(p, a, _, _)| !Path::new(p).starts_with(cache) && artist_match(a, artist))
        .filter(|(_, _, _, t)| {
            let have = t.to_lowercase();
            OTHER_VERSIONS.iter().all(|v| wanted.contains(v) == have.contains(v))
        })
        .filter_map(|(p, _, d, _)| match (duration, d) {
            (Some(w), Some(h)) if (w - h).abs() > 30.0 => None,
            (Some(w), Some(h)) => Some(((w - h).abs(), p)),
            _ => Some((0.0, p)),
        })
        .collect();
    hits.sort_by(|a, b| (!a.1.ends_with(".flac")).cmp(&!b.1.ends_with(".flac")).then(a.0.total_cmp(&b.0)));
    hits.into_iter().next().map(|h| h.1)
}

/// The best file to play for a song, in order: your library at the same length, your library at
/// another length, then the cache. Only when none exists is the song downloaded.
pub fn best(db: &Db, cache: &Path, artist: &str, title: &str, duration: Option<f64>) -> Option<String> {
    let exact = find(db, cache, artist, title, duration);
    match &exact {
        Some(p) if !Path::new(p).starts_with(cache) => exact,
        _ => find_loose(db, cache, artist, title, duration).or(exact),
    }
}

/// A song you already own: same title and artist outside the cache, whatever its length.
pub fn same_song(db: &Db, cache: &Path, artist: &str, title: &str) -> Option<String> {
    let tn = norm(title);
    if tn.is_empty() || artist.trim().is_empty() {
        return None;
    }
    db.lib_by_title(&tn).ok()?.into_iter().find(|(p, a, _, _)| !Path::new(p).starts_with(cache) && artist_match(a, artist)).map(|h| h.0)
}

/// Where a newly cached track goes: `<Artist>/<Album>/<Title>.ext`, or `Non-Album/<Artist>/<Title>.ext`.
pub fn target_path(root: &Path, dir_artist: &str, album: Option<&str>, title: &str, ext: &str) -> PathBuf {
    let file = format!("{}.{}", sanitize(title), ext);
    match album.map(str::trim).filter(|a| !a.is_empty()) {
        Some(al) => root.join(sanitize(dir_artist)).join(sanitize(al)).join(file),
        None => root.join("Non-Album").join(sanitize(dir_artist)).join(file),
    }
}

/// Delete cached songs older than `days` that no waiting song needs, and the folders they leave empty.
pub fn prune(db: &Db, cache: &Path, days: i64) -> Result<usize> {
    let in_use = db.open_files()?;
    let cutoff = std::time::SystemTime::now() - std::time::Duration::from_secs(days.max(1) as u64 * 86_400);
    let mut files = Vec::new();
    walk(cache, &mut files);
    let mut gone = 0;
    for p in files {
        let old = std::fs::metadata(&p).and_then(|m| m.modified()).is_ok_and(|t| t < cutoff);
        if old && !in_use.contains(p.to_string_lossy().as_ref()) && std::fs::remove_file(&p).is_ok() {
            gone += 1;
            let mut dir = p.parent();
            while let Some(d) = dir.filter(|d| *d != cache && std::fs::remove_dir(d).is_ok()) {
                dir = d.parent();
            }
        }
    }
    Ok(gone)
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
        match std::fs::create_dir(&cur) {
            Ok(()) => set_mode(&cur, 0o775),
            // Another job may have made it a moment ago.
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e.into()),
        }
    }
    // A name of its own per call: two jobs finishing the same song must not share one temp file.
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let part = dest.with_extension(format!("{}.part", NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
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
        let cache = Path::new("/cache");
        assert_eq!(find(&db, cache, "Gorillaz", "Feel Good Inc", Some(223.4)).as_deref(), Some("/m/b.flac"));
        assert_eq!(find(&db, cache, "Gorillaz", "Feel Good Inc", Some(240.0)), None);
        assert_eq!(find(&db, cache, "Gorillaz", "Feel Good Inc", None).as_deref(), Some("/m/b.flac"));
        assert_eq!(find(&db, cache, "", "Feel Good Inc", None), None);
        // A cached download loses to a file outside the cache, even a lossy one.
        add("/cache/Gorillaz/c.flac", "Gorillaz", 223.0);
        assert_eq!(find(&db, cache, "Gorillaz", "Feel Good Inc", None).as_deref(), Some("/m/b.flac"));
        assert_eq!(find(&db, Path::new("/m"), "Gorillaz", "Feel Good Inc", None).as_deref(), Some("/cache/Gorillaz/c.flac"));
    }

    #[test]
    fn your_own_copy_is_preferred_even_at_another_length_but_not_another_version() {
        let db = Db::memory().unwrap();
        let add = |path: &str, title: &str, d: f64| {
            db.lib_upsert(&LibEntry {
                path: path.into(),
                mtime: 1,
                artist: "OneRepublic".into(),
                title: title.into(),
                tn: norm(title),
                duration: Some(d),
            })
            .unwrap()
        };
        let cache = Path::new("/cache");
        add("/cache/OneRepublic/Counting Stars.m4a", "Counting Stars", 283.0);
        add("/m/Native/01 Counting Stars.mp3", "Counting Stars", 257.0);
        // The video cut is 26 s longer than your album copy: your copy is still played, not the download.
        assert_eq!(
            best(&db, cache, "OneRepublic", "Counting Stars (Official Video)", Some(283.0)).as_deref(),
            Some("/m/Native/01 Counting Stars.mp3")
        );
        // The same length as your copy: no change.
        assert_eq!(best(&db, cache, "OneRepublic", "Counting Stars", Some(257.0)).as_deref(), Some("/m/Native/01 Counting Stars.mp3"));
        // A live version is not the studio one, so it falls back to what matches by length (the cache).
        assert_eq!(
            best(&db, cache, "OneRepublic", "Counting Stars (Live)", Some(283.0)).as_deref(),
            Some("/cache/OneRepublic/Counting Stars.m4a")
        );
        // Too different in length to be the same recording: nothing matches, so it would be downloaded.
        assert_eq!(best(&db, cache, "OneRepublic", "Counting Stars", Some(330.0)), None);
        // Another artist's song with the same title is never swapped in.
        assert_eq!(best(&db, cache, "Someone Else", "Counting Stars", Some(257.0)), None);
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
        assert!(dest.exists() && !dest.with_extension("0.part").exists());

        let e = read_entry(&dest, &root).unwrap();
        assert_eq!((e.artist.as_str(), e.title.as_str()), ("Artist A, Artist B", "A Title"));
        assert!((e.duration.unwrap() - 2.0).abs() < 0.3);
        let tagged = lofty::read_from_path(&dest).unwrap();
        let tag = tagged.primary_tag().unwrap();
        assert_eq!(tag.album().as_deref(), Some("An Album"));
        assert_eq!(tag.picture_count(), 1);

        // The scan indexes it, finds it again by name and length, and a rescan changes nothing.
        let db = Db::memory().unwrap();
        assert_eq!(scan(&db, &[&root]).unwrap(), (1, 0));
        assert_eq!(find(&db, Path::new("/cache"), "Artist A", "A Title", Some(2.0)).as_deref(), dest.to_str());
        assert_eq!(scan(&db, &[&root]).unwrap(), (1, 0));
        std::fs::remove_file(&dest).unwrap();
        assert_eq!(scan(&db, &[&root]).unwrap(), (0, 1));
    }

    #[test]
    fn old_cached_songs_are_pruned_unless_someone_is_waiting_for_them() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("songs");
        let touch = |rel: &str, days_old: u64| {
            let p = cache.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            let f = std::fs::File::create(&p).unwrap();
            f.set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(days_old * 86_400)).unwrap();
            p
        };
        let stale = touch("Old/Album/stale.m4a", 90);
        let queued = touch("Old/Album/queued.m4a", 90);
        let fresh = touch("New/fresh.m4a", 1);
        let db = Db::memory().unwrap();
        let id = db
            .add_track("r", &crate::db::NewTrack { artist: "A".into(), dir_artist: "A".into(), title: "T".into(), ..Default::default() })
            .unwrap();
        db.next_job().unwrap();
        db.set_ready(id, queued.to_str().unwrap(), true).unwrap();
        assert_eq!(prune(&db, &cache, 60).unwrap(), 1);
        assert!(!stale.exists() && queued.exists() && fresh.exists());
        // Once nothing waits for it, the old file goes too, and its emptied folders with it.
        db.finish_track("r", id).unwrap();
        db.remove("r", id).unwrap();
        assert_eq!(prune(&db, &cache, 60).unwrap(), 1);
        assert!(!cache.join("Old").exists() && cache.exists());
    }

    #[test]
    fn only_jpeg_and_png_are_embeddable() {
        assert!(image_mime(&[0xFF, 0xD8, 0xFF, 0xE0]).is_some());
        assert!(image_mime(&[0x89, b'P', b'N', b'G', 0]).is_some());
        assert!(image_mime(b"RIFF....WEBP").is_none());
    }
}
