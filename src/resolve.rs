//! Turn whatever someone pastes into queue entries.
//!
//! * YouTube / SoundCloud links: yt-dlp reads the metadata and supplies the download URL.
//! * Spotify links: the public embed page lists title, artists and length. No audio comes
//!   from Spotify; the same recording is matched on YouTube Music later.
//! * Tidal links: the page title is used as a search.
//! * Anything else: a YouTube Music song search.

use crate::{
    config::Config,
    db::NewTrack,
    norm::{artist_match, norm},
    ytdlp,
};
use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use url::Url;

#[derive(Debug, PartialEq)]
pub enum SpKind {
    Track,
    Album,
    Playlist,
}

#[derive(Debug, PartialEq)]
pub enum Input {
    Spotify(SpKind, String),
    Tidal(Url),
    Media(Url),
    Search(String),
}

const YT_HOSTS: &[&str] = &["youtube.com", "www.youtube.com", "m.youtube.com", "music.youtube.com", "youtu.be"];
const SC_HOSTS: &[&str] = &["soundcloud.com", "www.soundcloud.com", "m.soundcloud.com", "on.soundcloud.com"];
const TIDAL_HOSTS: &[&str] = &["tidal.com", "www.tidal.com", "listen.tidal.com", "play.tidal.com"];
const ONLY: &str = "only YouTube, SoundCloud, Spotify and Tidal links work, or type a song name";

fn spotify(kind: &str, id: &str) -> Result<Input, String> {
    let kind = match kind {
        "track" => SpKind::Track,
        "album" => SpKind::Album,
        "playlist" => SpKind::Playlist,
        _ => return Err("Spotify links must be a track, album or playlist".into()),
    };
    if (10..=30).contains(&id.len()) && id.chars().all(|c| c.is_ascii_alphanumeric()) {
        Ok(Input::Spotify(kind, id.to_string()))
    } else {
        Err("that Spotify link looks broken".into())
    }
}

/// Decide what an input is, rejecting anything that is not an allowed site.
pub fn classify(input: &str) -> Result<Input, String> {
    let s = input.trim();
    if s.len() < 2 || s.len() > 300 {
        return Err("paste a link or type a song name".into());
    }
    if let Some(rest) = s.strip_prefix("spotify:") {
        let mut p = rest.split(':');
        return match (p.next(), p.next()) {
            (Some(k), Some(id)) => spotify(k, id),
            _ => Err("that Spotify link looks broken".into()),
        };
    }
    if !s.contains("://") {
        return Ok(Input::Search(s.to_string()));
    }
    let url = Url::parse(s).map_err(|_| "that does not look like a valid link".to_string())?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(ONLY.into());
    }
    let host = url.host_str().unwrap_or("").to_ascii_lowercase();
    if host == "open.spotify.com" {
        let mut segs = url.path_segments().map(|p| p.filter(|s| !s.is_empty()).collect::<Vec<_>>()).unwrap_or_default();
        if segs.first().is_some_and(|s| s.starts_with("intl-")) {
            segs.remove(0);
        }
        return match (segs.first(), segs.get(1)) {
            (Some(k), Some(id)) => spotify(k, id),
            _ => Err("that Spotify link looks broken".into()),
        };
    }
    if TIDAL_HOSTS.contains(&host.as_str()) {
        Ok(Input::Tidal(url))
    } else if YT_HOSTS.contains(&host.as_str()) || SC_HOSTS.contains(&host.as_str()) {
        Ok(Input::Media(url))
    } else {
        Err(ONLY.into())
    }
}

/// One YouTube Music search hit.
#[derive(Debug, Clone, PartialEq)]
pub struct Cand {
    pub video_id: String,
    pub title: String,
    pub artist: String,
    pub album: Option<String>,
    pub duration: Option<f64>,
}

impl Cand {
    pub fn url(&self) -> String {
        format!("https://music.youtube.com/watch?v={}", self.video_id)
    }
}

pub fn first_artist(a: &str) -> String {
    a.split([',', ';']).next().unwrap_or(a).trim().to_string()
}

fn clock(s: &str) -> Option<f64> {
    let parts: Vec<f64> = s.trim().split(':').map(|p| p.parse::<f64>()).collect::<Result<_, _>>().ok()?;
    match parts.as_slice() {
        [m, s] => Some(m * 60.0 + s),
        [h, m, s] => Some(h * 3600.0 + m * 60.0 + s),
        _ => None,
    }
}

fn runs_text(item: &Value, col: usize) -> Vec<String> {
    item.pointer(&format!("/flexColumns/{col}/musicResponsiveListItemFlexColumnRenderer/text/runs"))
        .and_then(Value::as_array)
        .map(|runs| runs.iter().filter_map(|r| r.get("text").and_then(Value::as_str).map(String::from)).collect())
        .unwrap_or_default()
}

/// Read the song results out of a YouTube Music search response.
pub fn parse_ytm_search(v: &Value) -> Vec<Cand> {
    let sections = v
        .pointer("/contents/tabbedSearchResultsRenderer/tabs/0/tabRenderer/content/sectionListRenderer/contents")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut out = Vec::new();
    for sec in &sections {
        let Some(items) = sec.pointer("/musicShelfRenderer/contents").and_then(Value::as_array) else { continue };
        for it in items {
            let Some(r) = it.get("musicResponsiveListItemRenderer") else { continue };
            let title = runs_text(r, 0).concat();
            let video_id = r
                .pointer("/playlistItemData/videoId")
                .or_else(|| {
                    r.pointer(
                        "/flexColumns/0/musicResponsiveListItemFlexColumnRenderer/text/runs/0/navigationEndpoint/watchEndpoint/videoId",
                    )
                })
                .and_then(Value::as_str);
            let (Some(video_id), false) = (video_id, title.is_empty()) else { continue };
            let mut groups = vec![String::new()];
            for run in runs_text(r, 1) {
                if run.trim() == "•" {
                    groups.push(String::new());
                } else if let Some(g) = groups.last_mut() {
                    g.push_str(&run);
                }
            }
            let duration = groups.last().and_then(|g| clock(g));
            let album = (groups.len() >= 3).then(|| groups[1].trim().to_string()).filter(|a| !a.is_empty());
            out.push(Cand { video_id: video_id.to_string(), title, artist: groups[0].trim().to_string(), album, duration });
        }
    }
    out
}

/// The search hit that is the same recording as the wanted track, if any.
pub fn pick_best<'a>(cands: &'a [Cand], artist: &str, title: &str, duration: Option<f64>) -> Option<&'a Cand> {
    let wanted = norm(title);
    let title_ok = |c: &Cand| {
        let have = norm(&c.title);
        !have.is_empty() && !wanted.is_empty() && (have.contains(&wanted) || wanted.contains(&have))
    };
    let delta = |c: &Cand| match (duration, c.duration) {
        (Some(w), Some(h)) => Some((w - h).abs()),
        _ => None,
    };
    let eligible = |tol: f64| {
        cands
            .iter()
            .filter(|c| title_ok(c) && artist_match(&c.artist, artist))
            .filter(|c| delta(c).is_none_or(|d| d <= tol))
            .min_by(|a, b| delta(a).unwrap_or(0.0).total_cmp(&delta(b).unwrap_or(0.0)))
    };
    // Spotify and YouTube Music lengths agree to about a second for the same master.
    eligible(2.5).or_else(|| eligible(6.0).filter(|c| norm(&c.title) == wanted))
}

fn next_data(html: &str) -> Option<Value> {
    let i = html.find("id=\"__NEXT_DATA__\"")?;
    let start = html[i..].find('>')? + i + 1;
    let end = html[start..].find("</script>")? + start;
    serde_json::from_str(&html[start..end]).ok()
}

fn split_artists(s: &str) -> String {
    s.replace('\u{a0}', " ").split(',').map(str::trim).filter(|p| !p.is_empty()).collect::<Vec<_>>().join(", ")
}

fn best_cover(entity: &Value) -> Option<String> {
    entity
        .pointer("/visualIdentity/image")?
        .as_array()?
        .iter()
        .filter_map(|i| Some((i.get("maxWidth")?.as_u64()?, i.get("url")?.as_str()?)))
        .max_by_key(|(w, _)| *w)
        .map(|(_, u)| u.to_string())
}

/// Read tracks out of a Spotify embed page. Only metadata is available there.
pub fn parse_spotify_embed(html: &str, max: usize) -> Result<Vec<NewTrack>> {
    let data = next_data(html).ok_or_else(|| anyhow!("could not read that Spotify page"))?;
    let e = data.pointer("/props/pageProps/state/data/entity").ok_or_else(|| anyhow!("could not read that Spotify page"))?;
    let name = e.get("name").and_then(Value::as_str).unwrap_or_default();
    let cover = best_cover(e);
    let secs = |v: &Value| v.get("duration").and_then(Value::as_f64).filter(|d| *d > 0.0).map(|d| d / 1000.0);
    let mk = |artist: String, title: &str, album: Option<String>, dir: String, duration: Option<f64>| NewTrack {
        dir_artist: if dir.is_empty() { first_artist(&artist) } else { dir },
        artist,
        title: title.to_string(),
        album,
        duration,
        cover: cover.clone(),
        ..Default::default()
    };
    match e.get("type").and_then(Value::as_str) {
        Some("track") => {
            let artists: Vec<&str> = e
                .get("artists")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(|x| x.get("name")?.as_str()).collect())
                .unwrap_or_default();
            if name.is_empty() || artists.is_empty() {
                bail!("that Spotify track has no details");
            }
            Ok(vec![mk(artists.join(", "), name, None, String::new(), secs(e))])
        }
        Some(kind @ ("album" | "playlist")) => {
            let is_album = kind == "album";
            let dir = if is_album { first_artist(e.get("subtitle").and_then(Value::as_str).unwrap_or_default()) } else { String::new() };
            let list = e.get("trackList").and_then(Value::as_array).ok_or_else(|| anyhow!("that Spotify {kind} is empty"))?;
            let tracks: Vec<NewTrack> = list
                .iter()
                .take(max)
                .filter_map(|t| {
                    let title = t.get("title")?.as_str()?;
                    let artist = split_artists(t.get("subtitle")?.as_str()?);
                    let album = is_album.then(|| name.to_string());
                    // Playlist art is the playlist's, not the song's, so only albums share one cover.
                    let mut n = mk(artist, title, album, dir.clone(), secs(t));
                    if !is_album {
                        n.cover = None;
                    }
                    Some(n)
                })
                .collect();
            if tracks.is_empty() {
                bail!("that Spotify {kind} has no tracks I can read");
            }
            Ok(tracks)
        }
        _ => bail!("Spotify links must be a track, album or playlist"),
    }
}

fn meta_tag(html: &str, prop: &str) -> Option<String> {
    let at = html.find(&format!("property=\"{prop}\""))?;
    let start = html[..at].rfind('<')?;
    let end = html[at..].find('>')? + at;
    let value = html[start..end].split("content=\"").nth(1)?.split('"').next()?;
    Some(value.replace("&amp;", "&").replace("&quot;", "\"").replace("&#39;", "'").replace("&lt;", "<").replace("&gt;", ">"))
}

/// The track name Tidal shows in the page title, used as a search.
pub fn parse_tidal(html: &str) -> Option<String> {
    let title = meta_tag(html, "og:title")?;
    let mut t = title.trim();
    for suffix in [" on TIDAL", " | TIDAL", " - TIDAL", " – TIDAL"] {
        t = t.strip_suffix(suffix).unwrap_or(t);
    }
    let t = t.trim();
    (!t.is_empty() && !t.starts_with("Not Found") && !t.eq_ignore_ascii_case("tidal")).then(|| t.to_string())
}

fn str_of<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty())
}

/// Convert one yt-dlp entry (full or flat) into a queue entry.
pub fn media_entry(v: &Value) -> Option<NewTrack> {
    let url = str_of(v, "webpage_url").or_else(|| str_of(v, "url"))?;
    let raw_title = str_of(v, "track").or_else(|| str_of(v, "title"))?;
    let mut artist = str_of(v, "artist").map(String::from).or_else(|| {
        let names: Vec<&str> = v.get("artists")?.as_array()?.iter().filter_map(Value::as_str).collect();
        (!names.is_empty()).then(|| names.join(", "))
    });
    let mut title = raw_title.to_string();
    if artist.is_none()
        && str_of(v, "track").is_none()
        && let Some((a, t)) = raw_title.split_once(" - ")
    {
        artist = Some(a.trim().to_string());
        title = t.trim().to_string();
    }
    let artist = artist
        .or_else(|| str_of(v, "uploader").or_else(|| str_of(v, "channel")).map(|u| u.trim_end_matches(" - Topic").to_string()))
        .unwrap_or_default();
    Some(NewTrack {
        dir_artist: if artist.is_empty() { "Unknown Artist".into() } else { first_artist(&artist) },
        artist,
        title,
        album: str_of(v, "album").map(String::from),
        duration: v.get("duration").and_then(Value::as_f64).filter(|d| *d > 0.0),
        cover: str_of(v, "thumbnail").map(String::from),
        src: Some(url.to_string()),
        ..Default::default()
    })
}

/// True when a YouTube or SoundCloud link names a list of songs rather than one.
fn is_playlist(url: &Url) -> bool {
    let has = |k: &str| url.query_pairs().any(|(key, _)| key == k);
    // youtu.be keeps the video id in the path, so a `list=` there still means one song.
    let one_video = has("v") || url.host_str() == Some("youtu.be");
    let path = url.path();
    path.contains("/playlist") || path.contains("/sets/") || (has("list") && !one_video)
}

pub struct Resolver {
    cfg: Arc<Config>,
    http: reqwest::Client,
}

impl Resolver {
    pub fn new(cfg: Arc<Config>) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent("Mozilla/5.0 (X11; Linux x86_64; rv:130.0) Gecko/20100101 Firefox/130.0")
            .timeout(Duration::from_secs(20))
            .build()?;
        Ok(Resolver { cfg, http })
    }

    pub fn http(&self) -> &reqwest::Client {
        &self.http
    }

    /// Resolve pasted input into queue entries (metadata only; nothing is downloaded yet).
    pub async fn resolve(&self, input: &str, added_by: Option<String>) -> Result<Vec<NewTrack>> {
        let mut tracks = match classify(input).map_err(|e| anyhow!(e))? {
            Input::Spotify(kind, id) => self.spotify(&kind, &id).await?,
            Input::Tidal(url) => self.tidal(&url).await?,
            Input::Media(url) => self.media(&url).await?,
            Input::Search(text) => self.search(&text).await?,
        };
        tracks.truncate(self.cfg.max_playlist);
        for t in &mut tracks {
            t.added_by = added_by.clone();
        }
        Ok(tracks)
    }

    async fn page(&self, url: &str) -> Result<String> {
        Ok(self.http.get(url).send().await?.error_for_status()?.text().await?)
    }

    async fn spotify(&self, kind: &SpKind, id: &str) -> Result<Vec<NewTrack>> {
        let k = match kind {
            SpKind::Track => "track",
            SpKind::Album => "album",
            SpKind::Playlist => "playlist",
        };
        let html = self
            .page(&format!("https://open.spotify.com/embed/{k}/{id}"))
            .await
            .map_err(|_| anyhow!("Spotify did not answer for that link"))?;
        parse_spotify_embed(&html, self.cfg.max_playlist)
    }

    async fn tidal(&self, url: &Url) -> Result<Vec<NewTrack>> {
        let html = self.page(url.as_str()).await.map_err(|_| anyhow!("Tidal did not answer for that link"))?;
        let title = parse_tidal(&html).ok_or_else(|| anyhow!("could not read that Tidal link"))?;
        Ok(vec![NewTrack { title: title.clone(), query: Some(title), ..Default::default() }])
    }

    async fn media(&self, url: &Url) -> Result<Vec<NewTrack>> {
        let playlist = is_playlist(url);
        let end = self.cfg.max_playlist.to_string();
        let args: Vec<&str> = if playlist {
            vec!["--flat-playlist", "--playlist-end", &end, "--", url.as_str()]
        } else {
            vec!["--no-playlist", "--", url.as_str()]
        };
        let v = ytdlp::json(&self.cfg, &args, 60).await.map_err(|e| anyhow!("could not read that link: {e}"))?;
        let tracks: Vec<NewTrack> = match v.get("entries").and_then(Value::as_array) {
            Some(entries) => entries.iter().filter_map(media_entry).collect(),
            None => media_entry(&v).into_iter().collect(),
        };
        if tracks.is_empty() {
            bail!("nothing playable found at that link");
        }
        Ok(tracks)
    }

    async fn search(&self, text: &str) -> Result<Vec<NewTrack>> {
        let c = self.first_for_query(text).await?;
        Ok(vec![NewTrack {
            dir_artist: first_artist(&c.artist),
            artist: c.artist.clone(),
            title: c.title.clone(),
            album: c.album.clone(),
            duration: c.duration,
            src: Some(c.url()),
            ..Default::default()
        }])
    }

    pub async fn ytm_search(&self, query: &str) -> Result<Vec<Cand>> {
        let body = json!({
            "context": {"client": {"clientName": "WEB_REMIX", "clientVersion": "1.20250101.01.00", "hl": "en", "gl": "US"}},
            "query": query,
            // Restricts results to songs.
            "params": "EgWKAQIIAWoKEAkQBRAKEAMQBA==",
        });
        let v: Value = self
            .http
            .post("https://music.youtube.com/youtubei/v1/search?prettyPrint=false")
            .header("Origin", "https://music.youtube.com")
            .json(&body)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(parse_ytm_search(&v))
    }

    /// Top song for a free-text query, ignoring clips and hour-long mixes.
    pub async fn first_for_query(&self, query: &str) -> Result<Cand> {
        let max = f64::from(self.cfg.max_seconds);
        self.ytm_search(query)
            .await
            .map_err(|_| anyhow!("music search is not answering"))?
            .into_iter()
            .find(|c| c.duration.is_none_or(|d| (30.0..=max).contains(&d)))
            .ok_or_else(|| anyhow!("no song found for \"{query}\""))
    }

    /// The YouTube Music recording that matches a known artist, title and length.
    pub async fn match_source(&self, artist: &str, title: &str, duration: Option<f64>) -> Result<Cand> {
        let cut = title.split(['(', '[']).next().unwrap_or(title).trim();
        // "(I Can't Get No) Satisfaction" cuts to nothing; keep the whole title then.
        let plain = if cut.is_empty() { title } else { cut };
        let query = format!("{} {}", first_artist(artist), plain);
        let cands = self.ytm_search(&query).await.map_err(|_| anyhow!("music search is not answering"))?;
        pick_best(&cands, artist, title, duration).cloned().ok_or_else(|| anyhow!("no matching recording found"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> String {
        std::fs::read_to_string(format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
    }

    #[test]
    fn classifies_links_and_text() {
        use Input::*;
        assert_eq!(classify("spotify:track:114sIoTAaTobcugh52xmM2"), Ok(Spotify(SpKind::Track, "114sIoTAaTobcugh52xmM2".into())));
        assert_eq!(
            classify("https://open.spotify.com/intl-pl/album/0ETFjACtuP2ADo6LFhL6HN?si=abc"),
            Ok(Spotify(SpKind::Album, "0ETFjACtuP2ADo6LFhL6HN".into()))
        );
        assert!(matches!(classify("https://youtu.be/fsR452Wrnak"), Ok(Media(_))));
        assert!(matches!(classify("https://music.youtube.com/watch?v=fsR452Wrnak"), Ok(Media(_))));
        assert!(matches!(classify("https://soundcloud.com/a/b"), Ok(Media(_))));
        assert!(matches!(classify("https://tidal.com/browse/track/1"), Ok(Tidal(_))));
        assert_eq!(classify("The Sequence Disturbia"), Ok(Search("The Sequence Disturbia".into())));
    }

    #[test]
    fn a_short_link_with_a_list_is_still_one_song() {
        let p = |s: &str| is_playlist(&Url::parse(s).unwrap());
        assert!(!p("https://youtu.be/fsR452Wrnak?list=RDfsR452Wrnak"));
        assert!(!p("https://www.youtube.com/watch?v=fsR452Wrnak&list=PL123"));
        assert!(p("https://www.youtube.com/playlist?list=PL123"));
        assert!(p("https://soundcloud.com/a/sets/b"));
    }

    #[test]
    fn rejects_everything_else() {
        assert!(classify("https://evil.example/x").is_err());
        assert!(classify("http://127.0.0.1:8080/admin").is_err());
        assert!(classify("file:///etc/passwd").is_err());
        assert!(classify("https://youtube.com.evil.example/watch").is_err());
        assert!(classify("https://open.spotify.com/artist/0ea4MiVQlqu6IEyKJ818OC").is_err());
        assert!(classify("https://open.spotify.com/track/short").is_err());
        assert!(classify("").is_err());
        assert!(classify(&"x".repeat(400)).is_err());
    }

    #[test]
    fn reads_a_spotify_track() {
        let t = parse_spotify_embed(&fixture("spotify_embed_track.html"), 50).unwrap();
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].title, "Legendy Polskie - Cichosza");
        assert_eq!(t[0].artist, "Marcin Macuk, Krzysztof Zalewski, Marcin Bors");
        assert_eq!(t[0].dir_artist, "Marcin Macuk");
        assert!((t[0].duration.unwrap() - 230.398).abs() < 0.01);
        assert!(t[0].album.is_none());
        assert!(t[0].cover.as_deref().unwrap().starts_with("https://"));
    }

    #[test]
    fn reads_a_spotify_album_and_playlist() {
        let a = parse_spotify_embed(&fixture("spotify_embed_album.html"), 50).unwrap();
        assert_eq!(a.len(), 3);
        assert_eq!(a[0].album.as_deref(), Some("Abbey Road (Remastered 2009)"));
        assert_eq!(a[0].dir_artist, "The Beatles");
        assert_eq!(a[0].title, "Come Together - Remastered 2009");
        let p = parse_spotify_embed(&fixture("spotify_embed_playlist.html"), 2).unwrap();
        assert_eq!(p.len(), 2);
        assert!(p[0].album.is_none() && p[0].cover.is_none());
    }

    #[test]
    fn garbage_pages_are_errors() {
        assert!(parse_spotify_embed("<html>nothing</html>", 5).is_err());
    }

    #[test]
    fn reads_tidal_titles() {
        let page = |t: &str| format!("<html><head><meta property=\"og:title\" content=\"{t}\"></head></html>");
        assert_eq!(parse_tidal(&page("Come Together - The Beatles on TIDAL")).as_deref(), Some("Come Together - The Beatles"));
        assert_eq!(parse_tidal(&page("Not Found - TIDAL")), None);
        assert_eq!(parse_tidal("<html></html>"), None);
    }

    #[test]
    fn parses_youtube_music_results_and_picks_the_right_one() {
        let v: Value = serde_json::from_str(&fixture("ytmusic_search.json")).unwrap();
        let cands = parse_ytm_search(&v);
        assert_eq!(cands.len(), 3);
        assert_eq!(
            cands[0],
            Cand {
                video_id: "fsR452Wrnak".into(),
                title: "Disturbia".into(),
                artist: "The Sequence".into(),
                album: Some("In Lights".into()),
                duration: Some(211.0)
            }
        );
        assert_eq!(pick_best(&cands, "The Sequence", "Disturbia", Some(211.4)).unwrap().video_id, "fsR452Wrnak");
        // Same title by another artist must not match, nor the right song at the wrong length.
        assert_eq!(pick_best(&cands, "Rihanna", "Disturbia", Some(239.0)).unwrap().video_id, "wmGjajXDHKo");
        assert!(pick_best(&cands, "The Sequence", "Disturbia", Some(300.0)).is_none());
        assert!(pick_best(&cands, "Nobody", "Disturbia", None).is_none());
    }

    #[test]
    fn clock_formats() {
        assert_eq!(clock("3:31"), Some(211.0));
        assert_eq!(clock("1:02:03"), Some(3723.0));
        assert_eq!(clock("Song"), None);
    }

    #[test]
    fn reads_yt_dlp_entries() {
        let music = json!({"title": "Disturbia", "track": "Disturbia", "artist": "The Sequence", "album": "In Lights", "uploader": "Sequence - Topic",
            "duration": 211, "thumbnail": "https://i.ytimg.com/x.webp", "webpage_url": "https://www.youtube.com/watch?v=fsR452Wrnak"});
        let t = media_entry(&music).unwrap();
        assert_eq!((t.artist.as_str(), t.title.as_str(), t.album.as_deref()), ("The Sequence", "Disturbia", Some("In Lights")));
        let sc = json!({"title": "Dence - Fyre (waitwhat remix)", "uploader": "waitwhat", "duration": 149.296, "webpage_url": "https://soundcloud.com/itswaitwhat/fyre-remix", "url": "https://stream.example/x.m3u8"});
        let t = media_entry(&sc).unwrap();
        assert_eq!((t.artist.as_str(), t.title.as_str()), ("Dence", "Fyre (waitwhat remix)"));
        assert_eq!(t.src.as_deref(), Some("https://soundcloud.com/itswaitwhat/fyre-remix"));
        let plain = json!({"title": "Some Talk", "channel": "Some Channel - Topic", "url": "https://www.youtube.com/watch?v=abc"});
        let t = media_entry(&plain).unwrap();
        assert_eq!((t.artist.as_str(), t.title.as_str()), ("Some Channel", "Some Talk"));
        assert!(media_entry(&json!({"title": "no url"})).is_none());
    }
}
