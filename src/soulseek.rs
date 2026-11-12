//! Background upgrade: for each song that only exists as a YouTube download in the cache, look for a
//! lossless (or 256k+ MP3) copy on Soulseek through slskd and file that one in the music library.
//!
//! Runs as its own process (`jamroom upgrade`), so the public web service never talks to slskd and
//! never writes to the library. Only a close match is accepted: the title and artist must be in the
//! file name and the length must be within 3 seconds. Anything else stays in the cache.

use crate::{
    config::Config,
    db::{Db, Upgrade, now},
    library,
    norm::norm,
    resolve::first_artist,
};
use anyhow::{Result, anyhow, bail};
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};
use url::Url;

/// Words that mark another version of a song. A file with one is skipped unless the title has it too.
const OTHER_VERSIONS: &[&str] = &[
    "live",
    "remix",
    "instrumental",
    "karaoke",
    "cover",
    "acoustic",
    "extended",
    "sped",
    "slowed",
    "nightcore",
    "demo",
    "radio edit",
    "8d",
    "reverb",
    "lofi",
    "tribute",
    "medley",
    "mashup",
    "bootleg",
    "version",
];
/// Look again after two weeks for songs nobody shares, after an hour for failures.
const RETRY_NONE: i64 = 14 * 86_400;
const RETRY_FAILED: i64 = 3_600;
/// A download still queued at the other end after this long is cancelled.
const GIVE_UP: i64 = 6 * 3_600;
const MAX_WAITING: usize = 2;

pub fn upgrade_key(artist: &str, title: &str) -> String {
    format!("{}|{}", norm(artist), norm(title))
}

/// What we are looking for.
#[derive(Debug, Clone)]
pub struct Want {
    pub artist: String,
    pub title: String,
    pub duration: Option<f64>,
}

/// The file chosen on someone's share.
#[derive(Debug, Clone, PartialEq)]
pub struct Pick {
    pub user: String,
    pub file: String,
    pub size: i64,
}

/// The search text: the first artist and the title without brackets or "- remaster" style tails.
pub fn clean_query(artist: &str, title: &str) -> String {
    let mut plain = String::new();
    let mut depth = 0u32;
    for ch in title.chars() {
        match ch {
            '(' | '[' => depth += 1,
            ')' | ']' => depth = depth.saturating_sub(1),
            _ if depth == 0 => plain.push(ch),
            _ => {}
        }
    }
    let plain = plain.split(" - ").next().unwrap_or("").split(" | ").next().unwrap_or("");
    let text: String = format!("{} {}", first_artist(artist), plain).chars().map(|c| if c.is_alphanumeric() { c } else { ' ' }).collect();
    let mut out = String::new();
    for word in text.split_whitespace() {
        if out.chars().count() + word.chars().count() + 1 > 70 {
            break;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
    }
    out
}

/// "07 - Song" -> "Song". Only strips when the name starts with a track number.
fn strip_track_number(s: &str) -> &str {
    let t = s.trim_start();
    let rest = t.trim_start_matches(|c: char| c.is_ascii_digit());
    if rest.len() == t.len() { s } else { rest.trim_start_matches(|c: char| c.is_whitespace() || ".-_".contains(c)) }
}

/// How good a shared file is for this song, or `None` if it is not an acceptable copy.
pub fn score(want: &Want, free_slot: bool, queue: i64, file: &Value) -> Option<f64> {
    if file.get("isLocked").and_then(Value::as_bool).unwrap_or(false) {
        return None;
    }
    let name = file.get("filename")?.as_str()?.replace('\\', "/");
    let low = name.to_lowercase();
    let bitrate = file.get("bitRate").and_then(Value::as_i64).unwrap_or(0);
    let quality = match low.rsplit('.').next()? {
        "flac" => 25.0,
        "mp3" if bitrate >= 320 => 14.0,
        "mp3" if bitrate >= 256 => 6.0,
        _ => return None,
    };
    let leaf = name.rsplit('/').next()?;
    let stem = leaf.rsplit_once('.').map_or(leaf, |(s, _)| s);
    let (stem, title, artist) = (norm(strip_track_number(stem)), norm(&want.title), norm(&want.artist));
    if title.is_empty() || !stem.contains(&title) || stem.chars().count() > title.chars().count() + artist.chars().count() + 6 {
        return None;
    }
    let whole = norm(&name);
    if !artist.is_empty() && !whole.contains(&artist) && !whole.contains(&artist.chars().take(6).collect::<String>()) {
        return None;
    }
    let (leaf_low, wanted_low) = (leaf.to_lowercase(), want.title.to_lowercase());
    if OTHER_VERSIONS.iter().any(|b| leaf_low.contains(b) && !wanted_low.contains(b)) {
        return None;
    }
    let (have, wanted) = (file.get("length").and_then(Value::as_f64)?, want.duration?);
    if (have - wanted).abs() > 3.0 {
        return None;
    }
    let queue_penalty = queue.clamp(0, 100) as f64 / 10.0;
    Some(60.0 + quality + if free_slot { 10.0 } else { 0.0 } - queue_penalty)
}

/// The best acceptable file among all search responses.
pub fn pick(want: &Want, responses: &Value) -> Option<Pick> {
    let mut best: Option<(f64, Pick)> = None;
    for peer in responses.as_array()? {
        let (Some(user), free, queue) = (
            peer.get("username").and_then(Value::as_str),
            peer.get("hasFreeUploadSlot").and_then(Value::as_bool).unwrap_or(false),
            peer.get("queueLength").and_then(Value::as_i64).unwrap_or(0),
        ) else {
            continue;
        };
        for f in peer.get("files").and_then(Value::as_array).into_iter().flatten() {
            let Some(s) = score(want, free, queue, f) else { continue };
            if best.as_ref().is_none_or(|(b, _)| s > *b) {
                let pick = Pick {
                    user: user.to_string(),
                    file: f["filename"].as_str().unwrap_or_default().to_string(),
                    size: f.get("size").and_then(Value::as_i64).unwrap_or(0),
                };
                best = Some((s, pick));
            }
        }
    }
    best.map(|(_, p)| p)
}

/// A path segment safe to put in a URL.
fn segment(s: &str) -> String {
    let mut u = Url::parse("http://x/").expect("static url");
    if let Ok(mut p) = u.path_segments_mut() {
        p.push(s);
    }
    u.path().trim_start_matches('/').to_string()
}

/// The slskd HTTP API.
pub struct Slskd {
    http: reqwest::Client,
    base: String,
}

impl Slskd {
    pub fn new(base: &str) -> Result<Self> {
        let http = reqwest::Client::builder().timeout(Duration::from_secs(120)).build()?;
        Ok(Slskd { http, base: format!("{}/api/v0", base.trim_end_matches('/')) })
    }

    async fn call(&self, method: Method, path: &str, body: Option<Value>) -> Result<Value> {
        for attempt in 0..6u64 {
            let mut req = self.http.request(method.clone(), format!("{}{path}", self.base));
            if let Some(b) = &body {
                req = req.json(b);
            }
            let resp = req.send().await?;
            if resp.status() == StatusCode::TOO_MANY_REQUESTS && attempt < 5 {
                tokio::time::sleep(Duration::from_secs(15 * (attempt + 1))).await;
                continue;
            }
            let text = resp.error_for_status()?.text().await?;
            return Ok(if text.trim().is_empty() { Value::Null } else { serde_json::from_str(&text)? });
        }
        bail!("slskd kept answering too many requests")
    }

    /// Run one search and return the peers' responses.
    pub async fn search(&self, text: &str) -> Result<Value> {
        let body = json!({ "searchText": text, "searchTimeout": 15000, "responseLimit": 150, "fileLimit": 1500 });
        let id = self.call(Method::POST, "/searches", Some(body)).await?["id"]
            .as_str()
            .ok_or_else(|| anyhow!("slskd gave no search id"))?
            .to_string();
        for _ in 0..30 {
            if self.call(Method::GET, &format!("/searches/{id}"), None).await?["isComplete"].as_bool() == Some(true) {
                break;
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        let found = self.call(Method::GET, &format!("/searches/{id}?includeResponses=true"), None).await?;
        let _ = self.call(Method::DELETE, &format!("/searches/{id}"), None).await;
        Ok(found["responses"].clone())
    }

    pub async fn enqueue(&self, pick: &Pick) -> Result<()> {
        let body = json!([{ "filename": pick.file, "size": pick.size }]);
        self.call(Method::POST, &format!("/transfers/downloads/{}", segment(&pick.user)), Some(body)).await?;
        Ok(())
    }

    /// The state of one download ("Completed, Succeeded", "Queued, Remotely", ...) and its id.
    pub async fn transfer(&self, user: &str, remote: &str) -> Result<Option<(String, String)>> {
        let v = self.call(Method::GET, &format!("/transfers/downloads/{}", segment(user)), None).await?;
        for dir in v.get("directories").and_then(Value::as_array).into_iter().flatten() {
            for f in dir.get("files").and_then(Value::as_array).into_iter().flatten() {
                if f.get("filename").and_then(Value::as_str) == Some(remote) {
                    let state = f.get("state").and_then(Value::as_str).unwrap_or_default().to_string();
                    return Ok(Some((state, f.get("id").and_then(Value::as_str).unwrap_or_default().to_string())));
                }
            }
        }
        Ok(None)
    }

    pub async fn cancel(&self, user: &str, id: &str) {
        let _ = self.call(Method::DELETE, &format!("/transfers/downloads/{}/{}?remove=true", segment(user), segment(id)), None).await;
    }
}

/// Where slskd put a finished download: `<complete>/<folder name>/<file>`, or anywhere under it.
pub fn locate(downloads: &Path, remote: &str) -> Option<PathBuf> {
    let parts: Vec<&str> = remote.split(['\\', '/']).collect();
    let leaf = *parts.last()?;
    if parts.len() > 1 {
        let direct = downloads.join(parts[parts.len() - 2]).join(leaf);
        if direct.is_file() {
            return Some(direct);
        }
    }
    let mut all = Vec::new();
    library::walk(downloads, &mut all);
    all.into_iter().find(|p| p.file_name().is_some_and(|n| n == leaf))
}

/// Tag a copy of the downloaded file and put it in the library. Blocking.
fn import(cfg: &Config, up: &Upgrade) -> Result<PathBuf> {
    let src = locate(&cfg.slsk_downloads, &up.remote).ok_or_else(|| anyhow!("the downloaded file is missing"))?;
    let have = library::probe_duration(&src).ok_or_else(|| anyhow!("the downloaded file cannot be read"))?;
    if let Some(want) = up.duration
        && (want - have).abs() > 4.0
    {
        bail!("the downloaded file has the wrong length");
    }
    let tmp = cfg.data.join("upgrade");
    std::fs::create_dir_all(&tmp)?;
    let copy = tmp.join(src.file_name().ok_or_else(|| anyhow!("no file name"))?);
    let result = (|| {
        std::fs::copy(&src, &copy)?;
        let album = library::read_album(&copy);
        library::write_tags(&copy, &up.artist, &first_artist(&up.artist), &up.title, album.as_deref(), None)?;
        let ext = copy.extension().and_then(|e| e.to_str()).unwrap_or("flac").to_ascii_lowercase();
        let dest = library::target_path(&cfg.library, &first_artist(&up.artist), album.as_deref(), &up.title, &ext);
        if !dest.exists() {
            library::place(&copy, &dest, &cfg.library)?;
        }
        Ok(dest)
    })();
    let _ = std::fs::remove_file(&copy);
    result
}

fn settle(db: &Db, up: &Upgrade, status: &str, note: &str) -> Result<()> {
    db.upgrade_put(&Upgrade { status: status.into(), note: note.into(), updated_at: now(), ..up.clone() })
}

/// Check a download that was queued earlier.
async fn follow(cfg: &Config, db: &Db, sl: &Slskd, up: &Upgrade) -> Result<()> {
    match sl.transfer(&up.peer, &up.remote).await? {
        Some((state, _)) if state == "Completed, Succeeded" => {
            let (cfg2, up2) = (cfg.clone(), up.clone());
            match tokio::task::spawn_blocking(move || import(&cfg2, &up2)).await? {
                Ok(dest) => {
                    if let Some(e) = library::read_entry(&dest, &cfg.library) {
                        db.lib_upsert(&e)?;
                    }
                    let n = db.apply_upgrade(&up.cached, &dest.to_string_lossy())?;
                    tracing::info!(path = %dest.display(), queued_songs = n, "upgraded from Soulseek");
                    settle(db, up, "done", &format!("from {}", up.peer))
                }
                Err(e) => {
                    tracing::warn!(song = %up.title, error = %e, "could not use the Soulseek download");
                    settle(db, up, "failed", &e.to_string())
                }
            }
        }
        Some((state, _)) if state.starts_with("Completed") => settle(db, up, "failed", &state),
        Some((_, id)) if now() - up.updated_at > GIVE_UP => {
            sl.cancel(&up.peer, &id).await;
            settle(db, up, "none", "gave up waiting for the other side")
        }
        None if now() - up.updated_at > 600 => settle(db, up, "failed", "slskd lost the download"),
        _ => Ok(()),
    }
}

/// Search for one song and queue the best copy.
async fn search_one(db: &Db, sl: &Slskd, cfg: &Config, fresh: Upgrade) -> Result<()> {
    if library::find(db, &cfg.cache, &fresh.artist, &fresh.title, fresh.duration).is_some_and(|p| !Path::new(&p).starts_with(&cfg.cache)) {
        return settle(db, &fresh, "done", "already in the library");
    }
    let want = Want { artist: fresh.artist.clone(), title: fresh.title.clone(), duration: fresh.duration };
    let mut queries = vec![clean_query(&fresh.artist, &fresh.title)];
    let title_only = clean_query("", &fresh.title);
    if title_only.split_whitespace().count() >= 2 && !queries.contains(&title_only) {
        queries.push(title_only);
    }
    for q in queries {
        let found = match sl.search(&q).await {
            Ok(r) => r,
            Err(e) => return settle(db, &fresh, "failed", &format!("search failed: {e}")),
        };
        if let Some(p) = pick(&want, &found) {
            return match sl.enqueue(&p).await {
                Ok(()) => {
                    tracing::info!(song = %fresh.title, peer = %p.user, "asked for a copy on Soulseek");
                    db.upgrade_put(&Upgrade {
                        status: "downloading".into(),
                        peer: p.user,
                        remote: p.file,
                        size: p.size,
                        updated_at: now(),
                        ..fresh
                    })
                }
                Err(e) => settle(db, &fresh, "failed", &format!("could not queue: {e}")),
            };
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    settle(db, &fresh, "none", "no good copy shared")
}

/// One round: follow the downloads in flight, then start looking for one more song.
pub async fn tick(cfg: &Config, db: &Db, sl: &Slskd) -> Result<()> {
    let known = db.upgrades()?;
    let waiting: Vec<&Upgrade> = known.values().filter(|u| u.status == "downloading").collect();
    for up in &waiting {
        if let Err(e) = follow(cfg, db, sl, up).await {
            tracing::warn!(song = %up.title, error = %e, "could not check a download");
        }
    }
    if waiting.len() >= MAX_WAITING {
        return Ok(());
    }
    let prefix = format!("{}/", cfg.cache.display());
    for (artist, title, duration, cached) in db.cached_songs(&prefix)? {
        let key = upgrade_key(&artist, &title);
        if key.starts_with('|') || key.ends_with('|') {
            continue;
        }
        if let Some(u) = known.get(&key) {
            let retry = match u.status.as_str() {
                "none" => RETRY_NONE,
                "failed" => RETRY_FAILED,
                _ => i64::MAX,
            };
            if now() - u.updated_at < retry {
                continue;
            }
        }
        let fresh = Upgrade { key, artist, title, duration, cached, status: "searching".into(), updated_at: now(), ..Default::default() };
        return search_one(db, sl, cfg, fresh).await;
    }
    Ok(())
}

/// Run until the process ends: one round every half minute, so Soulseek sees a gentle trickle.
pub async fn run(cfg: Config, db: Db) -> Result<()> {
    let sl = Slskd::new(&cfg.slskd_url)?;
    tracing::info!(slskd = %cfg.slskd_url, cache = %cfg.cache.display(), library = %cfg.library.display(), "Soulseek upgrader is up");
    loop {
        if let Err(e) = tick(&cfg, &db, &sl).await {
            tracing::warn!(error = %e, "Soulseek round failed");
        }
        tokio::time::sleep(Duration::from_secs(30)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn want() -> Want {
        Want { artist: "OneRepublic".into(), title: "Counting Stars".into(), duration: Some(257.0) }
    }

    fn file(name: &str, length: f64, bitrate: i64) -> Value {
        json!({ "filename": name, "size": 30_000_000, "length": length, "bitRate": bitrate, "isLocked": false })
    }

    #[test]
    fn a_close_lossless_match_is_accepted_and_odd_ones_are_not() {
        let w = want();
        let good = r"\\Music\OneRepublic\Native (2013)\07 - Counting Stars.flac";
        assert!(score(&w, true, 0, &file(good, 257.0, 0)).is_some());
        assert!(score(&w, true, 0, &file(good, 262.0, 0)).is_none(), "wrong length");
        assert!(score(&w, true, 0, &file(r"\\x\OneRepublic\Counting Stars (Remix).flac", 257.0, 0)).is_none(), "remix");
        assert!(score(&w, true, 0, &file(r"\\x\OneRepublic\Counting Stars.mp3", 257.0, 128)).is_none(), "low bitrate");
        assert!(score(&w, true, 0, &file(r"\\x\Someone Else\Counting Stars.flac", 257.0, 0)).is_none(), "other artist");
        assert!(
            score(&w, true, 0, &file(r"\\x\OneRepublic\Counting Stars And Many More Songs From The Album.flac", 257.0, 0)).is_none(),
            "longer title"
        );
        assert!(score(&w, true, 0, &json!({ "filename": good, "length": 257.0, "isLocked": true })).is_none(), "locked");
        let unknown_length = Want { duration: None, ..w };
        assert!(score(&unknown_length, true, 0, &file(good, 257.0, 0)).is_none(), "a song of unknown length is never matched");
    }

    #[test]
    fn lossless_beats_320_and_a_free_slot_beats_a_queue() {
        let w = want();
        let flac = score(&w, false, 0, &file(r"\\a\OneRepublic\Counting Stars.flac", 257.0, 0)).unwrap();
        let mp3 = score(&w, true, 0, &file(r"\\a\OneRepublic\Counting Stars.mp3", 257.0, 320)).unwrap();
        assert!(flac > mp3);
        let near = score(&w, true, 0, &file(r"\\a\OneRepublic\Counting Stars.flac", 257.0, 0)).unwrap();
        let far = score(&w, false, 80, &file(r"\\a\OneRepublic\Counting Stars.flac", 257.0, 0)).unwrap();
        assert!(near > far);
    }

    #[test]
    fn the_best_file_across_peers_is_picked() {
        let responses = json!([
            { "username": "slow", "hasFreeUploadSlot": false, "queueLength": 90, "files": [file(r"\\a\OneRepublic\Counting Stars.flac", 257.0, 0)] },
            { "username": "fast", "hasFreeUploadSlot": true, "queueLength": 0, "files": [file(r"\\b\OneRepublic\Native\07 Counting Stars.flac", 257.0, 0)] },
            { "username": "wrong", "hasFreeUploadSlot": true, "queueLength": 0, "files": [file(r"\\c\OneRepublic\Other Song.flac", 257.0, 0)] },
        ]);
        let p = pick(&want(), &responses).unwrap();
        assert_eq!((p.user.as_str(), p.size), ("fast", 30_000_000));
        assert!(pick(&want(), &json!([])).is_none());
    }

    #[test]
    fn search_text_drops_brackets_tails_and_symbols() {
        assert_eq!(
            clean_query("Timbaland, Nelly Furtado", "Morning After Dark (Official Music Video) ft. Nelly"),
            "Timbaland Morning After Dark ft Nelly"
        );
        assert_eq!(clean_query("A", "Song - 2011 Remaster"), "A Song");
        assert_eq!(clean_query("A", "Espresso Macchiato (LIVE) | Estonia | Eurovision"), "A Espresso Macchiato");
        assert!(clean_query("A", &"word ".repeat(40)).chars().count() <= 70);
    }

    #[test]
    fn a_finished_download_is_found_where_slskd_puts_it() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("Native (2013)");
        std::fs::create_dir_all(&folder).unwrap();
        let file = folder.join("07 - Counting Stars.flac");
        std::fs::write(&file, b"x").unwrap();
        assert_eq!(locate(dir.path(), r"\\Music\OneRepublic\Native (2013)\07 - Counting Stars.flac"), Some(file.clone()));
        // The folder name can differ; the file name still finds it.
        assert_eq!(locate(dir.path(), r"\\Music\Elsewhere\07 - Counting Stars.flac"), Some(file));
        assert_eq!(locate(dir.path(), r"\\Music\Nothing\gone.flac"), None);
    }

    /// A stand-in for slskd that offers one FLAC and reports it as finished.
    async fn fake_slskd(remote: &str) -> String {
        use axum::{Json, Router, http::StatusCode, routing::get, routing::post};
        let found = json!({
            "id": "s1", "isComplete": true,
            "responses": [{ "username": "bob", "hasFreeUploadSlot": true, "queueLength": 0,
                "files": [{ "filename": remote, "size": 8508, "length": 2, "bitRate": 0, "isLocked": false }] }]
        });
        let transfers = json!({ "username": "bob", "directories": [{ "directory": "x",
            "files": [{ "id": "t1", "filename": remote, "state": "Completed, Succeeded" }] }] });
        let app = Router::new()
            .route("/api/v0/searches", post(|| async { Json(json!({ "id": "s1" })) }))
            .route("/api/v0/searches/{id}", get(move || async move { Json(found) }).delete(|| async { StatusCode::OK }))
            .route(
                "/api/v0/transfers/downloads/{user}",
                post(|| async { StatusCode::CREATED }).get(move || async move { Json(transfers) }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        url
    }

    #[tokio::test]
    async fn a_cached_song_is_upgraded_from_soulseek_end_to_end() {
        use crate::db::NewTrack;
        use lofty::{file::TaggedFileExt, tag::Accessor};
        let dir = tempfile::tempdir().unwrap();
        let remote = r"\\bob\Music\Artist\Some Album\01 Song.flac";
        let mut cfg = Config::from_env();
        cfg.data = dir.path().join("data");
        cfg.library = dir.path().join("lib");
        cfg.cache = dir.path().join("cache");
        cfg.slsk_downloads = dir.path().join("slsk");
        cfg.slskd_url = fake_slskd(remote).await;
        std::fs::create_dir_all(cfg.slsk_downloads.join("Some Album")).unwrap();
        std::fs::create_dir_all(&cfg.library).unwrap();
        std::fs::write(cfg.slsk_downloads.join("Some Album/01 Song.flac"), include_bytes!("../tests/fixtures/silent.flac")).unwrap();

        let db = Db::memory().unwrap();
        let song = NewTrack {
            artist: "Artist".into(),
            dir_artist: "Artist".into(),
            title: "Song".into(),
            duration: Some(2.0),
            ..Default::default()
        };
        let id = db.add_track("r", &song).unwrap();
        db.next_job().unwrap();
        let cached = cfg.cache.join("Non-Album/Artist/Song.m4a");
        db.set_ready(id, cached.to_str().unwrap(), true).unwrap();
        let sl = Slskd::new(&cfg.slskd_url).unwrap();

        // Round one searches and asks for the file; round two sees it finished and files it.
        tick(&cfg, &db, &sl).await.unwrap();
        assert_eq!(db.upgrades().unwrap()["artist|song"].status, "downloading");
        tick(&cfg, &db, &sl).await.unwrap();
        assert_eq!(db.upgrades().unwrap()["artist|song"].status, "done");

        let dest = cfg.library.join("Non-Album/Artist/Song.flac");
        assert!(dest.exists());
        let tags = lofty::read_from_path(&dest).unwrap();
        let tag = tags.primary_tag().unwrap();
        assert_eq!((tag.artist().as_deref(), tag.title().as_deref()), (Some("Artist"), Some("Song")));
        assert_eq!(db.track_file("r", id).unwrap().as_deref(), dest.to_str(), "the queued song now plays the lossless file");
        assert!(
            db.lib_by_title(&norm("Song")).unwrap().iter().any(|(p, ..)| p == dest.to_str().unwrap()),
            "and the library index knows it"
        );
        assert!(std::fs::read_dir(cfg.data.join("upgrade")).unwrap().next().is_none(), "no scratch copy left behind");

        // Nothing left to upgrade: a further round changes nothing.
        tick(&cfg, &db, &sl).await.unwrap();
        assert_eq!(db.upgrades().unwrap().len(), 1);
    }
}
