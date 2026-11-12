//! HTTP routes: the room pages, the queue API, media streaming and the admin API.

use crate::{app::App, db::Room};
use anyhow::Result;
use axum::{
    Json, Router,
    extract::{ConnectInfo, Path, Query, Request, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::{delete, get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{io::Read, net::SocketAddr, sync::Arc};
use tower::ServiceExt;
use tower_http::services::ServeFile;

type Shared = State<Arc<App>>;
type Client = ConnectInfo<SocketAddr>;

pub struct ApiError(StatusCode, String);

impl ApiError {
    fn new(code: StatusCode, msg: &str) -> Self {
        ApiError(code, msg.to_string())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

impl<E: Into<anyhow::Error>> From<E> for ApiError {
    fn from(e: E) -> Self {
        let e: anyhow::Error = e.into();
        tracing::error!(error = %e, "request failed");
        ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "something went wrong")
    }
}

pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/", get(|| async { Html(include_str!("../web/index.html")) }))
        .route("/healthz", get(|| async { "ok" }))
        .route("/assets/jam.css", get(|| async { ([(header::CONTENT_TYPE, "text/css; charset=utf-8")], include_str!("../web/jam.css")) }))
        .route(
            "/assets/jam.js",
            get(|| async { ([(header::CONTENT_TYPE, "text/javascript; charset=utf-8")], include_str!("../web/jam.js")) }),
        )
        .route(
            "/assets/tick.js",
            get(|| async { ([(header::CONTENT_TYPE, "text/javascript; charset=utf-8")], include_str!("../web/tick.js")) }),
        )
        .route("/admin", get(|| async { Html(include_str!("../web/admin.html")) }))
        .route("/r/{code}", get(room_page))
        .route("/r/{code}/player", get(player_page))
        .route("/api/r/{code}/state", get(state))
        .route("/api/r/{code}/add", post(add))
        .route("/api/r/{code}/advance", post(advance))
        .route("/api/r/{code}/next", post(next))
        .route("/api/r/{code}/pause", post(pause))
        .route("/api/r/{code}/resume", post(resume))
        .route("/api/r/{code}/seek", post(seek))
        .route("/api/r/{code}/remove/{id}", post(remove))
        .route("/media/{code}/{id}", get(media))
        .route("/api/admin/rooms", get(admin_list).post(admin_create))
        .route("/api/admin/rooms/{code}", delete(admin_revoke))
        .layer(middleware::from_fn(security_headers))
        .with_state(app)
}

async fn security_headers(req: Request, next: Next) -> Response {
    let api = req.uri().path().starts_with("/api/");
    let mut res = next.run(req).await;
    let h = res.headers_mut();
    h.insert("x-content-type-options", HeaderValue::from_static("nosniff"));
    h.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    h.insert(
        "content-security-policy",
        HeaderValue::from_static(
            "default-src 'self'; img-src https: data:; style-src 'self' 'unsafe-inline'; script-src 'self' 'unsafe-inline'; media-src 'self'; frame-ancestors 'none'",
        ),
    );
    if api {
        h.insert("cache-control", HeaderValue::from_static("no-store"));
    }
    res
}

/// The caller's address. Behind Traefik the real client is the last `X-Forwarded-For` entry.
fn client_ip(headers: &HeaderMap, addr: SocketAddr) -> String {
    headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.rsplit(',').next())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| addr.ip().to_string())
}

/// A room that exists and has not been revoked. Wrong codes are rate-limited per client.
fn live_room(app: &App, ip: &str, code: &str) -> Result<Room, ApiError> {
    if code.len() <= 32
        && let Some(r) = app.db.room(code)?
        && !r.revoked
    {
        return Ok(r);
    }
    if !app.allow(&format!("bad:{ip}"), 20) {
        return Err(ApiError::new(StatusCode::TOO_MANY_REQUESTS, "too many wrong codes, wait a minute"));
    }
    Err(ApiError::new(StatusCode::NOT_FOUND, "unknown room"))
}

fn page(app: &App, headers: &HeaderMap, addr: SocketAddr, code: &str, html: &'static str) -> Response {
    match live_room(app, &client_ip(headers, addr), code) {
        Ok(_) => Html(html).into_response(),
        Err(ApiError(status, _)) => (status, Html("<!doctype html><meta charset=utf-8><body style=\"font:16px sans-serif;padding:2rem\"><h1>Room not found</h1><p>Check the code and try again.</p>")).into_response(),
    }
}

async fn room_page(State(app): Shared, ConnectInfo(a): Client, h: HeaderMap, Path(code): Path<String>) -> Response {
    page(&app, &h, a, &code, include_str!("../web/room.html"))
}

async fn player_page(State(app): Shared, ConnectInfo(a): Client, h: HeaderMap, Path(code): Path<String>) -> Response {
    page(&app, &h, a, &code, include_str!("../web/player.html"))
}

fn state_json(app: &App, room: &Room) -> Result<Json<Value>, ApiError> {
    let s = app.db.snapshot(&room.code)?;
    Ok(Json(json!({ "room": { "code": room.code, "name": room.name }, "now": s.now, "queue": s.queue, "recent": s.recent })))
}

async fn state(State(app): Shared, ConnectInfo(a): Client, h: HeaderMap, Path(code): Path<String>) -> Result<Json<Value>, ApiError> {
    let room = live_room(&app, &client_ip(&h, a), &code)?;
    state_json(&app, &room)
}

#[derive(Deserialize)]
struct AddReq {
    input: String,
    name: Option<String>,
}

fn clean_name(n: Option<String>) -> Option<String> {
    let n: String = n?.chars().filter(|c| !c.is_control()).take(40).collect();
    let n = n.trim().to_string();
    (!n.is_empty()).then_some(n)
}

async fn add(
    State(app): Shared,
    ConnectInfo(a): Client,
    h: HeaderMap,
    Path(code): Path<String>,
    Json(req): Json<AddReq>,
) -> Result<Json<Value>, ApiError> {
    let ip = client_ip(&h, a);
    let room = live_room(&app, &ip, &code)?;
    if !app.allow(&format!("add:{ip}"), app.cfg.adds_per_minute) {
        return Err(ApiError::new(StatusCode::TOO_MANY_REQUESTS, "slow down a little"));
    }
    let open = app.db.open_count(&room.code)?;
    if open >= app.cfg.max_queue {
        return Err(ApiError::new(StatusCode::CONFLICT, "the queue is full"));
    }
    let mut tracks = app.resolver.resolve(&req.input, clean_name(req.name)).await.map_err(|e| {
        // Refused adds are the only way to learn why someone could not add a song.
        let typed: String = req.input.chars().take(120).collect();
        tracing::info!(%ip, input = %typed, reason = %e, "add refused");
        ApiError(StatusCode::BAD_REQUEST, e.to_string())
    })?;
    tracks.truncate((app.cfg.max_queue - open) as usize);
    for t in &tracks {
        app.db.add_track(&room.code, t)?;
    }
    app.wake.notify_one();
    Ok(Json(json!({ "added": tracks.len(), "first": tracks.first().map(|t| t.title.clone()) })))
}

async fn advance(State(app): Shared, ConnectInfo(a): Client, h: HeaderMap, Path(code): Path<String>) -> Result<Json<Value>, ApiError> {
    let room = live_room(&app, &client_ip(&h, a), &code)?;
    app.db.claim_next(&room.code)?;
    state_json(&app, &room)
}

#[derive(Deserialize)]
struct NextQuery {
    /// The track the player just finished. Without it nothing is ended, only the next track starts.
    done: Option<i64>,
}

async fn next(
    State(app): Shared,
    ConnectInfo(a): Client,
    h: HeaderMap,
    Path(code): Path<String>,
    Query(q): Query<NextQuery>,
) -> Result<Json<Value>, ApiError> {
    let room = live_room(&app, &client_ip(&h, a), &code)?;
    if let Some(id) = q.done {
        app.db.finish_track(&room.code, id)?;
    }
    app.db.claim_next(&room.code)?;
    state_json(&app, &room)
}

async fn pause(State(app): Shared, ConnectInfo(a): Client, h: HeaderMap, Path(code): Path<String>) -> Result<Json<Value>, ApiError> {
    let room = live_room(&app, &client_ip(&h, a), &code)?;
    app.db.set_paused(&room.code, true)?;
    state_json(&app, &room)
}

async fn resume(State(app): Shared, ConnectInfo(a): Client, h: HeaderMap, Path(code): Path<String>) -> Result<Json<Value>, ApiError> {
    let room = live_room(&app, &client_ip(&h, a), &code)?;
    app.db.set_paused(&room.code, false)?;
    state_json(&app, &room)
}

#[derive(Deserialize)]
struct SeekQuery {
    to: f64,
}

async fn seek(
    State(app): Shared,
    ConnectInfo(a): Client,
    h: HeaderMap,
    Path(code): Path<String>,
    Query(q): Query<SeekQuery>,
) -> Result<Json<Value>, ApiError> {
    let room = live_room(&app, &client_ip(&h, a), &code)?;
    app.db.seek(&room.code, q.to)?;
    state_json(&app, &room)
}

async fn remove(
    State(app): Shared,
    ConnectInfo(a): Client,
    h: HeaderMap,
    Path((code, id)): Path<(String, i64)>,
) -> Result<Json<Value>, ApiError> {
    let room = live_room(&app, &client_ip(&h, a), &code)?;
    app.db.remove(&room.code, id)?;
    state_json(&app, &room)
}

async fn media(State(app): Shared, ConnectInfo(a): Client, h: HeaderMap, Path((code, id)): Path<(String, i64)>, req: Request) -> Response {
    let Ok(room) = live_room(&app, &client_ip(&h, a), &code) else { return StatusCode::NOT_FOUND.into_response() };
    match app.db.track_file(&room.code, id) {
        Ok(Some(file)) => {
            let ext = std::path::Path::new(&file).extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
            match ServeFile::new(file).oneshot(req).await {
                Ok(mut res) => {
                    // The guessed type for m4a is non-standard; some browsers refuse to play it.
                    let mime = match ext.as_str() {
                        "m4a" | "mp4" => Some("audio/mp4"),
                        "flac" => Some("audio/flac"),
                        "ogg" | "opus" => Some("audio/ogg"),
                        "mp3" => Some("audio/mpeg"),
                        _ => None,
                    };
                    if let Some(m) = mime {
                        res.headers_mut().insert("content-type", HeaderValue::from_static(m));
                    }
                    res.into_response()
                }
                Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
            }
        }
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

// ---- admin ----

fn authed(app: &App, h: &HeaderMap) -> bool {
    let want = app.cfg.admin_token.as_bytes();
    let got = h.get("authorization").and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer ")).unwrap_or("").as_bytes();
    // Constant-time comparison so response timing does not leak the token.
    !want.is_empty() && want.len() == got.len() && want.iter().zip(got).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0
}

fn require_admin(app: &App, ip: &str, h: &HeaderMap) -> Result<(), ApiError> {
    if authed(app, h) {
        return Ok(());
    }
    if !app.allow(&format!("admin:{ip}"), 10) {
        return Err(ApiError::new(StatusCode::TOO_MANY_REQUESTS, "too many attempts"));
    }
    Err(ApiError::new(StatusCode::UNAUTHORIZED, "admin token required"))
}

pub fn new_code() -> Result<String> {
    const ALPHABET: &[u8] = b"abcdefghjkmnpqrstuvwxyz23456789";
    let mut bytes = [0u8; 10];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|b| ALPHABET[*b as usize % ALPHABET.len()] as char).collect())
}

async fn admin_list(State(app): Shared, ConnectInfo(a): Client, h: HeaderMap) -> Result<Json<Value>, ApiError> {
    require_admin(&app, &client_ip(&h, a), &h)?;
    Ok(Json(json!({ "rooms": app.db.rooms()?, "library_tracks": app.db.lib_count()? })))
}

#[derive(Deserialize)]
struct NewRoom {
    name: String,
}

async fn admin_create(State(app): Shared, ConnectInfo(a): Client, h: HeaderMap, Json(req): Json<NewRoom>) -> Result<Json<Value>, ApiError> {
    require_admin(&app, &client_ip(&h, a), &h)?;
    let name = clean_name(Some(req.name)).ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "give the room a name"))?;
    let code = new_code()?;
    app.db.create_room(&code, &name)?;
    Ok(Json(json!({ "code": code, "name": name })))
}

async fn admin_revoke(State(app): Shared, ConnectInfo(a): Client, h: HeaderMap, Path(code): Path<String>) -> Result<Json<Value>, ApiError> {
    require_admin(&app, &client_ip(&h, a), &h)?;
    Ok(Json(json!({ "revoked": app.db.revoke_room(&code)? })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::Config,
        db::{Db, NewTrack},
    };
    use axum::{body::Body, http::Request as Req};
    use http_body_util::BodyExt;

    fn app_with_room() -> (Arc<App>, Router) {
        let mut cfg = Config::from_env();
        cfg.admin_token = "s3cret".into();
        let app = App::new(Arc::new(cfg), Db::memory().unwrap()).unwrap();
        app.db.create_room("room1", "Office").unwrap();
        (app.clone(), router(app))
    }

    async fn call(r: &Router, method: &str, uri: &str, auth: bool, body: Option<Value>) -> (StatusCode, Vec<u8>) {
        let mut b = Req::builder().method(method).uri(uri).header("content-type", "application/json");
        if auth {
            b = b.header("authorization", "Bearer s3cret");
        }
        let mut req = b.body(body.map(|v| Body::from(v.to_string())).unwrap_or_else(Body::empty)).unwrap();
        req.extensions_mut().insert(ConnectInfo("127.0.0.1:1".parse::<SocketAddr>().unwrap()));
        let res = r.clone().oneshot(req).await.unwrap();
        (res.status(), res.into_body().collect().await.unwrap().to_bytes().to_vec())
    }

    #[tokio::test]
    async fn unknown_and_revoked_rooms_are_not_found() {
        let (app, r) = app_with_room();
        assert_eq!(call(&r, "GET", "/api/r/nope/state", false, None).await.0, StatusCode::NOT_FOUND);
        assert_eq!(call(&r, "GET", "/r/nope", false, None).await.0, StatusCode::NOT_FOUND);
        assert_eq!(call(&r, "GET", "/r/room1", false, None).await.0, StatusCode::OK);
        app.db.revoke_room("room1").unwrap();
        assert_eq!(call(&r, "GET", "/api/r/room1/state", false, None).await.0, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn wrong_codes_get_rate_limited() {
        let (_, r) = app_with_room();
        let mut last = StatusCode::OK;
        for _ in 0..25 {
            last = call(&r, "GET", "/api/r/guess/state", false, None).await.0;
        }
        assert_eq!(last, StatusCode::TOO_MANY_REQUESTS);
    }

    #[tokio::test]
    async fn bad_input_is_rejected_without_touching_the_network() {
        let (_, r) = app_with_room();
        let (st, body) = call(&r, "POST", "/api/r/room1/add", false, Some(json!({"input": "http://127.0.0.1:8080/admin"}))).await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        assert!(String::from_utf8_lossy(&body).contains("only YouTube"));
    }

    #[tokio::test]
    async fn queue_flow_and_media_streaming() {
        let (app, r) = app_with_room();
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.m4a");
        std::fs::write(&file, b"0123456789").unwrap();
        let id = app
            .db
            .add_track("room1", &NewTrack { artist: "A".into(), dir_artist: "A".into(), title: "T".into(), ..Default::default() })
            .unwrap();
        app.db.next_job().unwrap();
        // Not fetched yet: no media, nothing to play.
        assert_eq!(call(&r, "GET", &format!("/media/room1/{id}"), false, None).await.0, StatusCode::NOT_FOUND);
        app.db.set_ready(id, file.to_str().unwrap(), false).unwrap();
        let (_, body) = call(&r, "POST", "/api/r/room1/advance", false, None).await;
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["now"]["title"], "T");
        let (st, bytes) = call(&r, "GET", &format!("/media/room1/{id}"), false, None).await;
        assert_eq!((st, bytes.as_slice()), (StatusCode::OK, b"0123456789".as_slice()));
        // Another room cannot read it.
        app.db.create_room("room2", "Other").unwrap();
        assert_eq!(call(&r, "GET", &format!("/media/room2/{id}"), false, None).await.0, StatusCode::NOT_FOUND);
        let (_, body) = call(&r, "POST", &format!("/api/r/room1/next?done={id}"), false, None).await;
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert!(v["now"].is_null());
        assert_eq!(v["recent"][0]["status"], "played");
    }

    #[tokio::test]
    async fn every_player_follows_one_clock_and_the_shared_files_are_served() {
        let (app, r) = app_with_room();
        let new = NewTrack { artist: "A".into(), dir_artist: "A".into(), title: "T".into(), ..Default::default() };
        let id = app.db.add_track("room1", &new).unwrap();
        app.db.next_job().unwrap();
        app.db.set_ready(id, "/x/t.m4a", false).unwrap();
        call(&r, "POST", "/api/r/room1/advance", false, None).await;
        let now = |body: &[u8]| serde_json::from_slice::<Value>(body).unwrap()["now"].clone();
        let (_, b) = call(&r, "POST", "/api/r/room1/seek?to=42", false, None).await;
        assert!((42.0..43.0).contains(&now(&b)["position"].as_f64().unwrap()) && now(&b)["paused"] == false);
        let (_, b) = call(&r, "POST", "/api/r/room1/pause", false, None).await;
        assert_eq!(now(&b)["paused"], true);
        let (_, b) = call(&r, "POST", "/api/r/room1/resume", false, None).await;
        assert_eq!(now(&b)["paused"], false);
        assert_eq!(call(&r, "POST", "/api/r/nope/pause", false, None).await.0, StatusCode::NOT_FOUND);
        assert_eq!(call(&r, "GET", "/assets/jam.css", false, None).await.0, StatusCode::OK);
        assert_eq!(call(&r, "GET", "/assets/jam.js", false, None).await.0, StatusCode::OK);
        assert_eq!(call(&r, "GET", "/assets/tick.js", false, None).await.0, StatusCode::OK);
    }

    #[tokio::test]
    async fn admin_needs_the_token() {
        let (_, r) = app_with_room();
        assert_eq!(call(&r, "GET", "/api/admin/rooms", false, None).await.0, StatusCode::UNAUTHORIZED);
        let (st, body) = call(&r, "POST", "/api/admin/rooms", true, Some(json!({"name": "Lobby"}))).await;
        assert_eq!(st, StatusCode::OK);
        let code = serde_json::from_slice::<Value>(&body).unwrap()["code"].as_str().unwrap().to_string();
        assert_eq!(code.len(), 10);
        assert_eq!(call(&r, "GET", &format!("/api/r/{code}/state"), false, None).await.0, StatusCode::OK);
        assert_eq!(call(&r, "DELETE", &format!("/api/admin/rooms/{code}"), true, None).await.0, StatusCode::OK);
        assert_eq!(call(&r, "GET", &format!("/api/r/{code}/state"), false, None).await.0, StatusCode::NOT_FOUND);
    }

    #[test]
    fn forwarded_for_uses_the_last_hop() {
        let mut h = HeaderMap::new();
        h.insert("x-forwarded-for", HeaderValue::from_static("1.1.1.1, 9.9.9.9"));
        assert_eq!(client_ip(&h, "127.0.0.1:1".parse().unwrap()), "9.9.9.9");
        assert_eq!(client_ip(&HeaderMap::new(), "127.0.0.1:1".parse().unwrap()), "127.0.0.1");
    }
}
