//! Thin wrapper around the yt-dlp binary. Arguments are always passed as an array, never
//! through a shell, and URLs come after `--` so they can never be read as options.

use crate::config::Config;
use anyhow::{Result, anyhow, bail};
use serde_json::Value;
use std::{process::Stdio, time::Duration};
use tokio::process::Command;

fn base(cfg: &Config) -> Command {
    let mut c = Command::new(&cfg.python);
    c.arg(&cfg.ytdlp);
    c.args(["--ignore-config", "--no-warnings", "--no-colors"])
        .env("XDG_CACHE_HOME", cfg.data.join("cache"))
        .env("HOME", &cfg.data)
        .stdin(Stdio::null())
        .kill_on_drop(true);
    c
}

fn last_error(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let line = text.lines().rev().find(|l| l.contains("ERROR")).or_else(|| text.lines().next_back()).unwrap_or("failed");
    line.trim().trim_start_matches("ERROR:").trim().chars().take(200).collect()
}

/// Run yt-dlp with `-J` and parse its JSON output.
pub async fn json(cfg: &Config, args: &[&str], secs: u64) -> Result<Value> {
    let mut c = base(cfg);
    c.arg("-J").args(args);
    let out = tokio::time::timeout(Duration::from_secs(secs), c.output()).await.map_err(|_| anyhow!("yt-dlp timed out"))??;
    if !out.status.success() {
        bail!("{}", last_error(&out.stderr));
    }
    Ok(serde_json::from_slice(&out.stdout)?)
}

/// Download the best audio stream of one URL into `out_template`.
pub async fn download(cfg: &Config, url: &str, out_template: &str) -> Result<()> {
    let mut c = base(cfg);
    c.args(["-q", "-f", "ba[ext=m4a]/ba", "--no-playlist", "--max-filesize", "80M"])
        .args(["--match-filter", &format!("duration<={}", cfg.max_seconds)])
        .args(["-o", out_template, "--", url]);
    let out = tokio::time::timeout(Duration::from_secs(240), c.output()).await.map_err(|_| anyhow!("download timed out"))??;
    if !out.status.success() {
        bail!("{}", last_error(&out.stderr));
    }
    Ok(())
}

/// Self-update the private copy of yt-dlp; YouTube changes often enough that this matters.
pub async fn update(cfg: &Config) -> Result<String> {
    let mut c = base(cfg);
    c.arg("-U").stdout(Stdio::piped()).stderr(Stdio::piped());
    let out = tokio::time::timeout(Duration::from_secs(120), c.output()).await.map_err(|_| anyhow!("update timed out"))??;
    Ok(String::from_utf8_lossy(&out.stdout).lines().last().unwrap_or("").to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_the_error_line() {
        let s = b"[youtube] x: Downloading\nERROR: [youtube] x: Video unavailable\n";
        assert_eq!(last_error(s), "[youtube] x: Video unavailable");
        assert_eq!(last_error(b""), "failed");
    }
}
