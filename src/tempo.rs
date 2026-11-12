//! Tempo (BPM) of the library's songs, so Autoplay can follow the pace of the music.
//!
//! Library files rarely carry a BPM tag, so the Soulseek upgrader's container works through the
//! library slowly in the background. For each song it asks ffmpeg for 30 seconds of mono audio
//! from the middle, then finds the beat by autocorrelation of the onsets. That is accurate enough
//! to tell a slow song from a fast one, which is all the radio needs.

use crate::db::Db;
use anyhow::{Result, bail};
use std::{path::Path, time::Duration};
use tokio::process::Command;

const RATE: u32 = 8000;
/// 10 ms per step at 8 kHz.
const HOP: usize = 80;
const SECONDS: f64 = 30.0;
/// Lags in 10 ms steps for 200 BPM down to 60 BPM.
const MIN_LAG: usize = 30;
const MAX_LAG: usize = 100;

/// The tempo of mono audio sampled at 8 kHz, in beats per minute, or `None` if no beat is found.
pub fn estimate(samples: &[f32]) -> Option<f64> {
    let frames = samples.len() / HOP;
    if frames < 1000 {
        return None;
    }
    // Loudness per step, squashed so quiet beats count as much as loud ones.
    let env: Vec<f32> = (0..frames)
        .map(|i| {
            let s = &samples[i * HOP..(i + 1) * HOP];
            (1.0 + 100.0 * (s.iter().map(|x| x * x).sum::<f32>() / HOP as f32).sqrt()).ln()
        })
        .collect();
    // Onsets: how much louder each step is than the one before.
    let mut onset: Vec<f32> = std::iter::once(0.0).chain(env.windows(2).map(|w| (w[1] - w[0]).max(0.0))).collect();
    let mean = onset.iter().sum::<f32>() / frames as f32;
    onset.iter_mut().for_each(|o| *o -= mean);
    if onset.iter().map(|o| o * o).sum::<f32>() / (frames as f32) < 1e-6 {
        return None;
    }
    // How well the onsets repeat at each beat length.
    let ac: Vec<f32> =
        (MIN_LAG..=MAX_LAG).map(|lag| (0..frames - lag).map(|i| onset[i] * onset[i + lag]).sum::<f32>() / (frames - lag) as f32).collect();
    // A mild preference for tempos near 120, so a beat is not mistaken for half or double of itself.
    let weighted = |k: usize| {
        let bpm = 6000.0 / (MIN_LAG + k) as f32;
        let octaves = (bpm / 120.0).log2();
        ac[k] * (-(octaves * octaves) / (2.0 * 0.8 * 0.8)).exp()
    };
    let best = (0..ac.len()).max_by(|a, b| weighted(*a).total_cmp(&weighted(*b)))?;
    if ac[best] <= 0.0 {
        return None;
    }
    // Refine between neighbouring lags.
    let lag = if best > 0 && best + 1 < ac.len() {
        let (a, b, c) = (ac[best - 1], ac[best], ac[best + 1]);
        let denom = a - 2.0 * b + c;
        let shift = if denom.abs() > f32::EPSILON { 0.5 * (a - c) / denom } else { 0.0 };
        (MIN_LAG + best) as f32 + shift.clamp(-0.5, 0.5)
    } else {
        (MIN_LAG + best) as f32
    };
    Some(f64::from(6000.0 / lag))
}

/// Measure one song with ffmpeg. `None` means no clear beat (spoken word, ambient, silence).
pub async fn analyse(path: &Path, duration: f64) -> Result<Option<f64>> {
    let start = ((duration - SECONDS) / 2.0).max(0.0);
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-v", "error", "-nostdin", "-ss", &format!("{start:.1}"), "-t", &format!("{SECONDS:.0}"), "-i"])
        .arg(path)
        .args(["-ac", "1", "-ar", &RATE.to_string(), "-f", "f32le", "-"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let out = tokio::time::timeout(Duration::from_secs(60), cmd.output()).await??;
    if !out.status.success() {
        bail!("ffmpeg could not read the file");
    }
    let samples: Vec<f32> = out.stdout.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect();
    Ok(estimate(&samples))
}

/// Work through the library one song at a time, slowly, for as long as the process runs.
pub async fn run(db: Db, cache_prefix: String) {
    tracing::info!("tempo analysis is running in the background");
    loop {
        let todo = match db.bpm_todo(&cache_prefix, 20) {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(error = %e, "could not read the tempo work list");
                tokio::time::sleep(Duration::from_secs(60)).await;
                continue;
            }
        };
        if todo.is_empty() {
            tokio::time::sleep(Duration::from_secs(600)).await;
            continue;
        }
        for (path, duration) in todo {
            // 0 marks "looked, no clear beat", so the song is not tried again until the file changes.
            let bpm = analyse(Path::new(&path), duration).await.ok().flatten().unwrap_or(0.0);
            if let Err(e) = db.set_bpm(&path, bpm) {
                tracing::warn!(error = %e, "could not save a tempo");
            }
            tokio::time::sleep(Duration::from_millis(1500)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Eight kHz audio with a short click every beat: a decaying burst of a 1 kHz tone.
    fn clicks(bpm: f64, seconds: f64) -> Vec<f32> {
        let n = (seconds * f64::from(RATE)) as usize;
        let beat = 60.0 / bpm * f64::from(RATE);
        let mut out = vec![0.0f32; n];
        let mut at = 0.0;
        while (at as usize) < n {
            for k in 0..400 {
                let i = at as usize + k;
                if i < n {
                    out[i] += 0.8 * (-(k as f32) / 80.0).exp() * (2.0 * std::f32::consts::PI * 1000.0 * k as f32 / RATE as f32).sin();
                }
            }
            at += beat;
        }
        out
    }

    #[test]
    fn the_tempo_of_a_steady_beat_is_found_within_a_few_percent() {
        for bpm in [70.0, 90.0, 110.0, 128.0, 150.0, 170.0] {
            let found = estimate(&clicks(bpm, 30.0)).expect("a beat");
            assert!((found - bpm).abs() / bpm < 0.04, "{bpm} BPM was heard as {found}");
        }
    }

    #[test]
    fn silence_and_very_short_clips_have_no_tempo() {
        assert_eq!(estimate(&vec![0.0; 240_000]), None);
        assert_eq!(estimate(&clicks(120.0, 5.0)), None);
    }
}
