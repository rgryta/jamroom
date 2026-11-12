//! jamroom: a shared play queue. Anyone with a room code pastes a link or a song name; one
//! browser tab plays the queue through whatever speakers it is connected to. Every track
//! that has to be downloaded is tagged and added to the music library.

mod app;
mod config;
mod db;
mod fetch;
mod library;
mod norm;
mod resolve;
mod web;
mod ytdlp;

use anyhow::Result;
use app::App;
use config::Config;
use db::Db;
use std::{net::SocketAddr, sync::Arc, time::Duration};

/// The image ships a yt-dlp; keep a private writable copy so it can update itself.
fn ensure_ytdlp(cfg: &Config) -> Result<()> {
    if cfg.ytdlp.exists() {
        return Ok(());
    }
    let seed = std::env::var("JAM_YTDLP_SEED").unwrap_or_else(|_| "/usr/local/bin/yt-dlp".into());
    if let Some(dir) = cfg.ytdlp.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::copy(&seed, &cfg.ytdlp)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&cfg.ytdlp, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}

/// Docker stops a container with SIGTERM, which PID 1 ignores unless it asks for it.
async fn shutdown() {
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("can listen for SIGTERM");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let cfg = Arc::new(Config::from_env());
    std::fs::create_dir_all(&cfg.data)?;
    let tmp = cfg.data.join("tmp");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp)?;
    std::fs::create_dir_all(cfg.data.join("cache"))?;
    ensure_ytdlp(&cfg)?;

    let db = Db::open(&cfg.data.join("jamroom.db"))?;
    db.reset_stale()?;
    if let Ok(name) = std::env::var("JAM_DEFAULT_ROOM")
        && db.rooms()?.is_empty()
    {
        let code = web::new_code()?;
        db.create_room(&code, &name)?;
        tracing::info!(%code, %name, "created the first room");
    }
    let app = App::new(cfg.clone(), db)?;

    // Keep the library index current: once at start, then every half hour.
    let scanner = app.clone();
    tokio::spawn(async move {
        loop {
            let a = scanner.clone();
            match tokio::task::spawn_blocking(move || library::scan(&a.db, &a.cfg.library)).await {
                Ok(Ok((files, gone))) => tracing::info!(files, gone, "library indexed"),
                Ok(Err(e)) => tracing::warn!(error = %e, "library scan failed"),
                Err(e) => tracing::warn!(error = %e, "library scan crashed"),
            }
            tokio::time::sleep(Duration::from_secs(1800)).await;
        }
    });

    // YouTube changes often; update yt-dlp daily.
    let updater = app.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(300)).await;
            match ytdlp::update(&updater.cfg).await {
                Ok(msg) => tracing::info!(%msg, "yt-dlp update check"),
                Err(e) => tracing::warn!(error = %e, "yt-dlp update failed"),
            }
            tokio::time::sleep(Duration::from_secs(86_400)).await;
        }
    });

    tokio::spawn(fetch::run(app.clone()));

    let listener = tokio::net::TcpListener::bind(&cfg.addr).await?;
    tracing::info!(addr = %cfg.addr, library = %cfg.library.display(), "jamroom is up");
    axum::serve(listener, web::router(app).into_make_service_with_connect_info::<SocketAddr>()).with_graceful_shutdown(shutdown()).await?;
    Ok(())
}
