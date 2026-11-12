//! State shared by the web handlers and the background worker.

use crate::{config::Config, db::Db, resolve::Resolver};
use anyhow::Result;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::sync::Notify;

pub struct App {
    pub cfg: Arc<Config>,
    pub db: Db,
    pub resolver: Resolver,
    /// Wakes the worker when something is queued.
    pub wake: Notify,
    limiter: Mutex<HashMap<String, (Instant, u32)>>,
}

impl App {
    pub fn new(cfg: Arc<Config>, db: Db) -> Result<Arc<App>> {
        let resolver = Resolver::new(cfg.clone())?;
        Ok(Arc::new(App { cfg, db, resolver, wake: Notify::new(), limiter: Mutex::new(HashMap::new()) }))
    }

    /// Fixed-window counter: true while `key` has made at most `limit` calls this minute.
    pub fn allow(&self, key: &str, limit: u32) -> bool {
        let mut m = self.limiter.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        if m.len() > 5000 {
            m.retain(|_, (t, _)| now.duration_since(*t) < Duration::from_secs(60));
        }
        let e = m.entry(key.to_string()).or_insert((now, 0));
        if now.duration_since(e.0) >= Duration::from_secs(60) {
            *e = (now, 0);
        }
        e.1 += 1;
        e.1 <= limit
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limiter_counts_per_key() {
        let cfg = Arc::new(Config::from_env());
        let app = App::new(cfg, Db::memory().unwrap()).unwrap();
        assert!(app.allow("a", 2));
        assert!(app.allow("a", 2));
        assert!(!app.allow("a", 2));
        assert!(app.allow("b", 2));
    }
}
