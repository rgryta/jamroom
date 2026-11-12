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
    norm::{artist_match, norm},
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

/// Words that mark a bracket group or tail as video-site decoration rather than part of the title.
const VIDEO_WORDS: &[&str] = &["official", "video", "audio", "lyric", "lyrics", "visualizer", "hd", "4k", "hq"];
/// Album names that are really compilations.
const COMPILATION_WORDS: &[&str] =
    &["compilation", "various", "what i call", "ultimix", "just dance", "vol.", "volume", "hits", "best of", "summer", "party", "karaoke"];

fn words(s: &str) -> impl Iterator<Item = String> + '_ {
    s.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(str::to_lowercase)
}

fn alnum(s: &str) -> String {
    s.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect()
}

/// A song title without video-site decoration: "(Official Video)", "[HD]", "- Lyrics" and a
/// trailing "ft. Someone" (the featured artist belongs in the artist tag).
pub fn tidy_title(title: &str) -> String {
    let chars: Vec<char> = title.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        let close = match chars[i] {
            '(' => Some(')'),
            '[' => Some(']'),
            _ => None,
        };
        if let Some(close) = close
            && let Some(len) = chars[i + 1..].iter().position(|c| *c == close)
        {
            let inner: String = chars[i + 1..i + 1 + len].iter().collect();
            if words(&inner).any(|w| VIDEO_WORDS.contains(&w.as_str())) {
                i += len + 2;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    let mut out = out.split_whitespace().collect::<Vec<_>>().join(" ");
    if let Some((head, tail)) = out.split_once(" - ")
        && words(tail).any(|w| VIDEO_WORDS.contains(&w.as_str()))
    {
        out = head.trim().to_string();
    }
    let lower = out.to_ascii_lowercase();
    let cut = [" feat.", " feat ", " ft.", " ft ", " featuring "].iter().filter_map(|m| lower.find(m)).min();
    if let Some(i) = cut {
        out.truncate(i);
    }
    let out = out.trim().to_string();
    if out.is_empty() { title.trim().to_string() } else { out }
}

/// The artist a song is filed under: the first one, without "feat. ..." tails.
pub fn main_artist(artist: &str) -> String {
    let mut first = first_artist(artist);
    let lower = first.to_ascii_lowercase();
    if let Some(i) = [" feat.", " feat ", " ft.", " ft ", " featuring "].iter().filter_map(|m| lower.find(m)).min() {
        first.truncate(i);
    }
    first.trim().to_string()
}

/// True for albums that only collect other people's songs ("Various Artists", "Now That's What I Call Music!").
pub fn looks_like_compilation(album: &str, album_artist: Option<&str>, artist: &str) -> bool {
    if album_artist.is_some_and(|aa| !artist_match(aa, artist)) {
        return true;
    }
    let low = album.to_lowercase();
    COMPILATION_WORDS.iter().any(|w| low.contains(w))
}

/// How the downloaded file is named and tagged in the library.
#[derive(Debug, PartialEq)]
pub struct Names {
    pub artist: String,
    pub dir_artist: String,
    pub title: String,
    pub album: Option<String>,
}

/// An error message with this prefix means "this file is not the recording we want", not a fault.
const NOT_THE_SONG: &str = "not the right recording: ";

/// A title without video decoration and without a bracketed "feat." credit, for comparing two titles.
fn bare_title(title: &str) -> String {
    let t = tidy_title(title);
    let mut out = String::new();
    let mut rest = t.as_str();
    while let Some(open) = rest.find(['(', '[']) {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        match after.find([')', ']']) {
            Some(close) => {
                let inner = after[..close].trim_start().to_lowercase();
                if !(inner.starts_with("feat") || inner.starts_with("ft") || inner.starts_with("with")) {
                    out.push_str(&rest[open..open + close + 2]);
                }
                rest = &after[close + 1..];
            }
            None => {
                out.push_str(&rest[open..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    alnum(&out)
}

/// True when two artist credits share a name: "Cardi B & Bruno Mars" and "Bruno Mars, Cardi B".
pub fn artists_overlap(a: &str, b: &str) -> bool {
    let names = |s: &str| -> Vec<String> {
        s.replace(" & ", ",")
            .replace(" and ", ",")
            .replace(" x ", ",")
            .replace(" feat.", ",")
            .replace(" ft.", ",")
            .replace(" featuring ", ",")
            .split([',', ';'])
            .map(|n| n.trim().to_string())
            .filter(|n| !n.is_empty())
            .collect()
    };
    let (na, nb) = (names(a), names(b));
    na.iter().any(|x| nb.iter().any(|y| artist_match(x, y))) || artist_match(a, b)
}

/// Decide how the downloaded file is named and tagged, or refuse it. The file's own tags are
/// trusted for names (Soulseek files are usually well tagged) but never the video-site title the
/// request came with, and never a compilation album. A file whose tags say it is another version
/// ("(kwikMIX ...)", "(Intro Clean)") or another artist's song is refused.
pub fn choose_names(tags: &library::FileTags, wanted_artist: &str, wanted_title: &str) -> Result<Names, String> {
    let wanted = tidy_title(wanted_title);
    let title = match tags.title.as_deref() {
        Some(t) => {
            let (have, want) = (bare_title(t), bare_title(&wanted));
            if have == want || (have.chars().count() >= 3 && want.starts_with(&have)) {
                tidy_title(t)
            } else if have.starts_with(&want) {
                return Err(format!("{NOT_THE_SONG}it is titled \"{t}\", a different version"));
            } else {
                wanted
            }
        }
        None => wanted,
    };
    let tag_artist = tags.artist.clone().filter(|a| !matches!(alnum(a).as_str(), "variousartists" | "various" | "va"));
    // A request whose artist came off a video ("Song (Spanish Version)") is not worth comparing against.
    let junk_artist = wanted_artist.contains(['(', '[', '|']);
    if let Some(a) = &tag_artist
        && !junk_artist
        && !artists_overlap(a, wanted_artist)
    {
        return Err(format!("{NOT_THE_SONG}it is by \"{a}\", not {wanted_artist}"));
    }
    let artist = tag_artist.unwrap_or_else(|| wanted_artist.to_string());
    let album = tags.album.clone().filter(|al| !looks_like_compilation(al, tags.album_artist.as_deref(), &artist));
    Ok(Names { dir_artist: main_artist(&artist), artist, title, album })
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
        let names = choose_names(&library::read_tags(&copy), &up.artist, &up.title).map_err(|e| anyhow!(e))?;
        library::write_tags(&copy, &names.artist, &names.dir_artist, &names.title, names.album.as_deref(), None)?;
        if names.album.is_none() {
            library::clear_album(&copy)?;
        }
        let ext = copy.extension().and_then(|e| e.to_str()).unwrap_or("flac").to_ascii_lowercase();
        // A real album goes in `<Artist>/<Album>/`. If that artist folder is yours and jamroom may not
        // write there, or the song has no album (or only a compilation), it goes in `Non-Album/<Artist>/`.
        if let Some(album) = names.album.as_deref() {
            let filed = library::target_path(&cfg.library, &names.dir_artist, Some(album), &names.title, &ext);
            if filed.exists() {
                return Ok(filed);
            }
            match library::place(&copy, &filed, &cfg.library) {
                Ok(()) => return Ok(filed),
                Err(e) if e.downcast_ref::<std::io::Error>().is_some_and(|io| io.kind() == std::io::ErrorKind::PermissionDenied) => {}
                Err(e) => return Err(e),
            }
        }
        let flat = library::target_path(&cfg.library, &names.dir_artist, None, &names.title, &ext);
        if !flat.exists() {
            library::place(&copy, &flat, &cfg.library)?;
        }
        Ok(flat)
    })();
    let _ = std::fs::remove_file(&copy);
    result
}

fn settle(db: &Db, up: &Upgrade, status: &str, note: &str) -> Result<()> {
    db.upgrade_put(&Upgrade { status: status.into(), note: note.into(), updated_at: now(), ..up.clone() })
}

/// The library already has this song: let waiting queue entries use that copy when it is the same recording.
fn use_existing(cfg: &Config, db: &Db, up: &Upgrade) -> Result<()> {
    if let Some(same) = library::find(db, &cfg.cache, &up.artist, &up.title, up.duration).filter(|p| !Path::new(p).starts_with(&cfg.cache))
    {
        db.apply_upgrade(&up.cached, &same)?;
    }
    Ok(())
}

/// Check a download that was queued earlier.
async fn follow(cfg: &Config, db: &Db, sl: &Slskd, up: &Upgrade) -> Result<()> {
    match sl.transfer(&up.peer, &up.remote).await? {
        Some((state, _)) if state == "Completed, Succeeded" => {
            // The song may have joined the library while the download waited. Never add a second copy.
            if library::same_song(db, &cfg.cache, &up.artist, &up.title).is_some() {
                use_existing(cfg, db, up)?;
                return settle(db, up, "done", "already in the library");
            }
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
                Err(e) if e.to_string().starts_with(NOT_THE_SONG) => {
                    tracing::info!(song = %up.title, reason = %e, "skipped a Soulseek download");
                    settle(db, up, "none", &e.to_string())
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
    if library::same_song(db, &cfg.cache, &fresh.artist, &fresh.title).is_some() {
        use_existing(cfg, db, &fresh)?;
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

    /// A library, a cache, a download folder with one tagged FLAC in it, a fake slskd and one cached song.
    async fn world(requested_title: &str) -> (tempfile::TempDir, Config, Db, Slskd, i64) {
        world_with(requested_title, "Now That's What I Call Music! 74", "Various Artists").await
    }

    async fn world_with(requested_title: &str, album: &str, album_artist: &str) -> (tempfile::TempDir, Config, Db, Slskd, i64) {
        use crate::db::NewTrack;
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
        let file = cfg.slsk_downloads.join("Some Album/01 Song.flac");
        std::fs::write(&file, include_bytes!("../tests/fixtures/silent.flac")).unwrap();
        // How a real Soulseek file is tagged: clean title, and a compilation as its album.
        library::write_tags(&file, "Artist", album_artist, "Song", Some(album), None).unwrap();

        let db = Db::memory().unwrap();
        let song = NewTrack {
            artist: "Artist".into(),
            dir_artist: "Artist".into(),
            title: requested_title.into(),
            duration: Some(2.0),
            ..Default::default()
        };
        let id = db.add_track("r", &song).unwrap();
        db.next_job().unwrap();
        db.set_ready(id, cfg.cache.join("Non-Album/Artist/Song.m4a").to_str().unwrap(), true).unwrap();
        let sl = Slskd::new(&cfg.slskd_url).unwrap();
        (dir, cfg, db, sl, id)
    }

    #[tokio::test]
    async fn a_cached_song_is_upgraded_from_soulseek_end_to_end() {
        use lofty::{file::TaggedFileExt, tag::Accessor};
        let (_dir, cfg, db, sl, id) = world("Song (Official Video)").await;

        // Round one searches and asks for the file; round two sees it finished and files it.
        tick(&cfg, &db, &sl).await.unwrap();
        assert_eq!(db.upgrades().unwrap()["artist|song"].status, "downloading");
        tick(&cfg, &db, &sl).await.unwrap();
        assert_eq!(db.upgrades().unwrap()["artist|song"].status, "done");

        // Filed under Non-Album with the file's own clean title, not the video title or the compilation.
        let dest = cfg.library.join("Non-Album/Artist/Song.flac");
        assert!(dest.exists(), "{:?}", std::fs::read_dir(&cfg.library).unwrap().flatten().collect::<Vec<_>>());
        let tags = lofty::read_from_path(&dest).unwrap();
        let tag = tags.primary_tag().unwrap();
        assert_eq!((tag.artist().as_deref(), tag.title().as_deref()), (Some("Artist"), Some("Song")));
        assert_eq!(tag.album(), None, "a compilation is not kept as the album");
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

    #[tokio::test]
    async fn a_song_you_already_own_is_never_added_a_second_time() {
        use crate::db::LibEntry;
        let (_dir, cfg, db, sl, id) = world("Song").await;
        // Your own copy, a slightly different edit (so not the same length), in your own folders.
        let mine = cfg.library.join("Artist/Album/01 Song.mp3");
        db.lib_upsert(&LibEntry {
            path: mine.to_string_lossy().into(),
            mtime: 1,
            artist: "Artist".into(),
            title: "Song".into(),
            tn: norm("Song"),
            duration: Some(30.0),
        })
        .unwrap();
        tick(&cfg, &db, &sl).await.unwrap();
        let up = &db.upgrades().unwrap()["artist|song"];
        assert_eq!((up.status.as_str(), up.note.as_str()), ("done", "already in the library"));
        assert!(!cfg.library.join("Non-Album").exists(), "nothing was added to the library");
        assert_ne!(db.track_file("r", id).unwrap().as_deref(), mine.to_str(), "a different edit is not swapped in");
        // The same recording is used instead of the cached download.
        db.lib_upsert(&LibEntry {
            path: mine.to_string_lossy().into(),
            mtime: 1,
            artist: "Artist".into(),
            title: "Song".into(),
            tn: norm("Song"),
            duration: Some(2.0),
        })
        .unwrap();
        let again = Upgrade { key: "artist|song".into(), ..up.clone() };
        use_existing(&cfg, &db, &again).unwrap();
        assert_eq!(db.track_file("r", id).unwrap().as_deref(), mine.to_str());
    }

    #[test]
    fn titles_artists_and_albums_are_tidied_for_the_library() {
        assert_eq!(tidy_title("Sucker (Official Video)"), "Sucker");
        assert_eq!(tidy_title("Too Sweet [HD]"), "Too Sweet");
        assert_eq!(tidy_title("Song - Official Lyric Video"), "Song");
        assert_eq!(tidy_title("Morning After Dark (Official Music Video) ft. Nelly Furtado, Soshy"), "Morning After Dark");
        assert_eq!(
            tidy_title("Get Lucky (feat. Pharrell Williams)"),
            "Get Lucky (feat. Pharrell Williams)",
            "a bracketed feature is part of the title"
        );
        assert_eq!(tidy_title("(Official Video)"), "(Official Video)", "never leave a title empty");
        assert_eq!(main_artist("Kato feat. Jon"), "Kato");
        assert_eq!(main_artist("Timbaland, Nelly Furtado"), "Timbaland");
        assert_eq!(main_artist("Simon & Garfunkel"), "Simon & Garfunkel");
        assert!(looks_like_compilation("Now That's What I Call Music! 74", None, "Sean Kingston"));
        assert!(looks_like_compilation("Anything", Some("Various Artists"), "Sean Kingston"));
        assert!(!looks_like_compilation("Homework", Some("Daft Punk"), "Daft Punk"));
        assert!(!looks_like_compilation("Unheard", None, "Hozier"));
    }

    #[test]
    fn the_file_is_trusted_for_names_but_the_request_fills_gaps() {
        let tags = library::FileTags {
            artist: Some("Kato feat. Jon".into()),
            title: Some("Turn The Lights Off".into()),
            album: Some("Summer Heat 2010".into()),
            album_artist: None,
        };
        let n = choose_names(&tags, "Kato feat. Jon", "Turn The Lights Off (Jon Hamm)").unwrap();
        assert_eq!((n.dir_artist.as_str(), n.title.as_str(), n.album), ("Kato", "Turn The Lights Off", None));
        // A file with no tags falls back to the tidied request.
        let n = choose_names(&library::FileTags::default(), "Hozier", "Too Sweet (Official Video)").unwrap();
        assert_eq!((n.artist.as_str(), n.title.as_str()), ("Hozier", "Too Sweet"));
        // "Various Artists" is not an artist; an unrelated title tag is ignored.
        let odd = library::FileTags { artist: Some("Various Artists".into()), title: Some("Other Song".into()), ..Default::default() };
        let n = choose_names(&odd, "Hozier", "Too Sweet").unwrap();
        assert_eq!((n.artist.as_str(), n.title.as_str()), ("Hozier", "Too Sweet"));
        // A real album is kept.
        let real = library::FileTags {
            artist: Some("Daft Punk".into()),
            album: Some("Homework".into()),
            album_artist: Some("Daft Punk".into()),
            ..Default::default()
        };
        assert_eq!(choose_names(&real, "Daft Punk", "Around the World").unwrap().album.as_deref(), Some("Homework"));
    }

    #[test]
    fn other_versions_and_other_artists_are_refused_but_credits_are_fine() {
        let tagged =
            |artist: &str, title: &str| library::FileTags { artist: Some(artist.into()), title: Some(title.into()), ..Default::default() };
        // Found in the real library run: DJ edits and a different artist's song with the same name.
        let dj = choose_names(&tagged("Jonas Brothers", "Sucker (kwikMIX by Stacy Mier)"), "Jonas Brothers", "Sucker (Official Video)");
        assert!(dj.unwrap_err().starts_with(NOT_THE_SONG));
        let intro = choose_names(
            &tagged("Bruno Mars & Cardi B", "Please Me (Trayze My Boo Blend) (Intro Clean) 130"),
            "Cardi B & Bruno Mars",
            "Please Me",
        );
        assert!(intro.is_err());
        let other = choose_names(&tagged("Living Scars", "Without Me"), "Halsey", "Without Me");
        assert!(other.unwrap_err().contains("Living Scars"));
        // The same duo credited the other way round, and a featured artist in the title, are fine.
        assert!(choose_names(&tagged("Bruno Mars & Cardi B", "Please Me"), "Cardi B & Bruno Mars", "Please Me (Official Video)").is_ok());
        let feat = choose_names(&tagged("Halsey", "Without Me (feat. Khalid)"), "Halsey", "Without Me").unwrap();
        assert_eq!(feat.title, "Without Me (feat. Khalid)");
        // A video-site artist name cannot be compared, so the file's own artist wins.
        let ketchup = choose_names(&tagged("Las Ketchup", "Las Ketchup"), "Aserejé (The Ketchup Song) [Spanish Version]", "Las Ketchup");
        assert_eq!(ketchup.unwrap().dir_artist, "Las Ketchup");
    }

    #[tokio::test]
    async fn a_real_album_gets_its_own_folder_and_a_blocked_artist_folder_falls_back_to_non_album() {
        use std::os::unix::fs::PermissionsExt;
        // A real album: <Artist>/<Album>/<Title>.
        let (_dir, cfg, db, sl, _) = world_with("Song", "Homework", "Artist").await;
        tick(&cfg, &db, &sl).await.unwrap();
        tick(&cfg, &db, &sl).await.unwrap();
        assert!(cfg.library.join("Artist/Homework/Song.flac").exists());
        assert!(!cfg.library.join("Non-Album").exists());

        // An artist folder of yours that jamroom may not write to: the song goes to Non-Album instead.
        let (_dir, cfg, db, sl, _) = world_with("Song", "Homework", "Artist").await;
        std::fs::create_dir_all(cfg.library.join("Artist")).unwrap();
        std::fs::set_permissions(cfg.library.join("Artist"), std::fs::Permissions::from_mode(0o555)).unwrap();
        if std::fs::File::create(cfg.library.join("Artist/probe")).is_ok() {
            return; // running as root, where permissions do not apply
        }
        tick(&cfg, &db, &sl).await.unwrap();
        tick(&cfg, &db, &sl).await.unwrap();
        assert!(cfg.library.join("Non-Album/Artist/Song.flac").exists());
        assert!(!cfg.library.join("Artist/Homework").exists());
        std::fs::set_permissions(cfg.library.join("Artist"), std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}
