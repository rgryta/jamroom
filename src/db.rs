//! SQLite storage: rooms, the play queue and the library index.

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    path::Path,
    sync::{Mutex, MutexGuard},
    time::{SystemTime, UNIX_EPOCH},
};

pub fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// A playing song's clock: its id, when it started (epoch ms) and where it is if paused.
type Clock = (i64, Option<i64>, Option<f64>);

pub fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS rooms(
    code TEXT PRIMARY KEY, name TEXT NOT NULL, created_at INTEGER NOT NULL, revoked INTEGER NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS tracks(
    id INTEGER PRIMARY KEY AUTOINCREMENT, room TEXT NOT NULL, status TEXT NOT NULL,
    artist TEXT NOT NULL, dir_artist TEXT NOT NULL, title TEXT NOT NULL, album TEXT,
    duration REAL, cover TEXT, src TEXT, query TEXT, file TEXT, error TEXT,
    added_by TEXT, added_at INTEGER NOT NULL, started_at INTEGER, fetched_at INTEGER);
CREATE INDEX IF NOT EXISTS tracks_room ON tracks(room, status);
CREATE TABLE IF NOT EXISTS upgrades(
    key TEXT PRIMARY KEY, artist TEXT NOT NULL, title TEXT NOT NULL, duration REAL, cached TEXT NOT NULL,
    status TEXT NOT NULL, peer TEXT NOT NULL DEFAULT '', remote TEXT NOT NULL DEFAULT '', size INTEGER NOT NULL DEFAULT 0,
    note TEXT NOT NULL DEFAULT '', updated_at INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS library(
    path TEXT PRIMARY KEY, mtime INTEGER NOT NULL, artist TEXT NOT NULL, title TEXT NOT NULL,
    tn TEXT NOT NULL, duration REAL);
CREATE INDEX IF NOT EXISTS library_tn ON library(tn);
";

pub struct Db {
    conn: Mutex<Connection>,
}

/// What Autoplay follows when it picks the next song. All off means a random song.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Mood {
    pub genre: bool,
    pub era: bool,
    pub artist: bool,
    pub language: bool,
    pub tempo: bool,
}

impl Mood {
    pub fn any(&self) -> bool {
        self.genre || self.era || self.artist || self.language || self.tempo
    }

    fn from_db(s: &str) -> Mood {
        let has = |k: &str| s.split(',').any(|p| p == k);
        Mood { genre: has("genre"), era: has("era"), artist: has("artist"), language: has("language"), tempo: has("tempo") }
    }

    fn to_db(&self) -> String {
        [("genre", self.genre), ("era", self.era), ("artist", self.artist), ("language", self.language), ("tempo", self.tempo)]
            .iter()
            .filter(|(_, on)| *on)
            .map(|(k, _)| *k)
            .collect::<Vec<_>>()
            .join(",")
    }
}

/// A change to the mood: only the named options are touched.
#[derive(Debug, Default, Deserialize)]
pub struct MoodPatch {
    pub genre: Option<bool>,
    pub era: Option<bool>,
    pub artist: Option<bool>,
    pub language: Option<bool>,
    pub tempo: Option<bool>,
}

/// How closely Autoplay follows the mood: loose mixes in surprises, tight stays near the best fits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Close {
    Loose,
    Balanced,
    Tight,
}

impl Close {
    fn from_db(n: i64) -> Close {
        match n {
            0 => Close::Loose,
            2 => Close::Tight,
            _ => Close::Balanced,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Room {
    pub code: String,
    pub name: String,
    pub revoked: bool,
    pub created_at: i64,
    /// When the queue runs dry, keep playing songs from the library.
    pub autoplay: bool,
    /// What Autoplay follows; nothing means a random song.
    pub mood: Mood,
    pub close: Close,
    /// What the last Autoplay pick followed, for example "Rock · 1990s".
    pub mood_note: String,
}

/// A queue entry as shown to clients (no file paths or source URLs).
#[derive(Debug, Clone, Serialize)]
pub struct Track {
    pub id: i64,
    pub status: String,
    pub artist: String,
    pub title: String,
    pub album: Option<String>,
    pub duration: Option<f64>,
    pub cover: Option<String>,
    pub added_by: Option<String>,
    pub error: Option<String>,
    /// Seconds into the song that every player should be at right now. Only the playing song has one.
    pub position: Option<f64>,
    /// True while the room is paused. Only meaningful for the playing song.
    pub paused: bool,
}

/// What the resolver learned about a requested track.
#[derive(Debug, Clone, Default)]
pub struct NewTrack {
    pub artist: String,
    pub dir_artist: String,
    pub title: String,
    pub album: Option<String>,
    pub duration: Option<f64>,
    pub cover: Option<String>,
    /// A direct YouTube/SoundCloud URL, when the request already named one.
    pub src: Option<String>,
    /// Free-text search to find a source when no URL or artist is known.
    pub query: Option<String>,
    pub added_by: Option<String>,
}

/// A queued track plus everything the worker needs to fetch it.
#[derive(Debug, Clone)]
pub struct Job {
    pub id: i64,
    pub item: NewTrack,
}

/// A library file: path, artist, length and title.
pub type LibHit = (String, String, Option<f64>, String);

/// A song whose file is in the cache: artist, title, length and that file.
pub type CachedSong = (String, String, Option<f64>, String);

/// The hunt for a better copy of one cached song. `status` is searching, downloading, done, none or failed.
#[derive(Debug, Clone, Default)]
pub struct Upgrade {
    pub key: String,
    pub artist: String,
    pub title: String,
    pub duration: Option<f64>,
    pub cached: String,
    pub status: String,
    pub peer: String,
    pub remote: String,
    pub size: i64,
    pub note: String,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Default)]
pub struct LibEntry {
    pub path: String,
    pub mtime: i64,
    pub artist: String,
    pub title: String,
    pub tn: String,
    pub duration: Option<f64>,
    pub album: Option<String>,
    pub genre: Option<String>,
    pub year: Option<i64>,
    /// Beats per minute, if the file's tag says so.
    pub bpm: Option<f64>,
    pub language: Option<String>,
}

/// A library song the radio can play.
#[derive(Debug, Clone, Default)]
pub struct RadioSong {
    pub path: String,
    pub artist: String,
    pub title: String,
    pub album: Option<String>,
    pub duration: f64,
    pub genre: Option<String>,
    pub year: Option<i64>,
    pub bpm: Option<f64>,
    pub language: Option<String>,
    /// In the download cache rather than the music library: it helps to learn the mood, but is not played.
    pub cached: bool,
}

#[derive(Debug, Serialize)]
pub struct Snapshot {
    pub now: Option<Track>,
    pub queue: Vec<Track>,
    pub recent: Vec<Track>,
}

fn room_from(r: &Row) -> rusqlite::Result<Room> {
    Ok(Room {
        code: r.get(0)?,
        name: r.get(1)?,
        revoked: r.get::<_, i64>(2)? != 0,
        created_at: r.get(3)?,
        autoplay: r.get::<_, i64>(4)? != 0,
        mood: Mood::from_db(&r.get::<_, String>(5)?),
        close: Close::from_db(r.get(6)?),
        mood_note: r.get(7)?,
    })
}

const TRACK_COLS: &str = "id, status, artist, title, album, duration, cover, added_by, error, started_ms, paused_pos";

fn track_from(r: &Row) -> rusqlite::Result<Track> {
    let status: String = r.get(1)?;
    let (started_ms, paused_pos): (Option<i64>, Option<f64>) = (r.get(9)?, r.get(10)?);
    let playing = status == "playing";
    // The playback clock: a paused song stays where it stopped, a running one moves with time.
    let position = playing.then(|| paused_pos.unwrap_or_else(|| started_ms.map_or(0.0, |s| ((now_ms() - s) as f64 / 1000.0).max(0.0))));
    Ok(Track {
        id: r.get(0)?,
        status,
        artist: r.get(2)?,
        title: r.get(3)?,
        album: r.get(4)?,
        duration: r.get(5)?,
        cover: r.get(6)?,
        added_by: r.get(7)?,
        error: r.get(8)?,
        position,
        paused: playing && paused_pos.is_some(),
    })
}

impl Db {
    pub fn open(path: &Path) -> Result<Db> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        Self::init(conn)
    }

    #[cfg(test)]
    pub fn memory() -> Result<Db> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Db> {
        conn.execute_batch(SCHEMA)?;
        // Columns added after the first release; older databases get them here.
        let added = [
            ("tracks", "started_ms", "INTEGER"),
            ("tracks", "paused_pos", "REAL"),
            ("rooms", "autoplay", "INTEGER NOT NULL DEFAULT 0"),
            ("rooms", "mood", "INTEGER NOT NULL DEFAULT 0"),
            ("rooms", "mood_opts", "TEXT NOT NULL DEFAULT ''"),
            ("rooms", "mood_close", "INTEGER NOT NULL DEFAULT 1"),
            ("rooms", "mood_note", "TEXT NOT NULL DEFAULT ''"),
            ("library", "bpm", "REAL"),
            ("library", "language", "TEXT"),
            ("library", "album", "TEXT"),
            ("library", "genre", "TEXT"),
            ("library", "year", "INTEGER"),
            // Below 2 = indexed before genre, year, tempo and language were kept, so the next scan reads the file again.
            ("library", "v", "INTEGER NOT NULL DEFAULT 0"),
        ];
        for (table, col, decl) in added {
            let has: i64 =
                conn.query_row(&format!("SELECT COUNT(*) FROM pragma_table_info('{table}') WHERE name = ?1"), [col], |r| r.get(0))?;
            if has == 0 {
                conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {col} {decl}"))?;
            }
        }
        // The first Autoplay had one "keep the mood" switch; it meant genre and era.
        conn.execute("UPDATE rooms SET mood_opts = 'genre,era', mood = 0 WHERE mood = 1 AND mood_opts = ''", [])?;
        Ok(Db { conn: Mutex::new(conn) })
    }

    fn c(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    // ---- rooms ----

    pub fn create_room(&self, code: &str, name: &str) -> Result<()> {
        self.c().execute("INSERT INTO rooms(code, name, created_at) VALUES(?1, ?2, ?3)", params![code, name, now()])?;
        Ok(())
    }

    pub fn room(&self, code: &str) -> Result<Option<Room>> {
        Ok(self
            .c()
            .query_row(
                "SELECT code, name, revoked, created_at, autoplay, mood_opts, mood_close, mood_note FROM rooms WHERE code = ?1",
                [code],
                room_from,
            )
            .optional()?)
    }

    pub fn rooms(&self) -> Result<Vec<Room>> {
        let c = self.c();
        let mut st =
            c.prepare("SELECT code, name, revoked, created_at, autoplay, mood_opts, mood_close, mood_note FROM rooms ORDER BY created_at")?;
        let rows = st.query_map([], room_from)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Turn Autoplay and the mood on or off for a room. `None` leaves a setting as it is.
    pub fn set_room_flags(&self, code: &str, autoplay: Option<bool>, mood: Option<&MoodPatch>, close: Option<Close>) -> Result<()> {
        let c = self.c();
        if let Some(a) = autoplay {
            c.execute("UPDATE rooms SET autoplay = ?2 WHERE code = ?1", params![code, a])?;
        }
        if let Some(p) = mood {
            let current: String = c.query_row("SELECT mood_opts FROM rooms WHERE code = ?1", [code], |r| r.get(0))?;
            let mut m = Mood::from_db(&current);
            m.genre = p.genre.unwrap_or(m.genre);
            m.era = p.era.unwrap_or(m.era);
            m.artist = p.artist.unwrap_or(m.artist);
            m.language = p.language.unwrap_or(m.language);
            m.tempo = p.tempo.unwrap_or(m.tempo);
            c.execute("UPDATE rooms SET mood_opts = ?2 WHERE code = ?1", params![code, m.to_db()])?;
        }
        if let Some(close) = close {
            let n = match close {
                Close::Loose => 0,
                Close::Balanced => 1,
                Close::Tight => 2,
            };
            c.execute("UPDATE rooms SET mood_close = ?2 WHERE code = ?1", params![code, n])?;
        }
        Ok(())
    }

    /// Remember what the latest Autoplay pick followed, to show in the room.
    pub fn set_mood_note(&self, code: &str, note: &str) -> Result<()> {
        self.c().execute("UPDATE rooms SET mood_note = ?2 WHERE code = ?1", params![code, note])?;
        Ok(())
    }

    pub fn revoke_room(&self, code: &str) -> Result<bool> {
        Ok(self.c().execute("UPDATE rooms SET revoked = 1 WHERE code = ?1 AND revoked = 0", [code])? > 0)
    }

    // ---- queue ----

    pub fn add_track(&self, room: &str, t: &NewTrack) -> Result<i64> {
        let c = self.c();
        c.execute(
            "INSERT INTO tracks(room, status, artist, dir_artist, title, album, duration, cover, src, query, added_by, added_at)
             VALUES(?1, 'queued', ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![room, t.artist, t.dir_artist, t.title, t.album, t.duration, t.cover, t.src, t.query, t.added_by, now()],
        )?;
        Ok(c.last_insert_rowid())
    }

    /// True when nothing in the room is playing or waiting.
    pub fn idle(&self, room: &str) -> Result<bool> {
        let n: i64 = self.c().query_row(
            "SELECT COUNT(*) FROM tracks WHERE room = ?1 AND status IN ('queued','working','ready','playing')",
            [room],
            |r| r.get(0),
        )?;
        Ok(n == 0)
    }

    /// The room's latest plays, newest first: (artist, title).
    pub fn recent_plays(&self, room: &str, n: i64) -> Result<Vec<(String, String)>> {
        let c = self.c();
        let mut st =
            c.prepare("SELECT artist, title FROM tracks WHERE room = ?1 AND status IN ('playing','played') ORDER BY id DESC LIMIT ?2")?;
        let rows = st.query_map(params![room, n], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Songs the radio may play: library files (not the download cache) of a sensible length.
    pub fn radio_songs(&self, cache_prefix: &str) -> Result<Vec<RadioSong>> {
        let c = self.c();
        let mut st = c.prepare(
            "SELECT path, artist, title, album, duration, genre, year, bpm, language, substr(path, 1, length(?1)) = ?1
             FROM library WHERE duration BETWEEN 60 AND 900",
        )?;
        let rows = st.query_map([cache_prefix], |r| {
            Ok(RadioSong {
                path: r.get(0)?,
                artist: r.get(1)?,
                title: r.get(2)?,
                album: r.get(3)?,
                duration: r.get(4)?,
                genre: r.get(5)?,
                year: r.get(6)?,
                bpm: r.get(7)?,
                language: r.get(8)?,
                cached: r.get(9)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Songs still without a tempo (0 means "looked, no clear beat"), cached ones first.
    pub fn bpm_todo(&self, cache_prefix: &str, limit: i64) -> Result<Vec<(String, f64)>> {
        let c = self.c();
        let mut st = c.prepare(
            "SELECT path, duration FROM library WHERE bpm IS NULL AND duration BETWEEN 60 AND 900
             ORDER BY (substr(path, 1, length(?1)) = ?1) DESC, rowid LIMIT ?2",
        )?;
        let rows = st.query_map(params![cache_prefix, limit], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn set_bpm(&self, path: &str, bpm: f64) -> Result<()> {
        self.c().execute("UPDATE library SET bpm = ?2 WHERE path = ?1", params![path, bpm])?;
        Ok(())
    }

    /// How many library songs have a tempo, and how many could: (known, total).
    pub fn tempo_stats(&self, cache_prefix: &str) -> Result<(i64, i64)> {
        Ok(self.c().query_row(
            "SELECT COALESCE(SUM(bpm > 0), 0), COUNT(*) FROM library
             WHERE duration BETWEEN 60 AND 900 AND substr(path, 1, length(?1)) != ?1",
            [cache_prefix],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?)
    }

    /// Queue a library song for the room, ready to play, but only if the room is still idle, so
    /// two players asking at once cannot queue two songs. True when it was queued.
    pub fn autoplay_insert(&self, room: &str, s: &RadioSong) -> Result<bool> {
        let mut c = self.c();
        let tx = c.transaction()?;
        let busy: i64 = tx.query_row(
            "SELECT COUNT(*) FROM tracks WHERE room = ?1 AND status IN ('queued','working','ready','playing')",
            [room],
            |r| r.get(0),
        )?;
        if busy > 0 {
            return Ok(false);
        }
        let lead = s.artist.split([',', ';']).next().unwrap_or(&s.artist).trim().to_string();
        tx.execute(
            "INSERT INTO tracks(room, status, artist, dir_artist, title, album, duration, file, added_by, added_at)
             VALUES(?1, 'ready', ?2, ?3, ?4, ?5, ?6, ?7, 'Autoplay', ?8)",
            params![room, s.artist, lead, s.title, s.album, s.duration, s.path, now()],
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// Drop songs Autoplay picked that have not started, so a changed setting can pick again.
    pub fn drop_pending_autoplay(&self, room: &str) -> Result<()> {
        self.c().execute("DELETE FROM tracks WHERE room = ?1 AND status = 'ready' AND added_by = 'Autoplay'", [room])?;
        Ok(())
    }

    /// Tracks still waiting to play (queued, being fetched, or ready).
    pub fn open_count(&self, room: &str) -> Result<i64> {
        Ok(self
            .c()
            .query_row("SELECT COUNT(*) FROM tracks WHERE room = ?1 AND status IN ('queued','working','ready')", [room], |r| r.get(0))?)
    }

    pub fn snapshot(&self, room: &str) -> Result<Snapshot> {
        let c = self.c();
        let list = |sql: &str| -> Result<Vec<Track>> {
            let mut st = c.prepare(sql)?;
            let rows = st.query_map([room], track_from)?;
            Ok(rows.collect::<rusqlite::Result<_>>()?)
        };
        let now = list(&format!("SELECT {TRACK_COLS} FROM tracks WHERE room = ?1 AND status = 'playing' LIMIT 1"))?.pop();
        let queue =
            list(&format!("SELECT {TRACK_COLS} FROM tracks WHERE room = ?1 AND status IN ('queued','working','ready') ORDER BY id"))?;
        let recent =
            list(&format!("SELECT {TRACK_COLS} FROM tracks WHERE room = ?1 AND status IN ('played','failed') ORDER BY id DESC LIMIT 8"))?;
        Ok(Snapshot { now, queue, recent })
    }

    /// Start the next track if nothing is playing. Returns whatever is playing afterwards.
    /// Strict order: if the head of the queue is still being fetched, nothing starts yet.
    pub fn claim_next(&self, room: &str) -> Result<Option<Track>> {
        let mut c = self.c();
        let tx = c.transaction()?;
        let q = format!("SELECT {TRACK_COLS} FROM tracks WHERE room = ?1 AND status = 'playing' LIMIT 1");
        if let Some(t) = tx.query_row(&q, [room], track_from).optional()? {
            return Ok(Some(t));
        }
        let q = format!("SELECT {TRACK_COLS} FROM tracks WHERE room = ?1 AND status IN ('queued','working','ready') ORDER BY id LIMIT 1");
        let head = tx.query_row(&q, [room], track_from).optional()?;
        let out = match head {
            Some(mut t) if t.status == "ready" => {
                tx.execute(
                    "UPDATE tracks SET status = 'playing', started_at = ?2, started_ms = ?3, paused_pos = NULL WHERE id = ?1",
                    params![t.id, now(), now_ms()],
                )?;
                t.status = "playing".into();
                t.position = Some(0.0);
                Some(t)
            }
            _ => None,
        };
        tx.commit()?;
        Ok(out)
    }

    /// Mark one specific track as played, only if it is the one playing. A late "ended" from the
    /// player must not end the track that replaced it.
    pub fn finish_track(&self, room: &str, id: i64) -> Result<()> {
        self.c().execute("UPDATE tracks SET status = 'played' WHERE room = ?1 AND id = ?2 AND status = 'playing'", params![room, id])?;
        Ok(())
    }

    /// The playing song of a room with its clock: (id, started_ms, paused_pos).
    fn playing_clock(c: &Connection, room: &str) -> Result<Option<Clock>> {
        Ok(c.query_row("SELECT id, started_ms, paused_pos FROM tracks WHERE room = ?1 AND status = 'playing' LIMIT 1", [room], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .optional()?)
    }

    /// Pause or resume the room. Every player follows the clock, so all of them stop and start together.
    pub fn set_paused(&self, room: &str, paused: bool) -> Result<()> {
        let c = self.c();
        let Some((id, started, at)) = Self::playing_clock(&c, room)? else { return Ok(()) };
        match (paused, at) {
            (true, None) => {
                let here = started.map_or(0.0, |s| ((now_ms() - s) as f64 / 1000.0).max(0.0));
                c.execute("UPDATE tracks SET paused_pos = ?2 WHERE id = ?1", params![id, here])?;
            }
            (false, Some(p)) => {
                c.execute(
                    "UPDATE tracks SET started_ms = ?2, paused_pos = NULL WHERE id = ?1",
                    params![id, now_ms() - (p * 1000.0) as i64],
                )?;
            }
            _ => {}
        }
        Ok(())
    }

    /// Move the playing song to `to` seconds for every player.
    pub fn seek(&self, room: &str, to: f64) -> Result<()> {
        let to = if to.is_finite() { to.max(0.0) } else { 0.0 };
        let c = self.c();
        let Some((id, _, at)) = Self::playing_clock(&c, room)? else { return Ok(()) };
        if at.is_some() {
            c.execute("UPDATE tracks SET paused_pos = ?2 WHERE id = ?1", params![id, to])?;
        } else {
            c.execute("UPDATE tracks SET started_ms = ?2 WHERE id = ?1", params![id, now_ms() - (to * 1000.0) as i64])?;
        }
        Ok(())
    }

    /// Remove a waiting track, or skip it if it is the one playing.
    pub fn remove(&self, room: &str, id: i64) -> Result<bool> {
        let c = self.c();
        let n = c.execute(
            "UPDATE tracks SET status = CASE status WHEN 'playing' THEN 'played' ELSE 'removed' END
             WHERE room = ?1 AND id = ?2 AND status IN ('queued','working','ready','playing')",
            params![room, id],
        )?;
        Ok(n > 0)
    }

    /// File to stream for a track that belongs to this room and has been fetched.
    pub fn track_file(&self, room: &str, id: i64) -> Result<Option<String>> {
        Ok(self
            .c()
            .query_row(
                "SELECT file FROM tracks WHERE room = ?1 AND id = ?2 AND status IN ('ready','playing','played') AND file IS NOT NULL",
                params![room, id],
                |r| r.get(0),
            )
            .optional()?)
    }

    // ---- worker ----

    pub fn next_job(&self) -> Result<Option<Job>> {
        let mut c = self.c();
        let tx = c.transaction()?;
        let job = tx
            .query_row(
                "SELECT id, artist, dir_artist, title, album, duration, cover, src, query, added_by
                 FROM tracks WHERE status = 'queued' ORDER BY id LIMIT 1",
                [],
                |r| {
                    Ok(Job {
                        id: r.get(0)?,
                        item: NewTrack {
                            artist: r.get(1)?,
                            dir_artist: r.get(2)?,
                            title: r.get(3)?,
                            album: r.get(4)?,
                            duration: r.get(5)?,
                            cover: r.get(6)?,
                            src: r.get(7)?,
                            query: r.get(8)?,
                            added_by: r.get(9)?,
                        },
                    })
                },
            )
            .optional()?;
        if let Some(j) = &job {
            tx.execute("UPDATE tracks SET status = 'working' WHERE id = ?1", [j.id])?;
        }
        tx.commit()?;
        Ok(job)
    }

    pub fn set_ready(&self, id: i64, file: &str, fetched: bool) -> Result<()> {
        self.c().execute(
            "UPDATE tracks SET status = 'ready', file = ?2, error = NULL, fetched_at = ?3 WHERE id = ?1 AND status = 'working'",
            params![id, file, if fetched { Some(now()) } else { None }],
        )?;
        Ok(())
    }

    /// Fill in names that only became known after the download (for example from yt-dlp).
    pub fn set_details(&self, id: i64, artist: &str, title: &str, album: Option<&str>) -> Result<()> {
        self.c().execute(
            "UPDATE tracks SET artist = ?2, title = ?3, album = COALESCE(?4, album) WHERE id = ?1",
            params![id, artist, title, album],
        )?;
        Ok(())
    }

    pub fn set_failed(&self, id: i64, error: &str) -> Result<()> {
        self.c().execute(
            "UPDATE tracks SET status = 'failed', error = ?2 WHERE id = ?1 AND status IN ('working','queued')",
            params![id, error],
        )?;
        Ok(())
    }

    /// Files that songs still waiting to play depend on.
    pub fn open_files(&self) -> Result<std::collections::HashSet<String>> {
        let c = self.c();
        let mut st = c.prepare("SELECT file FROM tracks WHERE file IS NOT NULL AND status IN ('queued','working','ready','playing')")?;
        let rows = st.query_map([], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    // ---- Soulseek upgrades ----

    /// Songs whose file is in the cache: artist, title, length and the cached file, oldest first.
    pub fn cached_songs(&self, prefix: &str) -> Result<Vec<CachedSong>> {
        let c = self.c();
        let mut st = c.prepare(
            "SELECT artist, title, duration, file FROM tracks WHERE file IS NOT NULL AND substr(file, 1, length(?1)) = ?1
             GROUP BY lower(artist), lower(title) ORDER BY MIN(id)",
        )?;
        let rows = st.query_map([prefix], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn upgrades(&self) -> Result<HashMap<String, Upgrade>> {
        let c = self.c();
        let mut st =
            c.prepare("SELECT key, artist, title, duration, cached, status, peer, remote, size, note, updated_at FROM upgrades")?;
        let rows = st.query_map([], |r| {
            Ok(Upgrade {
                key: r.get(0)?,
                artist: r.get(1)?,
                title: r.get(2)?,
                duration: r.get(3)?,
                cached: r.get(4)?,
                status: r.get(5)?,
                peer: r.get(6)?,
                remote: r.get(7)?,
                size: r.get(8)?,
                note: r.get(9)?,
                updated_at: r.get(10)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?.into_iter().map(|u| (u.key.clone(), u)).collect())
    }

    pub fn upgrade_put(&self, u: &Upgrade) -> Result<()> {
        self.c().execute(
            "INSERT INTO upgrades(key, artist, title, duration, cached, status, peer, remote, size, note, updated_at)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
             ON CONFLICT(key) DO UPDATE SET artist = ?2, title = ?3, duration = ?4, cached = ?5, status = ?6, peer = ?7,
                 remote = ?8, size = ?9, note = ?10, updated_at = ?11",
            params![u.key, u.artist, u.title, u.duration, u.cached, u.status, u.peer, u.remote, u.size, u.note, u.updated_at],
        )?;
        Ok(())
    }

    /// Point waiting songs at the better file. The song playing right now keeps its file: switching
    /// format under a player that is part-way through would break the stream.
    pub fn apply_upgrade(&self, cached: &str, file: &str) -> Result<usize> {
        Ok(self.c().execute("UPDATE tracks SET file = ?2 WHERE file = ?1 AND status != 'playing'", params![cached, file])?)
    }

    /// A consistent copy of the whole database (rooms and queue), safe to take while the service runs.
    pub fn backup_to(&self, path: &Path) -> Result<()> {
        let _ = std::fs::remove_file(path);
        self.c().execute("VACUUM INTO ?1", [path.to_string_lossy().as_ref()])?;
        Ok(())
    }

    /// Run at startup: downloads cut off by a restart go back in line. The playing song stays as
    /// it is: its clock is wall time, so listeners carry on in the right place after a restart.
    pub fn reset_stale(&self) -> Result<()> {
        self.c().execute("UPDATE tracks SET status = 'queued' WHERE status = 'working'", [])?;
        Ok(())
    }

    pub fn fetched_today(&self) -> Result<i64> {
        Ok(self.c().query_row("SELECT COUNT(*) FROM tracks WHERE fetched_at > ?1", [now() - 86_400], |r| r.get(0))?)
    }

    // ---- library index ----

    pub fn lib_upsert(&self, e: &LibEntry) -> Result<()> {
        self.lib_upsert_many(std::slice::from_ref(e))
    }

    pub fn lib_upsert_many(&self, entries: &[LibEntry]) -> Result<()> {
        let mut c = self.c();
        let tx = c.transaction()?;
        for e in entries {
            tx.execute(
                "INSERT INTO library(path, mtime, artist, title, tn, duration, album, genre, year, bpm, language, v)
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 2)
                 ON CONFLICT(path) DO UPDATE SET mtime = ?2, artist = ?3, title = ?4, tn = ?5, duration = ?6,
                     album = ?7, genre = ?8, year = ?9, language = ?11, v = 2,
                     bpm = COALESCE(?10, CASE WHEN library.mtime = ?2 THEN library.bpm ELSE NULL END)",
                params![e.path, e.mtime, e.artist, e.title, e.tn, e.duration, e.album, e.genre, e.year, e.bpm, e.language],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Every indexed path with its file time and how it was read (0 = before genre and year were kept).
    pub fn lib_known(&self) -> Result<HashMap<String, (i64, i64)>> {
        let c = self.c();
        let mut st = c.prepare("SELECT path, mtime, v FROM library")?;
        let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, (r.get::<_, i64>(1)?, r.get::<_, i64>(2)?))))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn lib_delete(&self, paths: &[String]) -> Result<()> {
        let mut c = self.c();
        let tx = c.transaction()?;
        for p in paths {
            tx.execute("DELETE FROM library WHERE path = ?1", [p])?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Candidate library files for a normalised title, with their artist and length.
    pub fn lib_by_title(&self, tn: &str) -> Result<Vec<LibHit>> {
        let c = self.c();
        let mut st = c.prepare("SELECT path, artist, duration, title FROM library WHERE tn = ?1")?;
        let rows = st.query_map([tn], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn lib_count(&self) -> Result<i64> {
        Ok(self.c().query_row("SELECT COUNT(*) FROM library", [], |r| r.get(0))?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(title: &str) -> NewTrack {
        NewTrack { artist: "A".into(), dir_artist: "A".into(), title: title.into(), ..Default::default() }
    }

    #[test]
    fn rooms_can_be_created_and_revoked() {
        let db = Db::memory().unwrap();
        db.create_room("abc", "Office").unwrap();
        assert!(!db.room("abc").unwrap().unwrap().revoked);
        assert!(db.revoke_room("abc").unwrap());
        assert!(db.room("abc").unwrap().unwrap().revoked);
        assert!(db.room("zzz").unwrap().is_none());
    }

    #[test]
    fn queue_plays_strictly_in_order() {
        let db = Db::memory().unwrap();
        let a = db.add_track("r", &track("one")).unwrap();
        let b = db.add_track("r", &track("two")).unwrap();
        // Head is not fetched yet, so nothing may start even though the second is ready.
        let j = db.next_job().unwrap().unwrap();
        assert_eq!(j.id, a);
        let j2 = db.next_job().unwrap().unwrap();
        assert_eq!(j2.id, b);
        db.set_ready(b, "/x/two.m4a", true).unwrap();
        assert!(db.claim_next("r").unwrap().is_none());
        db.set_ready(a, "/x/one.m4a", false).unwrap();
        assert_eq!(db.claim_next("r").unwrap().unwrap().id, a);
        // Claiming again returns the same playing track.
        assert_eq!(db.claim_next("r").unwrap().unwrap().id, a);
        db.finish_track("r", a).unwrap();
        assert_eq!(db.claim_next("r").unwrap().unwrap().id, b);
    }

    #[test]
    fn failed_head_does_not_block_the_queue() {
        let db = Db::memory().unwrap();
        let a = db.add_track("r", &track("bad")).unwrap();
        let b = db.add_track("r", &track("good")).unwrap();
        db.next_job().unwrap();
        db.next_job().unwrap();
        db.set_failed(a, "no match").unwrap();
        db.set_ready(b, "/x/good.m4a", false).unwrap();
        assert_eq!(db.claim_next("r").unwrap().unwrap().id, b);
        assert_eq!(db.snapshot("r").unwrap().recent[0].status, "failed");
    }

    #[test]
    fn remove_skips_playing_and_drops_waiting() {
        let db = Db::memory().unwrap();
        let a = db.add_track("r", &track("one")).unwrap();
        let b = db.add_track("r", &track("two")).unwrap();
        db.next_job().unwrap();
        db.set_ready(a, "/x/one.m4a", false).unwrap();
        db.claim_next("r").unwrap();
        assert!(db.remove("r", a).unwrap());
        assert!(db.remove("r", b).unwrap());
        assert!(!db.remove("r", b).unwrap());
        assert!(db.snapshot("r").unwrap().now.is_none());
        assert_eq!(db.open_count("r").unwrap(), 0);
    }

    #[test]
    fn a_late_finish_only_ends_the_track_it_names() {
        let db = Db::memory().unwrap();
        let a = db.add_track("r", &track("one")).unwrap();
        let b = db.add_track("r", &track("two")).unwrap();
        db.next_job().unwrap();
        db.next_job().unwrap();
        db.set_ready(a, "/x/one.m4a", false).unwrap();
        db.set_ready(b, "/x/two.m4a", false).unwrap();
        db.claim_next("r").unwrap();
        db.remove("r", a).unwrap(); // someone skipped it
        db.claim_next("r").unwrap(); // two is now playing
        db.finish_track("r", a).unwrap(); // the old player reports that one ended
        assert_eq!(db.snapshot("r").unwrap().now.unwrap().id, b);
        db.finish_track("r", b).unwrap();
        assert!(db.snapshot("r").unwrap().now.is_none());
    }

    #[test]
    fn rooms_do_not_see_each_others_tracks() {
        let db = Db::memory().unwrap();
        let a = db.add_track("r1", &track("one")).unwrap();
        db.next_job().unwrap();
        db.set_ready(a, "/x/one.m4a", false).unwrap();
        assert!(db.track_file("r1", a).unwrap().is_some());
        assert!(db.track_file("r2", a).unwrap().is_none());
    }

    #[test]
    fn rooms_and_the_queue_survive_closing_and_reopening_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("jam.db");
        {
            let db = Db::open(&file).unwrap();
            db.create_room("abc", "Office").unwrap();
            db.add_track("abc", &track("one")).unwrap();
            db.add_track("abc", &track("two")).unwrap();
        }
        let db = Db::open(&file).unwrap();
        db.reset_stale().unwrap();
        assert_eq!(db.room("abc").unwrap().unwrap().name, "Office");
        assert_eq!(db.snapshot("abc").unwrap().queue.len(), 2);
    }

    #[test]
    fn a_backup_is_a_complete_database_of_its_own() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("jam.db")).unwrap();
        db.create_room("abc", "Office").unwrap();
        db.add_track("abc", &track("one")).unwrap();
        let copy = dir.path().join("backup.db");
        db.backup_to(&copy).unwrap();
        db.backup_to(&copy).unwrap(); // a second backup replaces the first
        let restored = Db::open(&copy).unwrap();
        assert!(restored.room("abc").unwrap().is_some());
        assert_eq!(restored.snapshot("abc").unwrap().queue.len(), 1);
    }

    #[test]
    fn upgrades_are_remembered_and_only_waiting_songs_switch_file() {
        let db = Db::memory().unwrap();
        let a = db.add_track("r", &track("one")).unwrap();
        let b = db.add_track("r", &track("one")).unwrap();
        db.next_job().unwrap();
        db.next_job().unwrap();
        db.set_ready(a, "/cache/A/one.m4a", true).unwrap();
        db.set_ready(b, "/cache/A/one.m4a", true).unwrap();
        db.claim_next("r").unwrap(); // `a` is playing, `b` waits
        let songs = db.cached_songs("/cache/").unwrap();
        assert_eq!(songs.len(), 1, "the same song twice is one song");
        assert!(db.cached_songs("/elsewhere/").unwrap().is_empty());

        let up = Upgrade {
            key: "a|one".into(),
            artist: "A".into(),
            title: "one".into(),
            cached: "/cache/A/one.m4a".into(),
            status: "downloading".into(),
            updated_at: 5,
            ..Default::default()
        };
        db.upgrade_put(&up).unwrap();
        db.upgrade_put(&Upgrade { status: "done".into(), ..up.clone() }).unwrap();
        assert_eq!(db.upgrades().unwrap()["a|one"].status, "done");

        assert_eq!(db.apply_upgrade("/cache/A/one.m4a", "/music/A/one.flac").unwrap(), 1);
        assert_eq!(db.track_file("r", b).unwrap().as_deref(), Some("/music/A/one.flac"));
        assert_eq!(db.track_file("r", a).unwrap().as_deref(), Some("/cache/A/one.m4a"));
    }

    #[test]
    fn autoplay_queues_one_ready_song_only_when_the_room_is_idle() {
        let db = Db::memory().unwrap();
        db.create_room("r", "Office").unwrap();
        assert!(!db.room("r").unwrap().unwrap().autoplay);
        db.set_room_flags("r", Some(true), None, None).unwrap();
        db.set_room_flags("r", None, Some(&MoodPatch { genre: Some(true), tempo: Some(true), ..Default::default() }), Some(Close::Tight))
            .unwrap();
        // A later change touches only what it names.
        db.set_room_flags("r", None, Some(&MoodPatch { era: Some(true), tempo: Some(false), ..Default::default() }), None).unwrap();
        let room = db.room("r").unwrap().unwrap();
        assert!(room.autoplay && room.close == Close::Tight);
        assert_eq!(room.mood, Mood { genre: true, era: true, ..Default::default() });
        db.set_mood_note("r", "Rock").unwrap();
        assert_eq!(db.room("r").unwrap().unwrap().mood_note, "Rock");
        let song =
            RadioSong { path: "/m/a.flac".into(), artist: "A, B".into(), title: "One".into(), duration: 200.0, ..Default::default() };
        assert!(db.idle("r").unwrap());
        assert!(db.autoplay_insert("r", &song).unwrap());
        assert!(!db.autoplay_insert("r", &song).unwrap(), "a second player asking at the same moment queues nothing");
        assert!(!db.idle("r").unwrap());
        let t = db.claim_next("r").unwrap().unwrap();
        assert_eq!((t.status.as_str(), t.added_by.as_deref(), t.title.as_str()), ("playing", Some("Autoplay"), "One"));
        assert_eq!(db.track_file("r", t.id).unwrap().as_deref(), Some("/m/a.flac"));
        assert_eq!(db.recent_plays("r", 5).unwrap(), vec![("A, B".to_string(), "One".to_string())]);
    }

    #[test]
    fn the_radio_only_offers_library_songs_of_a_sensible_length() {
        let db = Db::memory().unwrap();
        let add = |path: &str, d: f64| {
            db.lib_upsert(&LibEntry {
                path: path.into(),
                mtime: 1,
                artist: "A".into(),
                title: path.into(),
                tn: path.into(),
                duration: Some(d),
                genre: Some("Rock".into()),
                ..Default::default()
            })
            .unwrap()
        };
        add("/music/ok.flac", 200.0);
        add("/music/jingle.flac", 20.0);
        add("/music/mix.flac", 3600.0);
        add("/data/songs/cached.m4a", 200.0);
        let songs = db.radio_songs("/data/songs/").unwrap();
        assert_eq!(songs.len(), 2, "the song and the cached one; the jingle and the mix are too short or too long");
        let library: Vec<_> = songs.iter().filter(|s| !s.cached).collect();
        assert_eq!((library[0].path.as_str(), library[0].genre.as_deref()), ("/music/ok.flac", Some("Rock")));
        assert!(songs.iter().any(|s| s.cached && s.path == "/data/songs/cached.m4a"));
    }

    #[test]
    fn tempo_is_kept_until_the_file_changes_and_counted_for_the_library_only() {
        let db = Db::memory().unwrap();
        let entry = |path: &str, mtime: i64, bpm: Option<f64>| LibEntry {
            path: path.into(),
            mtime,
            artist: "A".into(),
            title: path.into(),
            tn: path.into(),
            duration: Some(200.0),
            bpm,
            ..Default::default()
        };
        db.lib_upsert(&entry("/music/a.flac", 1, None)).unwrap();
        db.lib_upsert(&entry("/music/b.flac", 1, Some(128.0))).unwrap();
        db.lib_upsert(&entry("/data/songs/c.m4a", 1, None)).unwrap();
        // Cached songs are measured first; a song whose tag already has a tempo is not queued.
        let todo = db.bpm_todo("/data/songs/", 10).unwrap();
        assert_eq!(todo.iter().map(|t| t.0.as_str()).collect::<Vec<_>>(), ["/data/songs/c.m4a", "/music/a.flac"]);
        db.set_bpm("/music/a.flac", 97.5).unwrap();
        db.set_bpm("/data/songs/c.m4a", 0.0).unwrap(); // looked, no clear beat
        assert!(db.bpm_todo("/data/songs/", 10).unwrap().is_empty());
        assert_eq!(db.tempo_stats("/data/songs/").unwrap(), (2, 2));
        // Re-scanning an unchanged file keeps the measured tempo; a changed file forgets it.
        db.lib_upsert(&entry("/music/a.flac", 1, None)).unwrap();
        assert_eq!(db.radio_songs("/data/songs/").unwrap().iter().find(|s| s.path == "/music/a.flac").unwrap().bpm, Some(97.5));
        db.lib_upsert(&entry("/music/a.flac", 2, None)).unwrap();
        assert_eq!(db.bpm_todo("/data/songs/", 10).unwrap().len(), 1);
    }

    #[test]
    fn restart_requeues_half_done_work() {
        let db = Db::memory().unwrap();
        let a = db.add_track("r", &track("one")).unwrap();
        db.next_job().unwrap();
        db.reset_stale().unwrap();
        assert_eq!(db.next_job().unwrap().unwrap().id, a);
    }

    #[test]
    fn a_restart_does_not_stop_the_song_that_is_playing() {
        let db = Db::memory().unwrap();
        let a = db.add_track("r", &track("one")).unwrap();
        db.next_job().unwrap();
        db.set_ready(a, "/x/one.m4a", false).unwrap();
        db.claim_next("r").unwrap();
        db.reset_stale().unwrap();
        assert_eq!(db.claim_next("r").unwrap().unwrap().id, a);
    }

    #[test]
    fn the_playback_clock_runs_pauses_and_seeks() {
        let db = Db::memory().unwrap();
        let a = db.add_track("r", &track("one")).unwrap();
        db.next_job().unwrap();
        db.set_ready(a, "/x/one.m4a", false).unwrap();
        let t = db.claim_next("r").unwrap().unwrap();
        assert!(t.position.unwrap() < 1.0 && !t.paused);

        db.seek("r", 30.0).unwrap();
        let p = db.claim_next("r").unwrap().unwrap().position.unwrap();
        assert!((30.0..31.0).contains(&p), "{p}");

        db.set_paused("r", true).unwrap();
        let frozen = db.claim_next("r").unwrap().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(30));
        let later = db.claim_next("r").unwrap().unwrap();
        assert!(later.paused && frozen.position == later.position);

        db.seek("r", 5.0).unwrap();
        assert_eq!(db.claim_next("r").unwrap().unwrap().position, Some(5.0));
        db.set_paused("r", false).unwrap();
        let moving = db.claim_next("r").unwrap().unwrap();
        assert!(!moving.paused && (5.0..6.0).contains(&moving.position.unwrap()));
        // Nothing playing: pausing and seeking do nothing instead of failing.
        db.finish_track("r", a).unwrap();
        db.set_paused("r", true).unwrap();
        db.seek("r", 3.0).unwrap();
    }

    #[test]
    fn library_lookup_by_title() {
        let db = Db::memory().unwrap();
        let e = LibEntry {
            path: "/m/a.flac".into(),
            mtime: 1,
            artist: "A".into(),
            title: "Song".into(),
            tn: "song".into(),
            duration: Some(200.0),
            ..Default::default()
        };
        db.lib_upsert(&e).unwrap();
        assert_eq!(db.lib_by_title("song").unwrap().len(), 1);
        db.lib_delete(&["/m/a.flac".to_string()]).unwrap();
        assert_eq!(db.lib_count().unwrap(), 0);
    }
}
