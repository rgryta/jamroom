//! Runtime settings, all from environment variables so the compose file is the only config.

use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Config {
    pub addr: String,
    /// Holds the SQLite database, the yt-dlp copy and scratch downloads.
    pub data: PathBuf,
    /// The music library. jamroom reads it to reuse files you already have; it does not write YouTube downloads here.
    pub library: PathBuf,
    /// Where downloaded songs are kept, outside the library, so only better sources reach the library.
    pub cache: PathBuf,
    /// Cached songs older than this many days are deleted unless they are still queued.
    pub cache_days: i64,
    /// slskd's address, for the Soulseek upgrader (`jamroom upgrade`). Empty means off.
    pub slskd_url: String,
    /// Where slskd keeps finished downloads, as seen by the upgrader.
    pub slsk_downloads: PathBuf,
    /// Bearer token for the room admin API. Empty disables the admin API.
    pub admin_token: String,
    /// yt-dlp binary used for downloads.
    pub ytdlp: PathBuf,
    /// Interpreter that runs the yt-dlp script, so the data volume does not need to allow execution.
    pub python: String,
    pub max_seconds: u32,
    pub concurrency: usize,
    pub max_queue: i64,
    pub max_playlist: usize,
    /// Upper bound on new files written to the library per day.
    pub max_new_per_day: i64,
    /// Adds allowed per client address per minute.
    pub adds_per_minute: u32,
}

fn var<T: std::str::FromStr>(name: &str, default: T) -> T {
    std::env::var(name).ok().and_then(|v| v.trim().parse().ok()).unwrap_or(default)
}

impl Config {
    pub fn from_env() -> Self {
        let data = PathBuf::from(var("JAM_DATA", "/data".to_string()));
        Config {
            addr: var("JAM_ADDR", "0.0.0.0:8080".to_string()),
            python: var("JAM_PYTHON", "python3".to_string()),
            ytdlp: std::env::var("JAM_YTDLP").map(PathBuf::from).unwrap_or_else(|_| data.join("bin/yt-dlp")),
            cache: std::env::var("JAM_CACHE").map(PathBuf::from).unwrap_or_else(|_| data.join("songs")),
            cache_days: var("JAM_CACHE_DAYS", 60),
            slskd_url: var("JAM_SLSKD_URL", String::new()),
            slsk_downloads: PathBuf::from(var("JAM_SLSK_DOWNLOADS", "/slsk".to_string())),
            data,
            library: PathBuf::from(var("JAM_LIBRARY", "/music".to_string())),
            admin_token: var("JAM_ADMIN_TOKEN", String::new()),
            max_seconds: var("JAM_MAX_SECONDS", 900),
            concurrency: var("JAM_CONCURRENCY", 2),
            max_queue: var("JAM_MAX_QUEUE", 50),
            max_playlist: var("JAM_MAX_PLAYLIST", 50),
            max_new_per_day: var("JAM_MAX_NEW_PER_DAY", 150),
            adds_per_minute: var("JAM_ADDS_PER_MINUTE", 20),
        }
    }
}
