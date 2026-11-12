//! SQLite storage: rooms, the play queue and the library index.

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::Serialize;
use std::{
    collections::HashMap,
    path::Path,
    sync::{Mutex, MutexGuard},
    time::{SystemTime, UNIX_EPOCH},
};

pub fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
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
CREATE TABLE IF NOT EXISTS library(
    path TEXT PRIMARY KEY, mtime INTEGER NOT NULL, artist TEXT NOT NULL, title TEXT NOT NULL,
    tn TEXT NOT NULL, duration REAL);
CREATE INDEX IF NOT EXISTS library_tn ON library(tn);
";

pub struct Db {
    conn: Mutex<Connection>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Room {
    pub code: String,
    pub name: String,
    pub revoked: bool,
    pub created_at: i64,
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

#[derive(Debug, Clone)]
pub struct LibEntry {
    pub path: String,
    pub mtime: i64,
    pub artist: String,
    pub title: String,
    pub tn: String,
    pub duration: Option<f64>,
}

#[derive(Debug, Serialize)]
pub struct Snapshot {
    pub now: Option<Track>,
    pub queue: Vec<Track>,
    pub recent: Vec<Track>,
}

const TRACK_COLS: &str = "id, status, artist, title, album, duration, cover, added_by, error";

fn track_from(r: &Row) -> rusqlite::Result<Track> {
    Ok(Track {
        id: r.get(0)?,
        status: r.get(1)?,
        artist: r.get(2)?,
        title: r.get(3)?,
        album: r.get(4)?,
        duration: r.get(5)?,
        cover: r.get(6)?,
        added_by: r.get(7)?,
        error: r.get(8)?,
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
            .query_row("SELECT code, name, revoked, created_at FROM rooms WHERE code = ?1", [code], |r| {
                Ok(Room { code: r.get(0)?, name: r.get(1)?, revoked: r.get::<_, i64>(2)? != 0, created_at: r.get(3)? })
            })
            .optional()?)
    }

    pub fn rooms(&self) -> Result<Vec<Room>> {
        let c = self.c();
        let mut st = c.prepare("SELECT code, name, revoked, created_at FROM rooms ORDER BY created_at")?;
        let rows =
            st.query_map([], |r| Ok(Room { code: r.get(0)?, name: r.get(1)?, revoked: r.get::<_, i64>(2)? != 0, created_at: r.get(3)? }))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
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
                tx.execute("UPDATE tracks SET status = 'playing', started_at = ?2 WHERE id = ?1", params![t.id, now()])?;
                t.status = "playing".into();
                Some(t)
            }
            _ => None,
        };
        tx.commit()?;
        Ok(out)
    }

    /// Mark the playing track as played. Used for both "ended" and "skip".
    pub fn finish_current(&self, room: &str) -> Result<()> {
        self.c().execute("UPDATE tracks SET status = 'played' WHERE room = ?1 AND status = 'playing'", [room])?;
        Ok(())
    }

    /// Mark one specific track as played, only if it is the one playing. A late "ended" from the
    /// player must not end the track that replaced it.
    pub fn finish_track(&self, room: &str, id: i64) -> Result<()> {
        self.c().execute("UPDATE tracks SET status = 'played' WHERE room = ?1 AND id = ?2 AND status = 'playing'", params![room, id])?;
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

    /// Run at startup: anything left half-done by a restart goes back in line.
    pub fn reset_stale(&self) -> Result<()> {
        let c = self.c();
        c.execute("UPDATE tracks SET status = 'queued' WHERE status = 'working'", [])?;
        c.execute("UPDATE tracks SET status = 'ready' WHERE status = 'playing'", [])?;
        Ok(())
    }

    pub fn fetched_today(&self) -> Result<i64> {
        Ok(self.c().query_row("SELECT COUNT(*) FROM tracks WHERE fetched_at > ?1", [now() - 86_400], |r| r.get(0))?)
    }

    // ---- library index ----

    pub fn lib_upsert(&self, e: &LibEntry) -> Result<()> {
        self.c().execute(
            "INSERT INTO library(path, mtime, artist, title, tn, duration) VALUES(?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(path) DO UPDATE SET mtime = ?2, artist = ?3, title = ?4, tn = ?5, duration = ?6",
            params![e.path, e.mtime, e.artist, e.title, e.tn, e.duration],
        )?;
        Ok(())
    }

    pub fn lib_upsert_many(&self, entries: &[LibEntry]) -> Result<()> {
        let mut c = self.c();
        let tx = c.transaction()?;
        for e in entries {
            tx.execute(
                "INSERT INTO library(path, mtime, artist, title, tn, duration) VALUES(?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(path) DO UPDATE SET mtime = ?2, artist = ?3, title = ?4, tn = ?5, duration = ?6",
                params![e.path, e.mtime, e.artist, e.title, e.tn, e.duration],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn lib_known(&self) -> Result<HashMap<String, i64>> {
        let c = self.c();
        let mut st = c.prepare("SELECT path, mtime FROM library")?;
        let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
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
    pub fn lib_by_title(&self, tn: &str) -> Result<Vec<(String, String, Option<f64>)>> {
        let c = self.c();
        let mut st = c.prepare("SELECT path, artist, duration FROM library WHERE tn = ?1")?;
        let rows = st.query_map([tn], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
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
        db.finish_current("r").unwrap();
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
    fn restart_requeues_half_done_work() {
        let db = Db::memory().unwrap();
        let a = db.add_track("r", &track("one")).unwrap();
        db.next_job().unwrap();
        db.reset_stale().unwrap();
        assert_eq!(db.next_job().unwrap().unwrap().id, a);
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
        };
        db.lib_upsert(&e).unwrap();
        assert_eq!(db.lib_by_title("song").unwrap().len(), 1);
        db.lib_delete(&["/m/a.flac".to_string()]).unwrap();
        assert_eq!(db.lib_count().unwrap(), 0);
    }
}
