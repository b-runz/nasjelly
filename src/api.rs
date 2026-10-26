//! The slice of the Jellyfin HTTP API the Jellyfin Roku app needs for folder browsing
//! and playback. Paths arrive lowercased (see `main.rs`); query keys are matched
//! case-insensitively.

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Path as UrlPath, RawQuery, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio_util::io::ReaderStream;

use crate::config::Config;
use crate::hls::{self, Keyframes, Transcoder};
use crate::library::{self, Library, display_name, external_subs, hash_id};
use crate::playback::{self, AudioOut, Decision, DeviceProfile};
use crate::probe::{MediaInfo, Prober, Stream, StreamKind};
use crate::stream;

pub const SERVER_VERSION: &str = "10.10.7";
const TICKS_PER_SEC: f64 = 10_000_000.0;

pub struct AppState {
    pub cfg: Config,
    pub lib: Library,
    pub prober: Prober,
    pub keyframes: Keyframes,
    pub transcoder: Transcoder,
    pub server_id: String,
    pub user_id: String,
    pub token: String,
}

impl AppState {
    pub fn new(cfg: Config) -> AppState {
        let server_id = hash_id(&format!("nasjelly-server:{}", cfg.server_name));
        let user_id = hash_id(&format!("nasjelly-user:{}", cfg.username));
        // stable across restarts so the Roku stays logged in
        let token = hash_id(&format!("nasjelly-token:{}:{}", cfg.username, cfg.password));
        AppState {
            lib: Library::new(&cfg.folders),
            prober: Prober::new(cfg.ffprobe.clone()),
            keyframes: Keyframes::new(cfg.ffprobe.clone()),
            transcoder: Transcoder::new(
                cfg.ffmpeg.clone(),
                cfg.transcode_dir.clone().unwrap_or_else(std::env::temp_dir).join("nasjelly-hls"),
            ),
            server_id,
            user_id,
            token,
            cfg,
        }
    }
}

type St = State<Arc<AppState>>;

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/system/info/public", get(public_info))
        .route("/system/info", get(public_info))
        .route("/system/ping", get(ping).post(ping))
        .route("/users/public", get(empty_array))
        .route("/users/authenticatebyname", post(authenticate))
        .route("/users/me", get(user_me))
        .route("/users/{uid}", get(user_me))
        .route("/displaypreferences/{id}", get(display_preferences))
        .route("/userviews", get(user_views))
        .route("/users/{uid}/views", get(user_views))
        .route("/items", get(items))
        .route("/users/{uid}/items", get(items))
        .route("/items/latest", get(empty_array))
        .route("/users/{uid}/items/latest", get(empty_array))
        .route("/useritems/resume", get(empty_items))
        .route("/users/{uid}/items/resume", get(empty_items))
        .route("/shows/nextup", get(empty_items))
        .route("/items/{id}", get(item))
        .route("/users/{uid}/items/{id}", get(user_item))
        .route("/items/{id}/playbackinfo", post(playback_info).get(playback_info))
        .route("/videos/{id}/stream", get(video_stream))
        .route("/videos/{id}/master.m3u8", get(hls_playlist))
        .route("/videos/{id}/main.m3u8", get(hls_playlist))
        .route("/videos/{id}/hls/{segment}", get(hls_segment))
        .route("/videos/{id}/subtitletracks", get(subtitle_tracks))
        .route("/videos/{id}/subtitle.vtt", get(subtitle_vtt))
        .route("/videos/{id}/{msid}/subtitles/{index}/{start}/{file}", get(subtitle))
        .route("/videos/{id}/{msid}/subtitles/{index}/{file}", get(subtitle_no_start))
        .route("/sessions", get(empty_array))
        .route("/sessions/playing", post(no_content))
        .route("/sessions/playing/progress", post(no_content))
        .route("/sessions/playing/stopped", post(no_content))
        .route("/sessions/playing/ping", post(no_content))
        .route("/sessions/capabilities", post(no_content))
        .route("/sessions/capabilities/full", post(no_content))
        .fallback(not_found)
        .layer(middleware::from_fn_with_state(state.clone(), auth))
        .layer(middleware::from_fn(log_requests))
        .layer(middleware::from_fn(cors))
        .with_state(state)
}

// ---------- plumbing ----------

/// Query string as a map with lowercased keys.
fn query_map(raw: &Option<String>) -> HashMap<String, String> {
    let pairs: Vec<(String, String)> =
        raw.as_deref().map(|q| serde_urlencoded::from_str(q).unwrap_or_default()).unwrap_or_default();
    pairs.into_iter().map(|(k, v)| (k.to_ascii_lowercase(), v)).collect()
}

/// Token from `Authorization`/`X-Emby-Authorization` (`Token="..."`), `X-Emby-Token`,
/// or the `api_key`/`ApiKey` query parameter.
pub fn request_token(headers: &HeaderMap, query: &HashMap<String, String>) -> Option<String> {
    for name in ["authorization", "x-emby-authorization"] {
        if let Some(v) = headers.get(name).and_then(|v| v.to_str().ok()) {
            for part in v.split(',') {
                let part = part.trim();
                let part = part.strip_prefix("MediaBrowser ").unwrap_or(part).trim();
                if let Some((k, val)) = part.split_once('=') {
                    if k.trim().eq_ignore_ascii_case("token") {
                        let t = val.trim().trim_matches('"');
                        if !t.is_empty() {
                            return Some(t.to_string());
                        }
                    }
                }
            }
        }
    }
    for name in ["x-emby-token", "x-mediabrowser-token"] {
        if let Some(v) = headers.get(name).and_then(|v| v.to_str().ok()) {
            return Some(v.to_string());
        }
    }
    query.get("api_key").or_else(|| query.get("apikey")).cloned()
}

fn is_public(path: &str) -> bool {
    matches!(
        path,
        "/system/info/public" | "/system/ping" | "/users/public" | "/users/authenticatebyname"
    ) || (path.starts_with("/videos/") && path.contains("/subtitles/"))
        || path.contains("/images/")
        || path == "/"
}

async fn auth(State(st): St, req: Request, next: Next) -> Response {
    let path = req.uri().path().to_string();
    if !is_public(&path) {
        let q = query_map(&req.uri().query().map(str::to_string));
        if request_token(req.headers(), &q).as_deref() != Some(st.token.as_str()) {
            return StatusCode::UNAUTHORIZED.into_response();
        }
    }
    next.run(req).await
}

/// The Chromecast receiver fetches HLS playlists and segments cross-origin, which browsers
/// (and the cast web player) only allow with CORS headers. Answers preflights before auth.
async fn cors(req: Request, next: Next) -> Response {
    let mut resp = if req.method() == axum::http::Method::OPTIONS {
        StatusCode::NO_CONTENT.into_response()
    } else {
        next.run(req).await
    };
    let h = resp.headers_mut();
    h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*".parse().unwrap());
    h.insert(header::ACCESS_CONTROL_ALLOW_METHODS, "GET, POST, OPTIONS".parse().unwrap());
    h.insert(header::ACCESS_CONTROL_ALLOW_HEADERS, "*".parse().unwrap());
    h.insert(header::ACCESS_CONTROL_EXPOSE_HEADERS, "Content-Length, Content-Range".parse().unwrap());
    resp
}

async fn log_requests(req: Request, next: Next) -> Response {
    let method = req.method().clone();
    let uri = req.uri().clone();
    let range = req.headers().get(header::RANGE).and_then(|v| v.to_str().ok()).map(str::to_string);
    let started = std::time::Instant::now();
    let resp = next.run(req).await;
    let range = range.map(|r| format!(" [{r}]")).unwrap_or_default();
    println!(
        "{} {} {}{} -> {} ({} ms)",
        unix_now(),
        method,
        uri,
        range,
        resp.status().as_u16(),
        started.elapsed().as_millis()
    );
    resp
}

fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn json_response(v: Value) -> Response {
    axum::Json(v).into_response()
}

async fn not_found() -> StatusCode {
    StatusCode::NOT_FOUND
}

async fn no_content() -> StatusCode {
    StatusCode::NO_CONTENT
}

async fn empty_array() -> Response {
    json_response(json!([]))
}

async fn empty_items() -> Response {
    json_response(json!({ "Items": [], "TotalRecordCount": 0, "StartIndex": 0 }))
}

async fn ping() -> &'static str {
    "Jellyfin Server"
}

// ---------- system & users ----------

async fn public_info(State(st): St, headers: HeaderMap) -> Response {
    let host = headers.get(header::HOST).and_then(|v| v.to_str().ok()).unwrap_or("localhost");
    json_response(json!({
        "LocalAddress": format!("http://{host}"),
        "ServerName": st.cfg.server_name,
        "Version": SERVER_VERSION,
        "ProductName": "Jellyfin Server",
        "OperatingSystem": "Linux",
        "Id": st.server_id,
        "StartupWizardCompleted": true,
    }))
}

fn user_dto(st: &AppState) -> Value {
    json!({
        "Name": st.cfg.username,
        "ServerId": st.server_id,
        "Id": st.user_id,
        "HasPassword": !st.cfg.password.is_empty(),
        "HasConfiguredPassword": !st.cfg.password.is_empty(),
        "HasConfiguredEasyPassword": false,
        "EnableAutoLogin": false,
        "Configuration": {
            "AudioLanguagePreference": "",
            "PlayDefaultAudioTrack": true,
            "SubtitleLanguagePreference": "",
            "DisplayMissingEpisodes": false,
            "GroupedFolders": [],
            "SubtitleMode": "Default",
            "DisplayCollectionsView": false,
            "EnableLocalPassword": false,
            "OrderedViews": [],
            "LatestItemsExcludes": [],
            "MyMediaExcludes": [],
            "HidePlayedInLatest": true,
            "RememberAudioSelections": true,
            "RememberSubtitleSelections": true,
            "EnableNextEpisodeAutoPlay": false
        },
        "Policy": {
            "IsAdministrator": false,
            "IsHidden": false,
            "IsDisabled": false,
            "EnableLiveTvManagement": false,
            "EnableLiveTvAccess": false,
            "EnableMediaPlayback": true,
            "EnableAudioPlaybackTranscoding": true,
            "EnableVideoPlaybackTranscoding": true,
            "EnablePlaybackRemuxing": true,
            "EnableContentDeletion": false,
            "EnableContentDownloading": false,
            "EnableAllFolders": true
        }
    })
}

async fn authenticate(State(st): St, body: Bytes) -> Response {
    let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let field = |name: &str| -> String {
        v.as_object()
            .and_then(|o| o.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)))
            .and_then(|(_, v)| v.as_str())
            .unwrap_or("")
            .to_string()
    };
    let (user, pw) = (field("Username"), field("Pw"));
    if !user.eq_ignore_ascii_case(&st.cfg.username) || pw != st.cfg.password {
        println!("login failed for user {user:?}");
        return StatusCode::UNAUTHORIZED.into_response();
    }
    json_response(json!({
        "User": user_dto(&st),
        "SessionInfo": { "Id": hash_id(&format!("session:{}", unix_now())), "UserId": st.user_id, "UserName": st.cfg.username },
        "AccessToken": st.token,
        "ServerId": st.server_id,
    }))
}

async fn user_me(State(st): St) -> Response {
    json_response(user_dto(&st))
}

async fn display_preferences(UrlPath(id): UrlPath<String>) -> Response {
    let mut prefs = serde_json::Map::new();
    prefs.insert("homesection0".into(), json!("smalllibrarytiles"));
    prefs.insert("homesection1".into(), json!("resume"));
    for i in 2..=9 {
        prefs.insert(format!("homesection{i}"), json!("none"));
    }
    json_response(json!({
        "Id": id,
        "SortBy": "SortName",
        "SortOrder": "Ascending",
        "RememberIndexing": false,
        "RememberSorting": false,
        "ShowBackdrop": false,
        "ShowSidebar": false,
        "PrimaryImageHeight": 250,
        "PrimaryImageWidth": 250,
        "ScrollDirection": "Horizontal",
        "IndexBy": null,
        "Client": "emby",
        "CustomPrefs": prefs,
    }))
}

// ---------- items ----------

fn user_data(id: &str) -> Value {
    json!({ "PlaybackPositionTicks": 0, "PlayCount": 0, "IsFavorite": false, "Played": false, "Key": id, "ItemId": id })
}

fn root_dto(st: &AppState, root: &library::Root) -> Value {
    json!({
        "Name": root.name,
        "ServerId": st.server_id,
        "Id": root.id,
        "Type": "CollectionFolder",
        "CollectionType": "folders",
        "IsFolder": true,
        "SortName": root.name.to_lowercase(),
        "ImageTags": {},
        "BackdropImageTags": [],
        "LocationType": "FileSystem",
        "UserData": user_data(&root.id),
    })
}

fn folder_dto(st: &AppState, id: &str, path: &Path) -> Value {
    if let Some(root) = st.lib.root_by_id(id) {
        return root_dto(st, root);
    }
    let name = display_name(path, true);
    json!({
        "Name": name,
        "ServerId": st.server_id,
        "Id": id,
        "Type": "Folder",
        "IsFolder": true,
        "SortName": name.to_lowercase(),
        "ImageTags": {},
        "BackdropImageTags": [],
        "LocationType": "FileSystem",
        "UserData": user_data(id),
    })
}

fn video_dto(st: &AppState, id: &str, path: &Path, info: Option<(&MediaInfo, Value)>) -> Value {
    let name = display_name(path, false);
    let mut v = json!({
        "Name": name,
        "ServerId": st.server_id,
        "Id": id,
        "Type": "Video",
        "MediaType": "Video",
        "VideoType": "VideoFile",
        "IsFolder": false,
        "SortName": name.to_lowercase(),
        "Container": playback::primary_container(path),
        "Path": path.to_string_lossy(),
        "ImageTags": {},
        "BackdropImageTags": [],
        "LocationType": "FileSystem",
        "UserData": user_data(id),
    });
    if let Some((info, source)) = info {
        let o = v.as_object_mut().unwrap();
        o.insert("RunTimeTicks".into(), json!(ticks(info.duration_secs)));
        o.insert("MediaStreams".into(), source["MediaStreams"].clone());
        if let Some(vs) = info.video() {
            o.insert("Width".into(), json!(vs.width));
            o.insert("Height".into(), json!(vs.height));
        }
        o.insert("MediaSources".into(), json!([source]));
    }
    v
}

fn ticks(secs: f64) -> i64 {
    (secs * TICKS_PER_SEC) as i64
}

/// The streams the Roku sees, in order: video, audio tracks, external .srt files.
/// `Index` is the position in this list; `ffprobe` maps back to the source stream.
pub struct StreamList {
    pub json: Vec<Value>,
    /// (exposed index, ffprobe index) for audio streams
    pub audio: Vec<(usize, usize)>,
    pub subs: Vec<(usize, PathBuf)>,
    pub default_audio: Option<usize>,
}

fn channel_layout(ch: Option<u32>) -> String {
    match ch {
        Some(1) => "mono".into(),
        Some(2) => "stereo".into(),
        Some(6) => "5.1".into(),
        Some(8) => "7.1".into(),
        Some(n) => format!("{n}ch"),
        None => String::new(),
    }
}

pub fn stream_list(id: &str, path: &Path, info: &MediaInfo) -> StreamList {
    let mut json = Vec::new();
    let mut audio = Vec::new();
    let mut subs = Vec::new();
    let mut default_audio = None;

    if let Some(v) = info.video() {
        let res = match v.height {
            Some(h) if h >= 2000 => "4K ".to_string(),
            Some(h) => format!("{h}p "),
            None => String::new(),
        };
        json.push(json!({
            "Index": 0, "Type": "Video", "Codec": v.codec,
            "Width": v.width, "Height": v.height, "BitRate": v.bit_rate,
            "DisplayTitle": format!("{res}{}", v.codec.to_uppercase()),
            "IsDefault": true, "IsForced": false, "IsExternal": false, "IsTextSubtitleStream": false,
        }));
    }
    for a in info.audio() {
        let idx = json.len();
        let lang = a.language.clone().unwrap_or_default();
        let mut title = format!("{} {}", a.codec.to_uppercase(), channel_layout(a.channels)).trim().to_string();
        if !lang.is_empty() {
            title = format!("{} - {title}", lang.to_uppercase());
        }
        if let Some(t) = &a.title {
            title = format!("{t} ({title})");
        }
        json.push(json!({
            "Index": idx, "Type": "Audio", "Codec": a.codec, "Language": a.language,
            "Title": a.title, "DisplayTitle": title, "Channels": a.channels,
            "ChannelLayout": channel_layout(a.channels), "BitRate": a.bit_rate,
            "IsDefault": a.is_default, "IsForced": a.is_forced, "IsExternal": false,
            "IsTextSubtitleStream": false,
        }));
        if default_audio.is_none() && a.is_default {
            default_audio = Some(idx);
        }
        audio.push((idx, a.index));
    }
    if default_audio.is_none() {
        default_audio = audio.first().map(|(i, _)| *i);
    }
    for sub in external_subs(path) {
        let idx = json.len();
        let lang = sub.language.clone().unwrap_or_default();
        let display = if lang.is_empty() { format!("{} - SRT", sub.title) } else { format!("{} - SRT ({})", lang.to_uppercase(), sub.title) };
        json.push(json!({
            "Index": idx, "Type": "Subtitle", "Codec": "srt", "Language": sub.language,
            "Title": sub.title, "DisplayTitle": display,
            "IsDefault": false, "IsForced": false, "IsExternal": true,
            "IsTextSubtitleStream": true, "SupportsExternalStream": true,
            "DeliveryMethod": "External",
            "DeliveryUrl": format!("/Videos/{id}/{id}/Subtitles/{idx}/0/Stream.srt"),
            "Path": sub.path.to_string_lossy(),
        }));
        subs.push((idx, sub.path));
    }
    StreamList { json, audio, subs, default_audio }
}

fn media_source(id: &str, path: &Path, info: &MediaInfo, streams: &StreamList) -> Value {
    json!({
        "Id": id,
        "Path": path.to_string_lossy(),
        "Protocol": "File",
        "Type": "Default",
        "Container": playback::primary_container(path),
        "Size": info.size,
        "Name": display_name(path, false),
        "IsRemote": false,
        "VideoType": "VideoFile",
        "RunTimeTicks": ticks(info.duration_secs),
        "Bitrate": info.bit_rate,
        "SupportsDirectPlay": true,
        "SupportsDirectStream": true,
        "SupportsTranscoding": true,
        "SupportsProbing": true,
        "RequiresOpening": false,
        "RequiresClosing": false,
        "IsInfiniteStream": false,
        "MediaStreams": streams.json,
        "DefaultAudioStreamIndex": streams.default_audio,
    })
}

async fn video_info(st: &AppState, id: &str) -> Result<(PathBuf, MediaInfo), Response> {
    let Some(path) = st.lib.resolve(id) else {
        return Err(StatusCode::NOT_FOUND.into_response());
    };
    if !path.is_file() {
        return Err(StatusCode::NOT_FOUND.into_response());
    }
    match st.prober.probe(&path).await {
        Ok(info) => Ok((path, info)),
        Err(e) => {
            println!("probe error: {e}");
            Err(StatusCode::INTERNAL_SERVER_ERROR.into_response())
        }
    }
}

async fn user_views(State(st): St) -> Response {
    let items: Vec<Value> = st.lib.roots.iter().map(|r| root_dto(&st, r)).collect();
    let n = items.len();
    json_response(json!({ "Items": items, "TotalRecordCount": n, "StartIndex": 0 }))
}

async fn user_item(st: St, UrlPath((_uid, id)): UrlPath<(String, String)>) -> Response {
    item_response(&st, &id).await
}

async fn item(st: St, UrlPath(id): UrlPath<String>) -> Response {
    item_response(&st, &id).await
}

async fn item_response(st: &AppState, id: &str) -> Response {
    let Some(path) = st.lib.resolve(id) else { return StatusCode::NOT_FOUND.into_response() };
    if path.is_dir() {
        return json_response(folder_dto(st, id, &path));
    }
    match video_info(st, id).await {
        Ok((path, info)) => {
            let streams = stream_list(id, &path, &info);
            let source = media_source(id, &path, &info, &streams);
            json_response(video_dto(st, id, &path, Some((&info, source))))
        }
        Err(r) => r,
    }
}

async fn items(State(st): St, RawQuery(raw): RawQuery) -> Response {
    let q = query_map(&raw);
    let get = |k: &str| q.get(k).map(String::as_str).unwrap_or("");

    // Favorites, recursive searches and type-filtered home rows have nothing to show here.
    if get("recursive").eq_ignore_ascii_case("true")
        || get("filters").to_ascii_lowercase().contains("isfavorite")
        || !get("ids").is_empty() && get("parentid").is_empty()
    {
        return empty_items().await;
    }
    let parent = get("parentid");
    if parent.is_empty() {
        return empty_items().await;
    }
    let Some(dir) = st.lib.resolve(parent) else { return StatusCode::NOT_FOUND.into_response() };
    let entries = match st.lib.list_dir(&dir) {
        Ok(e) => e,
        Err(e) => {
            println!("cannot list {}: {e}", dir.display());
            return empty_items().await;
        }
    };

    let mut rows: Vec<(library::Entry, String)> = entries
        .into_iter()
        .map(|e| {
            let name = display_name(&e.path, e.is_dir);
            (e, name)
        })
        .collect();

    let lower = |s: &str| s.to_lowercase();
    let starts = lower(get("namestartswith"));
    let less = lower(get("namelessthan"));
    let ge = lower(get("namestartswithorgreater"));
    let term = lower(get("searchterm"));
    rows.retain(|(_, name)| {
        let n = lower(name);
        (starts.is_empty() || n.starts_with(&starts))
            && (less.is_empty() || n < less)
            && (ge.is_empty() || n >= ge)
            && (term.is_empty() || n.contains(&term))
    });

    let sort_by = lower(get("sortby"));
    if sort_by.contains("datecreated") || sort_by.contains("datelastcontentadded") {
        rows.sort_by(|a, b| a.0.modified.cmp(&b.0.modified));
    } else {
        rows.sort_by(|a, b| b.0.is_dir.cmp(&a.0.is_dir).then_with(|| lower(&a.1).cmp(&lower(&b.1))));
    }
    if get("sortorder").eq_ignore_ascii_case("descending") {
        rows.reverse();
    }

    let total = rows.len();
    let start: usize = get("startindex").parse().unwrap_or(0);
    let limit: usize = get("limit").parse().unwrap_or(usize::MAX);
    let page: Vec<Value> = rows
        .iter()
        .skip(start)
        .take(limit)
        .map(|(e, _)| {
            if e.is_dir {
                folder_dto(&st, &e.id, &e.path)
            } else {
                video_dto(&st, &e.id, &e.path, None)
            }
        })
        .collect();
    json_response(json!({ "Items": page, "TotalRecordCount": total, "StartIndex": start }))
}

// ---------- playback ----------

fn pick_audio<'a>(info: &'a MediaInfo, streams: &StreamList, requested: Option<usize>) -> Option<(usize, &'a Stream)> {
    let exposed = requested
        .filter(|r| streams.audio.iter().any(|(i, _)| i == r))
        .or(streams.default_audio)?;
    let (_, ff) = streams.audio.iter().find(|(i, _)| *i == exposed)?;
    info.streams.iter().find(|s| s.index == *ff).map(|s| (exposed, s))
}

async fn playback_info(State(st): St, UrlPath(id): UrlPath<String>, RawQuery(raw): RawQuery, body: Bytes) -> Response {
    let q = query_map(&raw);
    let (path, info) = match video_info(&st, &id).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let profile: DeviceProfile = body
        .get("DeviceProfile")
        .and_then(|p| serde_json::from_value(p.clone()).ok())
        .unwrap_or_default();
    let requested_audio = q.get("audiostreamindex").and_then(|v| v.parse().ok());

    let streams = stream_list(&id, &path, &info);
    let audio = pick_audio(&info, &streams, requested_audio);
    let decision = playback::decide(&path, &info, audio.map(|(_, s)| s), &profile);
    let play_session = hash_id(&format!("{id}:{}:{:?}", unix_now(), std::time::Instant::now()));

    let mut source = media_source(&id, &path, &info, &streams);
    let o = source.as_object_mut().unwrap();
    if let Some((exposed, _)) = audio {
        o.insert("DefaultAudioStreamIndex".into(), json!(exposed));
    }
    match &decision {
        Decision::DirectPlay => {}
        Decision::Remux { audio: audio_out, reasons } => {
            o.insert("SupportsDirectPlay".into(), json!(false));
            o.insert("SupportsDirectStream".into(), json!(false));
            let (mode, channels) = match audio_out {
                Some(AudioOut::Copy) | None => ("copy", 0),
                Some(AudioOut::Ac3 { channels }) => ("ac3", *channels),
                Some(AudioOut::Aac { channels }) => ("aac", *channels),
            };
            let mut url = format!(
                "/videos/{id}/master.m3u8?MediaSourceId={id}&PlaySessionId={play_session}&ApiKey={}&AudioOut={mode}&AudioChannels={channels}",
                st.token
            );
            if let Some((exposed, _)) = audio {
                url.push_str(&format!("&AudioStreamIndex={exposed}"));
            }
            url.push_str(&format!("&TranscodeReasons={}", reasons.join(",")));
            o.insert("TranscodingUrl".into(), json!(url));
            o.insert("TranscodingSubProtocol".into(), json!("hls"));
            o.insert("TranscodingContainer".into(), json!("ts"));
        }
        Decision::Unplayable { reasons } => {
            println!("{}: cannot play without re-encoding video ({})", path.display(), reasons.join(","));
            o.insert("SupportsDirectPlay".into(), json!(false));
            o.insert("SupportsDirectStream".into(), json!(false));
            o.insert("SupportsTranscoding".into(), json!(false));
        }
    }
    println!("playback {}: {:?}", path.display(), decision);
    json_response(json!({ "MediaSources": [source], "PlaySessionId": play_session }))
}

async fn video_stream(State(st): St, UrlPath(id): UrlPath<String>, headers: HeaderMap) -> Response {
    match st.lib.resolve(&id) {
        Some(path) if path.is_file() => stream::serve_file(&path, &headers).await,
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn segments_for(st: &AppState, path: &Path, info: &MediaInfo) -> Vec<(f64, f64)> {
    let kf = st.keyframes.get(path).await;
    match kf.as_ref() {
        Some(k) => hls::plan_segments(k, info.duration_secs, hls::TARGET_SEGMENT_SECS),
        None => hls::fixed_segments(info.duration_secs, hls::TARGET_SEGMENT_SECS),
    }
}

async fn hls_playlist(State(st): St, UrlPath(id): UrlPath<String>, RawQuery(raw): RawQuery) -> Response {
    let (path, info) = match video_info(&st, &id).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    let segs = segments_for(&st, &path, &info).await;
    let body = hls::playlist(&segs, raw.as_deref().unwrap_or(""));
    ([(header::CONTENT_TYPE, "application/vnd.apple.mpegurl")], body).into_response()
}

async fn hls_segment(
    State(st): St,
    UrlPath((id, segment)): UrlPath<(String, String)>,
    RawQuery(raw): RawQuery,
) -> Response {
    let q = query_map(&raw);
    let Some(n) = segment.trim_end_matches(".ts").parse::<usize>().ok() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let (path, info) = match video_info(&st, &id).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    let kf = st.keyframes.get(&path).await;
    let segs = segments_for(&st, &path, &info).await;
    let Some(video) = info.video() else { return StatusCode::NOT_FOUND.into_response() };

    let streams = stream_list(&id, &path, &info);
    let requested = q.get("audiostreamindex").and_then(|v| v.parse().ok());
    let channels: u32 = q.get("audiochannels").and_then(|v| v.parse().ok()).unwrap_or(2);
    let audio_out = match q.get("audioout").map(String::as_str) {
        Some("ac3") => AudioOut::Ac3 { channels },
        Some("aac") => AudioOut::Aac { channels },
        _ => AudioOut::Copy,
    };
    let audio = pick_audio(&info, &streams, requested).map(|(_, s)| (s.index, &audio_out));
    let spec = hls::JobSpec {
        key: format!("{id}:{:?}:{:?}", audio.map(|(i, _)| i), audio_out),
        path: &path,
        segments: &segs,
        keyframes: kf.as_deref(),
        video_index: video.index,
        audio,
    };
    let file = match st.transcoder.segment(&spec, n).await {
        Ok(f) => f,
        Err(e) => {
            println!("transcode: {e}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    match tokio::fs::File::open(&file).await {
        Ok(f) => {
            let len = f.metadata().await.map(|m| m.len()).unwrap_or(0);
            (
                [(header::CONTENT_TYPE, "video/mp2t".to_string()), (header::CONTENT_LENGTH, len.to_string())],
                Body::from_stream(ReaderStream::with_capacity(f, 64 * 1024)),
            )
                .into_response()
        }
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn subtitle(
    st: St,
    UrlPath((id, _msid, index, _start, _file)): UrlPath<(String, String, usize, String, String)>,
) -> Response {
    subtitle_response(&st, &id, index).await
}

async fn subtitle_no_start(
    st: St,
    UrlPath((id, _msid, index, _file)): UrlPath<(String, String, usize, String)>,
) -> Response {
    subtitle_response(&st, &id, index).await
}

async fn subtitle_response(st: &AppState, id: &str, index: usize) -> Response {
    let (path, info) = match video_info(st, id).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    let streams = stream_list(id, &path, &info);
    let Some((_, sub_path)) = streams.subs.iter().find(|(i, _)| *i == index) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match tokio::fs::read(sub_path).await {
        Ok(bytes) => (
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            to_utf8(&bytes),
        )
            .into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

/// Text subtitle codecs ffmpeg can convert to WebVTT. Bitmap formats (PGS, VobSub) need OCR.
const TEXT_SUB_CODECS: [&str; 7] = ["subrip", "srt", "ass", "ssa", "webvtt", "mov_text", "text"];

/// Subtitle tracks a Chromecast can show. Ids are `e<ffprobe index>` for tracks inside the
/// file and `x<n>` for external .srt files; `subtitle_vtt` takes them as `track`.
async fn subtitle_tracks(State(st): St, UrlPath(id): UrlPath<String>) -> Response {
    let (path, info) = match video_info(&st, &id).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    let mut tracks = Vec::new();
    for s in info.streams.iter().filter(|s| s.kind == StreamKind::Subtitle) {
        if !TEXT_SUB_CODECS.contains(&s.codec.as_str()) {
            continue;
        }
        let lang = s.language.clone().unwrap_or_default();
        let mut title = s.title.clone().unwrap_or_else(|| format!("{} {}", lang.to_uppercase(), s.codec).trim().to_string());
        if s.is_forced {
            title.push_str(" (forced)");
        }
        tracks.push(json!({ "id": format!("e{}", s.index), "title": title, "language": s.language, "external": false }));
    }
    for (i, sub) in external_subs(&path).iter().enumerate() {
        let title = format!("{} (file)", sub.title);
        tracks.push(json!({ "id": format!("x{i}"), "title": title, "language": sub.language, "external": true }));
    }
    json_response(json!({ "Tracks": tracks }))
}

async fn subtitle_vtt(State(st): St, UrlPath(id): UrlPath<String>, RawQuery(raw): RawQuery) -> Response {
    let q = query_map(&raw);
    let Some(track) = q.get("track") else { return StatusCode::BAD_REQUEST.into_response() };
    let (path, info) = match video_info(&st, &id).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    let vtt = if let Some(n) = track.strip_prefix('x').and_then(|n| n.parse::<usize>().ok()) {
        let subs = external_subs(&path);
        let Some(sub) = subs.get(n) else { return StatusCode::NOT_FOUND.into_response() };
        match tokio::fs::read(&sub.path).await {
            Ok(bytes) => crate::vtt::srt_to_vtt(&to_utf8(&bytes)),
            Err(_) => return StatusCode::NOT_FOUND.into_response(),
        }
    } else if let Some(n) = track.strip_prefix('e').and_then(|n| n.parse::<usize>().ok()) {
        let is_text_sub = info
            .streams
            .iter()
            .any(|s| s.index == n && s.kind == StreamKind::Subtitle && TEXT_SUB_CODECS.contains(&s.codec.as_str()));
        if !is_text_sub {
            return StatusCode::NOT_FOUND.into_response();
        }
        // reads the whole file once; cheap on CPU since only the subtitle stream is converted
        let out = tokio::process::Command::new(&st.cfg.ffmpeg)
            .args(["-nostdin", "-v", "error", "-i"])
            .arg(&path)
            .args(["-map", &format!("0:{n}"), "-f", "webvtt", "-"])
            .stdin(std::process::Stdio::null())
            .output()
            .await;
        match out {
            Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
            Ok(o) => {
                println!("subtitle extract: {}", String::from_utf8_lossy(&o.stderr).trim());
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
            Err(e) => {
                println!("subtitle extract: cannot run ffmpeg: {e}");
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
        }
    } else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    ([(header::CONTENT_TYPE, "text/vtt; charset=utf-8")], vtt).into_response()
}

/// Subtitle files are often Windows-1252/Latin-1; the Roku wants UTF-8.
pub fn to_utf8(bytes: &[u8]) -> String {
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(_) => bytes.iter().map(|&b| b as char).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn token_from_mediabrowser_header() {
        let mut h = HeaderMap::new();
        h.insert(
            "authorization",
            HeaderValue::from_static(r#"MediaBrowser Client="Jellyfin Roku", Device="Roku", DeviceId="x", Version="3", Token="abc123""#),
        );
        assert_eq!(request_token(&h, &HashMap::new()).as_deref(), Some("abc123"));
    }

    #[test]
    fn token_from_query() {
        let q = query_map(&Some("ApiKey=zz&Other=1".into()));
        assert_eq!(request_token(&HeaderMap::new(), &q).as_deref(), Some("zz"));
    }

    #[test]
    fn latin1_subtitles_become_utf8() {
        assert_eq!(to_utf8(&[0x66, 0xE6, 0x72]), "fær");
        assert_eq!(to_utf8("fær".as_bytes()), "fær");
        assert_eq!(to_utf8(&[0xEF, 0xBB, 0xBF, b'a']), "a");
    }
}
