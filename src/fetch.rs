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
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .find(|p| p.is_file() && (library::is_audio(p) || p.extension().is_some_and(|e| e == "webm" || e == "mp4")))
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

    if let Some(p) = library::find(&app.db, &it.artist, &it.title, it.duration) {
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
            if let Some(p) = library::find(&app.db, &it.artist, &it.title, it.duration) {
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
    let dest = library::target_path(&app.cfg.library, &it.dir_artist, it.album.as_deref(), &it.title, &ext);
    let root = app.cfg.library.clone();
    let (tag_artist, tag_title, tag_album, dir_artist) = (it.artist.clone(), it.title.clone(), it.album.clone(), it.dir_artist.clone());
    let dest2 = dest.clone();
    if !dest.exists() {
        tokio::task::spawn_blocking(move || -> Result<()> {
            library::write_tags(&file, &tag_artist, &dir_artist, &tag_title, tag_album.as_deref(), cover)?;
            library::place(&file, &dest2, &root)
        })
        .await??;
    }
    if let Some(e) = library::read_entry(&dest, &app.cfg.library) {
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
                            let _ = app.db.set_failed(job.id, &e.to_string().chars().take(160).collect::<String>());
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
