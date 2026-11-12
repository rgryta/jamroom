//! The worker: take a queued track, reuse the library copy or download, tag and file it.

use crate::{app::App, db::Job, library, resolve::first_artist, ytdlp};
use anyhow::{Result, bail};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::sync::Semaphore;

/// Removes the scratch download folder however the job ends.
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn downloaded_file(dir: &Path) -> Option<PathBuf> {
    std::fs::read_dir(dir).ok()?.flatten().map(|e| e.path()).find(|p| p.is_file() && library::is_audio(p))
}

async fn cover_bytes(app: &App, url: &str) -> Option<Vec<u8>> {
    if !url.starts_with("https://") {
        return None;
    }
    let resp = app.resolver.http().get(url).send().await.ok()?.error_for_status().ok()?;
    if resp.content_length().is_some_and(|n| n > 4_000_000) {
        return None;
    }
    resp.bytes().await.ok().map(|b| b.to_vec())
}

/// Make a track playable. Returns the file path that the player will stream.
async fn make_ready(app: &App, job: &Job) -> Result<()> {
    let id = job.id;
    let mut it = job.item.clone();

    if let Some(p) = library::best(&app.db, &app.cfg.cache, &it.artist, &it.title, it.duration) {
        return app.db.set_ready(id, &p, false);
    }

    let mut src = it.src.clone();
    let mut matched = false;
    if src.is_none() {
        let cand = match &it.query {
            Some(q) => app.resolver.first_for_query(q).await?,
            None => app.resolver.match_source(&it.artist, &it.title, it.duration).await?,
        };
        if it.query.is_some() {
            // The request only had a name; adopt what the search found.
            it.artist = cand.artist.clone();
            it.dir_artist = first_artist(&cand.artist);
            it.title = cand.title.clone();
            it.album = cand.album.clone();
            it.duration = cand.duration;
            app.db.set_details(id, &it.artist, &it.title, it.album.as_deref())?;
            if let Some(p) = library::best(&app.db, &app.cfg.cache, &it.artist, &it.title, it.duration) {
                return app.db.set_ready(id, &p, false);
            }
        }
        src = Some(cand.url());
        matched = true;
    }
    let Some(src) = src else { bail!("no source for this track") };

    if app.db.fetched_today()? >= app.cfg.max_new_per_day {
        bail!("daily download limit reached, try again tomorrow");
    }

    let tmp = app.cfg.data.join("tmp").join(id.to_string());
    let _scratch = Scratch(tmp.clone());
    tokio::fs::create_dir_all(&tmp).await?;
    ytdlp::download(&app.cfg, &src, &format!("{}/%(id)s.%(ext)s", tmp.display())).await?;
    let Some(file) = downloaded_file(&tmp) else { bail!("nothing was downloaded (too long, too large or unavailable)") };

    if matched
        && let (Some(want), Some(have)) = (it.duration, library::probe_duration(&file))
        && (want - have).abs() > 8.0
    {
        bail!("the matched recording has the wrong length");
    }
    if it.artist.trim().is_empty() {
        it.artist = "Unknown Artist".into();
        it.dir_artist = "Unknown Artist".into();
    }

    let cover = match &it.cover {
        Some(u) => cover_bytes(app, u).await,
        None => None,
    };
    let ext = file.extension().and_then(|e| e.to_str()).unwrap_or("m4a").to_ascii_lowercase();
    // YouTube audio is 128 kbps AAC, so it stays in the cache and never reaches the music library.
    let dest = library::target_path(&app.cfg.cache, &it.dir_artist, it.album.as_deref(), &it.title, &ext);
    let root = app.cfg.cache.clone();
    let (tag_artist, tag_title, tag_album, dir_artist) = (it.artist.clone(), it.title.clone(), it.album.clone(), it.dir_artist.clone());
    let dest2 = dest.clone();
    if !dest.exists() {
        tokio::task::spawn_blocking(move || -> Result<()> {
            library::write_tags(&file, &tag_artist, &dir_artist, &tag_title, tag_album.as_deref(), cover)?;
            library::place(&file, &dest2, &root)
        })
        .await??;
        tracing::info!(path = %dest.display(), "cached");
    }
    if let Some(e) = library::read_entry(&dest, &app.cfg.cache) {
        app.db.lib_upsert(&e)?;
    }
    app.db.set_ready(id, &dest.to_string_lossy(), true)?;
    Ok(())
}

/// Run until the process ends, handling up to `concurrency` jobs at once.
pub async fn run(app: Arc<App>) {
    let gate = Arc::new(Semaphore::new(app.cfg.concurrency.max(1)));
    loop {
        let permit = gate.clone().acquire_owned().await.expect("semaphore is never closed");
        match app.db.next_job() {
            Ok(Some(job)) => {
                let app = app.clone();
                tokio::spawn(async move {
                    match make_ready(&app, &job).await {
                        Ok(()) => tracing::info!(id = job.id, title = %job.item.title, "ready"),
                        Err(e) => {
                            tracing::warn!(id = job.id, title = %job.item.title, error = %e, "failed");
                            // Disk errors name server paths; clients only get a short note.
                            let shown =
                                if e.downcast_ref::<std::io::Error>().is_some() { "could not save the file".into() } else { e.to_string() };
                            let _ = app.db.set_failed(job.id, &shown.chars().take(160).collect::<String>());
                        }
                    }
                    drop(permit);
                });
            }
            other => {
                if let Err(e) = other {
                    tracing::error!(error = %e, "could not read the queue");
                }
                drop(permit);
                let _ = tokio::time::timeout(std::time::Duration::from_secs(3), app.wake.notified()).await;
            }
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::{
        config::Config,
        db::{Db, NewTrack},
    };

    /// A stand-in for yt-dlp: copies a real silent m4a to the `-o` path.
    fn app_with_stub(dir: &Path) -> Arc<App> {
        let fixture = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/silent.m4a");
        let stub = dir.join("stub.sh");
        std::fs::write(
            &stub,
            format!("while [ $# -gt 0 ]; do [ \"$1\" = -o ] && out=$2; shift; done\nout=$(echo \"$out\" | sed 's/%(id)s/x/;s/%(ext)s/m4a/')\ncp {fixture} \"$out\"\n"),
        )
        .unwrap();
        let mut cfg = Config::from_env();
        cfg.python = "sh".into();
        cfg.ytdlp = stub;
        cfg.data = dir.join("data");
        cfg.library = dir.join("lib");
        cfg.cache = dir.join("cache");
        std::fs::create_dir_all(&cfg.library).unwrap();
        std::fs::create_dir_all(&cfg.cache).unwrap();
        std::fs::create_dir_all(cfg.data.join("tmp")).unwrap();
        App::new(Arc::new(cfg), Db::memory().unwrap()).unwrap()
    }

    fn song() -> NewTrack {
        NewTrack {
            artist: "Artist".into(),
            dir_artist: "Artist".into(),
            title: "Song".into(),
            album: Some("Album".into()),
            src: Some("https://music.youtube.com/watch?v=x".into()),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn a_download_is_tagged_filed_and_playable_even_when_two_jobs_want_the_same_song() {
        let dir = tempfile::tempdir().unwrap();
        let app = app_with_stub(dir.path());
        let (a, b) = (app.db.add_track("r", &song()).unwrap(), app.db.add_track("r", &song()).unwrap());
        let (ja, jb) = (app.db.next_job().unwrap().unwrap(), app.db.next_job().unwrap().unwrap());
        let (ra, rb) = tokio::join!(make_ready(&app, &ja), make_ready(&app, &jb));
        ra.unwrap();
        rb.unwrap();
        let file = dir.path().join("cache/Artist/Album/Song.m4a");
        assert!(file.exists());
        assert_eq!(std::fs::read_dir(dir.path().join("lib")).unwrap().count(), 0, "nothing from YouTube reaches the library");
        assert_eq!(app.db.track_file("r", a).unwrap().as_deref(), file.to_str());
        assert_eq!(app.db.track_file("r", b).unwrap().as_deref(), file.to_str());
        assert_eq!(std::fs::read_dir(file.parent().unwrap()).unwrap().count(), 1, "no .part files left behind");
    }

    #[tokio::test]
    async fn a_song_already_in_the_library_is_not_downloaded() {
        let dir = tempfile::tempdir().unwrap();
        let app = app_with_stub(dir.path());
        let first = app.db.add_track("r", &song()).unwrap();
        let job = app.db.next_job().unwrap().unwrap();
        make_ready(&app, &job).await.unwrap();
        library::scan(&app.db, &[&app.cfg.library, &app.cfg.cache]).unwrap();
        // Break the downloader: a second request must be served from the library.
        std::fs::remove_file(&app.cfg.ytdlp).unwrap();
        let second = app.db.add_track("r", &song()).unwrap();
        let job = app.db.next_job().unwrap().unwrap();
        make_ready(&app, &job).await.unwrap();
        assert_eq!(app.db.track_file("r", second).unwrap(), app.db.track_file("r", first).unwrap());
    }
}
