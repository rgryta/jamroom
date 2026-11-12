//! Autoplay: when a room's queue runs dry and Autoplay is on, pick the next song from the music
//! library, so the room keeps playing like a radio. Library files are local and full quality, so
//! the song starts at once and nothing is downloaded.
//!
//! * Mood off: a random song, never one played lately and never the same artist three times in a row.
//! * Mood on: songs that share a genre or a decade with what has just played are favoured. Songs
//!   without genre tags (the YouTube cache) borrow the tags of the same artist's library songs.

use crate::{
    config::Config,
    db::{Db, RadioSong, Room},
    norm::norm,
    resolve::first_artist,
};
use anyhow::Result;
use std::{
    collections::{HashMap, HashSet},
    hash::{BuildHasher, Hasher},
};

/// How many past plays are remembered, so a song does not come round again soon.
const REMEMBERED: i64 = 60;
/// How many of the latest songs set the mood.
const MOOD_SONGS: usize = 5;
/// The same artist is not picked again within this many songs.
const ARTIST_GAP: usize = 3;

/// A random number in [0, 1), from the operating system's hash seed (no extra dependency).
fn random() -> f64 {
    (std::collections::hash_map::RandomState::new().build_hasher().finish() >> 11) as f64 / (1u64 << 53) as f64
}

fn key(artist: &str, title: &str) -> String {
    format!("{}|{}", norm(artist), norm(title))
}

fn lead_artist(artist: &str) -> String {
    norm(&first_artist(artist))
}

fn genres(tag: Option<&str>) -> Vec<String> {
    tag.map(|g| g.split([';', '/', ',', '\0']).map(|p| p.trim().to_lowercase()).filter(|p| !p.is_empty()).collect()).unwrap_or_default()
}

/// Choose among `pool` by how well each song fits the genres and years of the latest plays.
fn by_mood(songs: &[RadioSong], recent: &[(String, String)], pool: &[usize], rnd: &mut dyn FnMut() -> f64) -> Option<usize> {
    let artists: Vec<String> = songs.iter().map(|s| lead_artist(&s.artist)).collect();
    let mut genre_weight: HashMap<String, f64> = HashMap::new();
    let mut years: Vec<i64> = Vec::new();
    for (artist, title) in recent.iter().take(MOOD_SONGS) {
        let (a, t) = (lead_artist(artist), norm(title));
        let same_artist: Vec<usize> = (0..songs.len()).filter(|&i| artists[i] == a).collect();
        // The exact song if the library has it, else the artist's other songs, counted lighter.
        let exact: Vec<usize> = same_artist.iter().copied().filter(|&i| norm(&songs[i].title) == t).collect();
        let (basis, weight) = if exact.is_empty() { (same_artist, 0.5) } else { (exact, 1.0) };
        for &i in &basis {
            for g in genres(songs[i].genre.as_deref()) {
                *genre_weight.entry(g).or_default() += weight / basis.len() as f64;
            }
            years.extend(songs[i].year);
        }
    }
    years.sort_unstable();
    let middle = years.get(years.len() / 2).copied();
    let score = |s: &RadioSong| {
        let by_genre: f64 = genres(s.genre.as_deref()).iter().map(|g| genre_weight.get(g).copied().unwrap_or(0.0)).sum();
        let by_year = match (s.year, middle) {
            (Some(y), Some(m)) if (y - m).abs() <= 5 => 1.0,
            (Some(y), Some(m)) if (y - m).abs() <= 10 => 0.5,
            _ => 0.0,
        };
        by_genre + by_year
    };
    let scored: Vec<(usize, f64)> = pool.iter().map(|&i| (i, score(&songs[i]))).filter(|(_, s)| *s > 0.0).collect();
    let total: f64 = scored.iter().map(|(_, s)| s * s).sum();
    if total <= 0.0 {
        return None;
    }
    // Weighted by the square of the score, so close fits win most of the time but not always.
    let mut at = rnd() * total;
    for (i, s) in &scored {
        at -= s * s;
        if at <= 0.0 {
            return Some(*i);
        }
    }
    scored.last().map(|(i, _)| *i)
}

/// The index of the next song to play, or `None` if the library has nothing to offer.
/// `recent` is the room's latest plays, newest first.
pub fn pick(songs: &[RadioSong], recent: &[(String, String)], mood: bool, rnd: &mut dyn FnMut() -> f64) -> Option<usize> {
    let played: HashSet<String> = recent.iter().map(|(a, t)| key(a, t)).collect();
    let lately: HashSet<String> = recent.iter().take(ARTIST_GAP).map(|(a, _)| lead_artist(a)).collect();
    let fresh = |s: &RadioSong| !played.contains(&key(&s.artist, &s.title));
    let mut pool: Vec<usize> = (0..songs.len()).filter(|&i| fresh(&songs[i]) && !lately.contains(&lead_artist(&songs[i].artist))).collect();
    if pool.is_empty() {
        pool = (0..songs.len()).filter(|&i| fresh(&songs[i])).collect();
    }
    if pool.is_empty() {
        pool = (0..songs.len()).collect();
    }
    if pool.is_empty() {
        return None;
    }
    if mood && let Some(i) = by_mood(songs, recent, &pool, rnd) {
        return Some(i);
    }
    Some(pool[(rnd() * pool.len() as f64) as usize % pool.len()])
}

/// If the room is idle and Autoplay is on, queue the next library song. True when one was queued.
pub fn fill(db: &Db, cfg: &Config, room: &Room) -> Result<bool> {
    if !room.autoplay || !db.idle(&room.code)? {
        return Ok(false);
    }
    let songs = db.radio_songs(&format!("{}/", cfg.cache.display()))?;
    let recent = db.recent_plays(&room.code, REMEMBERED)?;
    match pick(&songs, &recent, room.mood, &mut random) {
        Some(i) => db.autoplay_insert(&room.code, &songs[i]),
        None => Ok(false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn song(artist: &str, title: &str, genre: Option<&str>, year: Option<i64>) -> RadioSong {
        RadioSong {
            path: format!("/m/{artist}/{title}.flac"),
            artist: artist.into(),
            title: title.into(),
            album: None,
            duration: 200.0,
            genre: genre.map(String::from),
            year,
        }
    }

    fn played(artist: &str, title: &str) -> (String, String) {
        (artist.into(), title.into())
    }

    #[test]
    fn nothing_recent_comes_round_again_and_artists_do_not_repeat() {
        let songs =
            vec![song("A", "One", None, None), song("A", "Two", None, None), song("B", "Three", None, None), song("C", "Four", None, None)];
        let recent = vec![played("A", "One")];
        for r in [0.0, 0.4, 0.99] {
            let i = pick(&songs, &recent, false, &mut || r).unwrap();
            assert!(["Three", "Four"].contains(&songs[i].title.as_str()), "picked {}", songs[i].title);
        }
        // Everything has been played: it still picks something instead of going silent.
        let all: Vec<_> = songs.iter().map(|s| played(&s.artist, &s.title)).collect();
        assert!(pick(&songs, &all, false, &mut || 0.5).is_some());
        assert!(pick(&[], &[], false, &mut || 0.5).is_none());
    }

    #[test]
    fn keeping_the_mood_favours_the_same_genre_and_decade() {
        let songs = vec![
            song("Rocker", "Loud", Some("Rock"), Some(1998)),
            song("Other Rocker", "Louder", Some("Rock"), Some(2001)),
            song("Smooth", "Jazzy", Some("Jazz"), Some(1960)),
            song("Popper", "Hit", Some("Pop"), Some(2019)),
        ];
        // The room has just played the first rock song, so rock from that time is what fits.
        let recent = vec![played("Rocker", "Loud")];
        for r in [0.0, 0.5, 0.99] {
            let i = pick(&songs, &recent, true, &mut || r).unwrap();
            assert_eq!(songs[i].title, "Louder");
        }
        // Mood off: the pool is every song but the played one, so jazz can come up.
        assert_eq!(pick(&songs, &recent, false, &mut || 0.34).map(|i| songs[i].title.as_str()), Some("Jazzy"));
    }

    #[test]
    fn a_song_without_tags_borrows_its_artists_other_songs_and_no_clues_means_random() {
        let songs = vec![
            song("Band", "Library Song", Some("Metal"), Some(2005)),
            song("Another", "Heavy", Some("Metal"), Some(2006)),
            song("Chill", "Soft", Some("Ambient"), Some(1985)),
        ];
        // The room played "Band - Cache Song" (not in the library): Band's genre is metal.
        let recent = vec![played("Band", "Cache Song")];
        for r in [0.0, 0.7] {
            let i = pick(&songs, &recent, true, &mut || r).unwrap();
            assert_eq!(songs[i].title, "Heavy", "Band is on cooldown, so the other metal song fits");
        }
        // Nothing known about the latest song: mood has no clue, so it simply picks.
        assert!(pick(&songs, &[played("Unknown", "X")], true, &mut || 0.5).is_some());
    }
}
