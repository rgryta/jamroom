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

/// The reason yt-dlp failed, in words a person in the room can act on. Known causes get a plain
/// sentence; anything else keeps yt-dlp's own text. Keep each sentence short: failed songs show it in the queue.
fn last_error(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let line = text.lines().rev().find(|l| l.contains("ERROR")).or_else(|| text.lines().next_back()).unwrap_or("failed");
    let line = line.trim().trim_start_matches("ERROR:").trim();
    let low = line.to_ascii_lowercase();
    let known = if low.contains("unavailable") || low.contains("not available") {
        Some("that isn't available any more (removed, private or blocked where the server is). Try another link or type the song name")
    } else if low.contains("sign in to confirm") {
        Some("YouTube is blocking the server for now. Try again in a few minutes")
    } else if low.contains("playlist type is unviewable") {
        Some("YouTube mixes can't be opened as a list. Open one song and copy its link")
    } else {
        None
    };
    known.map_or_else(|| line.chars().take(200).collect(), String::from)
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
        assert!(last_error(s).starts_with("that isn't available any more"));
        assert_eq!(last_error(b""), "failed");
        assert_eq!(last_error(b"ERROR: [generic] something odd happened\n"), "[generic] something odd happened");
    }

    #[test]
    fn known_causes_get_a_plain_sentence() {
        let gone = last_error(b"ERROR: [youtube] VnCgmlxHXH0: This video is not available\n");
        assert!(gone.contains("removed, private or blocked") && gone.contains("type the song name"));
        assert!(last_error(b"ERROR: [youtube] x: Sign in to confirm you're not a bot\n").starts_with("YouTube is blocking"));
        assert!(last_error(b"ERROR: [youtube:tab] RDx: YouTube said: This playlist type is unviewable.\n").contains("mixes"));
        // Failed songs show this text in the queue, which cuts it at 160 characters.
        assert!(gone.len() < 160);
    }
}
