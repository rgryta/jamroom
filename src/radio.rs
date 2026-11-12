//! Autoplay: when a room's queue runs dry and Autoplay is on, pick the next song from the music
//! library, so the room keeps playing like a radio. Library songs are local and full quality, so
//! the song starts at once and nothing is downloaded.
//!
//! * With nothing to follow: a random song, never one played lately and never the same artist
//!   within three songs.
//! * With "keep the mood": songs that match what the last five songs were like, on whichever of
//!   genre, era, artist, language and tempo are switched on. How strictly is the "how close"
//!   setting: loose mixes in surprises, tight stays near the best fits.

use crate::{
    config::Config,
    db::{Close, Db, Mood, RadioSong, Room},
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
/// Without the artist option, the same artist is not picked again within this many songs.
const ARTIST_GAP: usize = 3;
/// Loose mood: how often the pick ignores the mood altogether.
const LOOSE_SURPRISE: f64 = 0.3;
/// Tight mood: the pick is made among this many best fits.
const TIGHT_TOP: usize = 3;
/// Genres that are talking, not music; the radio never picks them.
const SPOKEN: &[&str] =
    &["comedy", "audiobook", "audio book", "spoken word", "spoken", "speech", "podcast", "stand-up", "standup", "interview"];

/// True for songs the radio may play.
pub fn playable(s: &RadioSong) -> bool {
    !genres(s.genre.as_deref()).iter().any(|g| SPOKEN.contains(&g.as_str()))
}

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

/// A language tag as a three-letter code ("English", "eng" and "en" are all "eng").
fn language(tag: Option<&str>) -> Option<String> {
    let t = tag?.trim().to_lowercase();
    let code = match t.as_str() {
        "english" | "en" => "eng",
        "polish" | "pl" => "pol",
        "german" | "de" | "ger" => "deu",
        "french" | "fr" | "fre" => "fra",
        "spanish" | "es" => "spa",
        "czech" | "cs" | "cze" => "ces",
        "russian" | "ru" => "rus",
        "italian" | "it" => "ita",
        "" | "xxx" | "und" | "zxx" | "mul" => return None,
        other => other,
    };
    Some(code.chars().take(3).collect())
}

fn language_name(code: &str) -> String {
    match code {
        "eng" => "English",
        "pol" => "Polish",
        "deu" => "German",
        "fra" => "French",
        "spa" => "Spanish",
        "ces" => "Czech",
        "rus" => "Russian",
        "ita" => "Italian",
        other => other,
    }
    .to_string()
}

fn capitalise(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
}

/// What the latest songs were like, on the dimensions that are switched on.
#[derive(Default)]
struct Profile {
    genre_weight: HashMap<String, f64>,
    years: Vec<i64>,
    language_weight: HashMap<String, f64>,
    bpms: Vec<f64>,
    /// The artist playing now (or last), as shown and as matched.
    latest_artist: String,
    latest_lead: String,
}

fn median<T: Copy + PartialOrd>(mut v: Vec<T>) -> Option<T> {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    v.get(v.len() / 2).copied()
}

fn profile(songs: &[RadioSong], recent: &[(String, String)]) -> Profile {
    let artists: Vec<String> = songs.iter().map(|s| lead_artist(&s.artist)).collect();
    let mut p = Profile::default();
    for (n, (artist, title)) in recent.iter().take(MOOD_SONGS).enumerate() {
        let (a, t) = (lead_artist(artist), norm(title));
        if n == 0 {
            p.latest_artist = first_artist(artist);
            p.latest_lead = a.clone();
        }
        let same_artist: Vec<usize> = (0..songs.len()).filter(|&i| artists[i] == a).collect();
        // The exact song if the index has it, else the artist's other songs, counted lighter.
        let exact: Vec<usize> = same_artist.iter().copied().filter(|&i| norm(&songs[i].title) == t).collect();
        let (basis, weight) = if exact.is_empty() { (same_artist, 0.5) } else { (exact, 1.0) };
        for &i in &basis {
            let s = &songs[i];
            for g in genres(s.genre.as_deref()) {
                *p.genre_weight.entry(g).or_default() += weight / basis.len() as f64;
            }
            if let Some(l) = language(s.language.as_deref()) {
                *p.language_weight.entry(l).or_default() += weight / basis.len() as f64;
            }
            p.years.extend(s.year);
            // Tempo is a property of one recording, so only the exact song counts.
            if weight == 1.0 {
                p.bpms.extend(s.bpm.filter(|b| *b > 0.0));
            }
        }
    }
    p
}

/// "Rock · 1990s · English · ~118 BPM": what the radio is following right now.
fn describe(mood: &Mood, p: &Profile) -> String {
    let mut parts: Vec<String> = Vec::new();
    if mood.genre {
        let mut top: Vec<_> = p.genre_weight.iter().collect();
        top.sort_by(|a, b| b.1.total_cmp(a.1).then(a.0.cmp(b.0)));
        let named: Vec<String> = top.iter().take(2).map(|(g, _)| capitalise(g)).collect();
        if !named.is_empty() {
            parts.push(named.join(" / "));
        }
    }
    if mood.era
        && let Some(y) = median(p.years.clone())
    {
        parts.push(format!("{}0s", y / 10));
    }
    if mood.language
        && let Some((l, _)) = p.language_weight.iter().max_by(|a, b| a.1.total_cmp(b.1).then(b.0.cmp(a.0)))
    {
        parts.push(language_name(l));
    }
    if mood.tempo
        && let Some(b) = median(p.bpms.clone())
    {
        parts.push(format!("~{} BPM", b.round()));
    }
    if mood.artist && !p.latest_artist.is_empty() {
        parts.push(p.latest_artist.clone());
    }
    parts.join(" \u{b7} ")
}

fn is_latest_artist(s: &RadioSong, p: &Profile) -> bool {
    !p.latest_lead.is_empty() && lead_artist(&s.artist) == p.latest_lead
}

/// How well a song fits the profile, on the dimensions that are switched on. 0 means no fit.
fn fit(s: &RadioSong, mood: &Mood, p: &Profile) -> f64 {
    let mut score = 0.0;
    if mood.genre {
        score += genres(s.genre.as_deref()).iter().map(|g| p.genre_weight.get(g).copied().unwrap_or(0.0)).sum::<f64>();
    }
    if mood.era
        && let (Some(y), Some(m)) = (s.year, median(p.years.clone()))
    {
        score += match (y - m).abs() {
            0..=5 => 1.0,
            6..=10 => 0.5,
            _ => 0.0,
        };
    }
    if mood.language
        && let Some(l) = language(s.language.as_deref())
    {
        let total: f64 = p.language_weight.values().sum();
        if total > 0.0 {
            score += p.language_weight.get(&l).copied().unwrap_or(0.0) / total;
        }
    }
    if mood.tempo
        && let (Some(b), Some(m)) = (s.bpm.filter(|b| *b > 0.0), median(p.bpms.clone()))
    {
        let off = (b - m).abs() / m;
        score += if off <= 0.08 {
            1.0
        } else if off <= 0.15 {
            0.5
        } else {
            0.0
        };
    }
    if mood.artist && is_latest_artist(s, p) {
        score += 2.0;
    }
    score
}

/// The index of the next song to play and a short description of what it follows, or `None` if
/// the library has nothing to offer. `songs` is everything indexed (the download cache too, which
/// only helps to learn what recent songs were like); only library files are played. `recent` is
/// the room's latest plays, newest first.
pub fn pick(
    songs: &[RadioSong],
    recent: &[(String, String)],
    mood: &Mood,
    close: Close,
    rnd: &mut dyn FnMut() -> f64,
) -> Option<(usize, String)> {
    let played: HashSet<String> = recent.iter().map(|(a, t)| key(a, t)).collect();
    let lately: HashSet<String> =
        if mood.artist { HashSet::new() } else { recent.iter().take(ARTIST_GAP).map(|(a, _)| lead_artist(a)).collect() };
    let library: Vec<usize> = (0..songs.len()).filter(|&i| !songs[i].cached).collect();
    let fresh = |i: &usize| !played.contains(&key(&songs[*i].artist, &songs[*i].title));
    let mut pool: Vec<usize> = library.iter().copied().filter(|i| fresh(i) && !lately.contains(&lead_artist(&songs[*i].artist))).collect();
    if pool.is_empty() {
        pool = library.iter().copied().filter(fresh).collect();
    }
    if pool.is_empty() {
        pool = library;
    }
    if pool.is_empty() {
        return None;
    }
    let randomly = |rnd: &mut dyn FnMut() -> f64| pool[(rnd() * pool.len() as f64) as usize % pool.len()];
    if !mood.any() {
        return Some((randomly(rnd), String::new()));
    }
    let p = profile(songs, recent);
    let note = describe(mood, &p);
    let mut scored: Vec<(usize, f64)> = pool.iter().map(|&i| (i, fit(&songs[i], mood, &p))).filter(|(_, s)| *s > 0.0).collect();
    if scored.is_empty() || (close == Close::Loose && rnd() < LOOSE_SURPRISE) {
        return Some((randomly(rnd), note));
    }
    // The artist option is not a nudge: while the current artist has a song left, it is the pick.
    if mood.artist && scored.iter().any(|(i, _)| is_latest_artist(&songs[*i], &p)) {
        scored.retain(|(i, _)| is_latest_artist(&songs[*i], &p));
    }
    scored.sort_by(|a, b| b.1.total_cmp(&a.1));
    let chosen = match close {
        // The best few, evenly: it stays near the mood without playing the same top song every time.
        Close::Tight => {
            let top = &scored[..scored.len().min(TIGHT_TOP)];
            top[(rnd() * top.len() as f64) as usize % top.len()].0
        }
        // Weighted by the score: any fit can win, the better fits more often. Balanced squares the
        // score, which favours the close fits; loose uses it as is.
        _ => {
            let weight = |s: f64| if close == Close::Balanced { s * s } else { s };
            let total: f64 = scored.iter().map(|(_, s)| weight(*s)).sum();
            let mut at = rnd() * total;
            let mut last = scored[0].0;
            for (i, s) in &scored {
                last = *i;
                at -= weight(*s);
                if at <= 0.0 {
                    break;
                }
            }
            last
        }
    };
    Some((chosen, note))
}

/// If the room is idle and Autoplay is on, queue the next library song. True when one was queued.
pub fn fill(db: &Db, cfg: &Config, room: &Room) -> Result<bool> {
    if !room.autoplay || !db.idle(&room.code)? {
        return Ok(false);
    }
    let mut songs = db.radio_songs(&format!("{}/", cfg.cache.display()))?;
    songs.retain(|s| s.cached || playable(s));
    let recent = db.recent_plays(&room.code, REMEMBERED)?;
    let Some((i, note)) = pick(&songs, &recent, &room.mood, room.close, &mut random) else { return Ok(false) };
    db.set_mood_note(&room.code, &note)?;
    db.autoplay_insert(&room.code, &songs[i])
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
            ..Default::default()
        }
    }

    fn played(artist: &str, title: &str) -> (String, String) {
        (artist.into(), title.into())
    }

    fn only(f: impl Fn(&mut Mood)) -> Mood {
        let mut m = Mood::default();
        f(&mut m);
        m
    }

    fn title(songs: &[RadioSong], r: Option<(usize, String)>) -> &str {
        songs[r.unwrap().0].title.as_str()
    }

    #[test]
    fn talking_is_not_music() {
        assert!(playable(&song("A", "One", Some("Rock"), None)));
        assert!(playable(&song("A", "Two", None, None)));
        assert!(!playable(&song("A", "Skit", Some("Comedy"), None)));
        assert!(!playable(&song("A", "Chapter 1", Some("Audiobook"), None)));
        assert!(!playable(&song("A", "Mixed", Some("Rock; Comedy"), None)));
    }

    #[test]
    fn with_nothing_to_follow_it_is_random_without_repeats_or_artist_streaks() {
        let songs =
            vec![song("A", "One", None, None), song("A", "Two", None, None), song("B", "Three", None, None), song("C", "Four", None, None)];
        let recent = vec![played("A", "One")];
        let none = Mood::default();
        for r in [0.0, 0.4, 0.99] {
            let got = pick(&songs, &recent, &none, Close::Balanced, &mut || r).unwrap();
            assert!(["Three", "Four"].contains(&songs[got.0].title.as_str()), "picked {}", songs[got.0].title);
            assert_eq!(got.1, "", "a random pick follows nothing");
        }
        // Different random numbers reach different songs: it really is a random pick.
        let reached: HashSet<usize> =
            [0.0, 0.6].iter().map(|r| pick(&songs, &recent, &none, Close::Balanced, &mut || *r).unwrap().0).collect();
        assert_eq!(reached.len(), 2);
        // Everything has been played: it still picks something instead of going silent.
        let all: Vec<_> = songs.iter().map(|s| played(&s.artist, &s.title)).collect();
        assert!(pick(&songs, &all, &none, Close::Balanced, &mut || 0.5).is_some());
        assert!(pick(&[], &[], &none, Close::Balanced, &mut || 0.5).is_none());
    }

    #[test]
    fn genre_and_era_each_steer_the_pick_on_their_own() {
        let songs = vec![
            song("Rocker", "Loud", Some("Rock"), Some(1998)),
            song("Old Rocker", "Classic", Some("Rock"), Some(1972)),
            song("Newer Jazz", "Smooth", Some("Jazz"), Some(1999)),
            song("Popper", "Hit", Some("Pop"), Some(2019)),
        ];
        let recent = vec![played("Rocker", "Loud")];
        for r in [0.0, 0.5, 0.99] {
            // Genre only: the other rock song, whatever its decade.
            let g = pick(&songs, &recent, &only(|m| m.genre = true), Close::Balanced, &mut || r);
            assert_eq!(title(&songs, g), "Classic");
            // Era only: the song from the same years, whatever its genre.
            let e = pick(&songs, &recent, &only(|m| m.era = true), Close::Balanced, &mut || r);
            assert_eq!(title(&songs, e), "Smooth");
        }
        let both = only(|m| {
            m.genre = true;
            m.era = true;
        });
        assert_eq!(pick(&songs, &recent, &both, Close::Balanced, &mut || 0.0).unwrap().1, "Rock \u{b7} 1990s");
    }

    #[test]
    fn artist_language_and_tempo_steer_too() {
        let mut songs = vec![
            RadioSong { language: Some("eng".into()), bpm: Some(120.0), ..song("Band", "Played", Some("Rock"), Some(2000)) },
            RadioSong { language: Some("pol".into()), bpm: Some(121.0), ..song("Polish", "Fast Polish", Some("Rock"), Some(2000)) },
            RadioSong { language: Some("english".into()), bpm: Some(70.0), ..song("Slow", "Slow English", Some("Rock"), Some(2000)) },
            RadioSong { language: Some("eng".into()), bpm: Some(122.0), ..song("Quick", "Fast English", Some("Rock"), Some(2000)) },
        ];
        let recent = vec![played("Band", "Played")];
        // Language: English ones qualify (the Polish one does not), whatever the pace.
        let l = pick(&songs, &recent, &only(|m| m.language = true), Close::Tight, &mut || 0.0).unwrap();
        assert!(["Slow English", "Fast English"].contains(&songs[l.0].title.as_str()));
        assert_eq!(l.1, "English");
        // Tempo: only the songs at a similar pace; the 70 BPM one is out.
        for r in [0.0, 0.5, 0.99] {
            let t = pick(&songs, &recent, &only(|m| m.tempo = true), Close::Balanced, &mut || r).unwrap();
            assert!(["Fast Polish", "Fast English"].contains(&songs[t.0].title.as_str()));
            assert_eq!(t.1, "~120 BPM");
        }
        // Tempo and language together: the fast English song.
        let both = only(|m| {
            m.tempo = true;
            m.language = true;
        });
        assert_eq!(title(&songs, pick(&songs, &recent, &both, Close::Tight, &mut || 0.0)), "Fast English");
        // Artist: more from the same artist, which lifts the no-repeat gap.
        songs.push(song("Band", "Another By Band", Some("Pop"), Some(1990)));
        let a = pick(&songs, &recent, &only(|m| m.artist = true), Close::Balanced, &mut || 0.5);
        assert_eq!(title(&songs, a), "Another By Band");
    }

    #[test]
    fn the_artist_option_follows_the_artist_playing_now_over_everything_else() {
        let songs = vec![
            song("Older", "Before", Some("Rock"), Some(1990)),
            song("Older", "Before Too", Some("Rock"), Some(1990)),
            song("Rock Band", "Loud", Some("Rock"), Some(1990)),
            song("Rock Band", "Louder", Some("Pop"), Some(2010)),
            song("Other", "Same Genre", Some("Rock"), Some(1990)),
        ];
        // Newest first: Rock Band is playing now, Older was a few songs ago.
        let recent = vec![played("Rock Band", "Loud"), played("Other", "Mid"), played("Older", "Before")];
        let mood = only(|m| {
            m.artist = true;
            m.genre = true;
        });
        for close in [Close::Balanced, Close::Tight] {
            for r in [0.0, 0.5, 0.99] {
                let got = pick(&songs, &recent, &mood, close, &mut || r);
                assert_eq!(title(&songs, got), "Louder", "{close:?} at {r}");
            }
        }
    }

    #[test]
    fn a_song_without_tags_borrows_its_artists_other_songs_and_no_clues_means_random() {
        let songs = vec![
            song("Band", "Library Song", Some("Metal"), Some(2005)),
            song("Another", "Heavy", Some("Metal"), Some(2006)),
            song("Chill", "Soft", Some("Ambient"), Some(1985)),
        ];
        let mood = only(|m| m.genre = true);
        // The room played "Band - Cache Song" (not in the index): Band's genre is metal.
        let recent = vec![played("Band", "Cache Song")];
        for r in [0.0, 0.7] {
            assert_eq!(title(&songs, pick(&songs, &recent, &mood, Close::Balanced, &mut || r)), "Heavy");
        }
        // Nothing known about the latest song: the mood has no clue, so it simply picks.
        assert!(pick(&songs, &[played("Unknown", "X")], &mood, Close::Balanced, &mut || 0.5).is_some());
    }

    #[test]
    fn how_close_decides_how_adventurous_the_pick_is() {
        let songs = vec![
            song("Anchor", "Played", Some("Rock"), None),
            song("A", "Perfect", Some("Rock"), None),
            song("B", "Good", Some("Rock/Pop"), None),
            song("C", "Poor", Some("Pop"), None),
            song("D", "Other", Some("Jazz"), None),
        ];
        let recent = vec![played("Anchor", "Played")];
        let genre = only(|m| m.genre = true);
        // Tight: always one of the best fits (a pure rock song), never a poor one.
        for r in [0.0, 0.5, 0.99] {
            assert!(["Perfect", "Good"].contains(&title(&songs, pick(&songs, &recent, &genre, Close::Tight, &mut || r))));
        }
        // Loose: sometimes ignores the mood (the surprise draw is below 0.3), so a song with no fit can come up.
        let mut draws = vec![0.1, 0.99].into_iter();
        let surprise = pick(&songs, &recent, &genre, Close::Loose, &mut || draws.next().unwrap_or(0.99));
        assert_eq!(title(&songs, surprise), "Other");
        // Loose, with no surprise: still follows the mood.
        let mut draws = vec![0.9, 0.0].into_iter();
        let follows = pick(&songs, &recent, &genre, Close::Loose, &mut || draws.next().unwrap_or(0.0));
        assert!(["Perfect", "Good"].contains(&title(&songs, follows)));
    }

    #[test]
    fn the_cache_teaches_the_mood_but_is_never_played() {
        let mut cached = song("Band", "From YouTube", Some("Metal"), Some(2005));
        cached.cached = true;
        let songs = vec![cached, song("Another", "Heavy", Some("Metal"), Some(2006)), song("Chill", "Soft", Some("Ambient"), Some(1985))];
        let recent = vec![played("Band", "From YouTube")];
        for r in [0.0, 0.5, 0.99] {
            assert_eq!(title(&songs, pick(&songs, &recent, &only(|m| m.genre = true), Close::Balanced, &mut || r)), "Heavy");
        }
        let none = pick(&songs[..1], &recent, &Mood::default(), Close::Balanced, &mut || 0.0);
        assert!(none.is_none(), "a cache-only index offers nothing to play");
    }
}
