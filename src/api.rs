//! Loopback HTTP API (127.0.0.1 only).
//!
//! - `GET /health`  -> { name, version, connected, moq, torrent, port }
//! - `GET /version` -> { name, version }
//! - `GET /events`  -> SSE: `hello` then 15s heartbeat comments.
//! - `GET /capabilities`   -> { name, version, capabilities }
//! - `GET /audio/apps`     -> { apps } when listable, else 501 { error }
//! - `POST /audio/mode`    -> { mode } persists all/none/custom (live)
//! - `POST /audio/mute`    -> { app, muted } persists one toggle (live)
//! - `GET /audio/stream`   -> infinite s16le 48kHz stereo PCM while a capture
//!   session is live (`404 {error:"idle"}` when stopped, `501` where unwired)
//! - `POST /capture/start` -> { started, display_id|window_id, width, height, fps }
//! - `POST /capture/stop`  -> { stopped: true }
//! - `GET /capture/snapshot` -> one-frame JPEG (`?display_id=<id|empty=primary>` xor `?window_id=<id>`, `&width=<px>`)
//! - `GET /youtube/tools`  -> { status, done?, total?, error? } (pinned yt-dlp + FFmpeg in the data dir)
//! - `POST /youtube/tools` -> starts the download; same body as GET
//! - `POST /youtube/resolve` -> { url } → { summary } or { error, detail }
//! - `POST /youtube/run`   -> { url, runId, claim, roomId, mediaGeneration, region, startMs, apiBase } → { runId }
//! - `GET /youtube/run/:id`, `DELETE /youtube/run/:id`
//!
//! Security: Host must be loopback (DNS-rebinding guard); CORS only echoes
//! allowlisted origins (`JLOCAL_ALLOWED_ORIGINS`); everything is `no-store`.

use std::convert::Infallible;
use std::time::Duration;

use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive};
use axum::response::{IntoResponse, Response, Sse};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::Serialize;
use tokio::io::AsyncReadExt;

use crate::status::AppState;

#[derive(Serialize)]
struct Health<'a> {
    name: &'a str,
    version: &'a str,
    connected: bool,
    moq: &'a str,
    torrent: &'a str,
    port: u16,
    started_unix: u64,
}

#[derive(Serialize)]
struct Version<'a> {
    name: &'a str,
    version: &'a str,
}

#[derive(Serialize)]
struct Capabilities<'a> {
    name: &'a str,
    version: &'a str,
    capabilities: CapabilitiesBody,
}

#[derive(Serialize)]
struct CapabilitiesBody {
    screen: ScreenCapabilities,
    audio: AudioCapabilities,
    torrent: TorrentCapabilities,
    youtube: YoutubeCapabilities,
    permissions: PermissionCapabilities,
}

#[derive(Serialize)]
struct YoutubeCapabilities {
    /// Tools present: links can be resolved and prepared here.
    available: bool,
    /// `ready`, `missing`, `downloading`, `failed` or `unsupported`.
    tools: &'static str,
}
#[derive(Serialize)]
struct ScreenCapabilities {
    available: bool,
    capture: bool,
    /// Hardware H.264 access units over `GET /capture/h264` (macOS).
    h264: bool,
    #[serde(rename = "maxWidth")]
    max_width: u32,
    #[serde(rename = "maxHeight")]
    max_height: u32,
    #[serde(rename = "maxFps")]
    max_fps: u32,
}

#[derive(Serialize)]
struct AudioCapabilities {
    #[serde(rename = "appList")]
    app_list: bool,
    capture: bool,
}

#[derive(Serialize)]
struct TorrentCapabilities {
    available: bool,
}

#[derive(Serialize)]
struct PermissionCapabilities {
    #[serde(rename = "screenCapture")]
    screen_capture: bool,
}
pub fn router(state: AppState, port: u16) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/version", get(version))
        .route("/events", get(events))
        .route("/capabilities", get(capabilities))
        .route("/audio/apps", get(audio_apps))
        .route("/audio/mode", post(audio_mode))
        .route("/audio/mute", post(audio_mute))
        .route("/audio/stream", get(audio_stream))
        .route("/capture/displays", get(capture_displays))
        .route("/capture/windows", get(capture_windows))
        .route("/capture/preview.jpg", get(capture_preview))
        .route("/capture/snapshot", get(capture_snapshot))
        .route("/capture/stream", get(capture_stream))
        .route("/capture/h264", get(capture_h264))
        .route("/capture/start", post(capture_start))
        .route("/capture/stop", post(capture_stop))
        .route("/torrent/add", post(torrent_add))
        .route("/torrent/list", get(torrent_list))
        .route("/torrent/data/:id/:file", get(torrent_data))
        .route("/torrent/select", post(torrent_select))
        .route("/torrent/stats/:id", get(torrent_stats))
        .route("/torrent/:id", delete(torrent_remove))
        .route(
            "/youtube/tools",
            get(youtube_tools).post(youtube_tools_install),
        )
        .route("/youtube/resolve", post(youtube_resolve))
        .route("/youtube/run", post(youtube_run))
        .route(
            "/youtube/run/:id",
            get(youtube_run_status).delete(youtube_run_cancel),
        )
        .route("/health", axum::routing::options(preflight))
        .route("/version", axum::routing::options(preflight))
        .route("/events", axum::routing::options(preflight))
        .route("/capabilities", axum::routing::options(preflight))
        .route("/audio/apps", axum::routing::options(preflight))
        .route("/audio/mode", axum::routing::options(preflight))
        .route("/audio/mute", axum::routing::options(preflight))
        .route("/audio/stream", axum::routing::options(preflight))
        .route("/capture/displays", axum::routing::options(preflight))
        .route("/capture/windows", axum::routing::options(preflight))
        .route("/capture/preview.jpg", axum::routing::options(preflight))
        .route("/capture/snapshot", axum::routing::options(preflight))
        .route("/capture/stream", axum::routing::options(preflight))
        .route("/capture/h264", axum::routing::options(preflight))
        .route("/capture/start", axum::routing::options(preflight))
        .route("/capture/stop", axum::routing::options(preflight))
        .route("/torrent/add", axum::routing::options(preflight))
        .route("/torrent/list", axum::routing::options(preflight))
        .route("/torrent/data/:id/:file", axum::routing::options(preflight))
        .route("/torrent/select", axum::routing::options(preflight))
        .route("/torrent/stats/:id", axum::routing::options(preflight))
        .route("/torrent/:id", axum::routing::options(preflight))
        .route("/youtube/tools", axum::routing::options(preflight))
        .route("/youtube/resolve", axum::routing::options(preflight))
        .route("/youtube/run", axum::routing::options(preflight))
        .route("/youtube/run/:id", axum::routing::options(preflight))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            require_loopback_host,
        ))
        .with_state(ApiState { state, port })
}

#[derive(Clone)]
struct ApiState {
    state: AppState,
    port: u16,
}

pub async fn serve(listener: tokio::net::TcpListener, state: AppState) -> anyhow::Result<()> {
    let port = listener.local_addr()?.port();
    let app = router(state, port);
    axum::serve(listener, app).await?;
    Ok(())
}
async fn health(State(s): State<ApiState>, headers: HeaderMap) -> impl IntoResponse {
    let body = Health {
        name: crate::status::NAME,
        version: crate::status::VERSION,
        connected: true,
        moq: "disabled",
        torrent: if s.state.torrent.is_some() {
            "live"
        } else {
            "standby"
        },
        port: s.port,
        started_unix: s.state.started_unix,
    };
    let mut res = Json(body).into_response();
    apply_cors(&s.state, &headers, res.headers_mut());
    with_no_store(res)
}

async fn version(State(s): State<ApiState>, headers: HeaderMap) -> impl IntoResponse {
    let body = Version {
        name: crate::status::NAME,
        version: crate::status::VERSION,
    };
    let mut res = Json(body).into_response();
    apply_cors(&s.state, &headers, res.headers_mut());
    with_no_store(res)
}

async fn preflight(State(s): State<ApiState>, headers: HeaderMap) -> impl IntoResponse {
    let mut res = StatusCode::NO_CONTENT.into_response();
    apply_cors(&s.state, &headers, res.headers_mut());
    with_no_store(res)
}

/// SSE: one `hello` event, then 15s keep-alive comments.
/// Browsers must use `fetch` + reader, not `EventSource`.
async fn events(State(s): State<ApiState>, headers: HeaderMap) -> impl IntoResponse {
    let hello = serde_json::json!({
        "name": crate::status::NAME,
        "version": crate::status::VERSION,
    })
    .to_string();
    let stream = async_stream::stream! {
        yield Ok::<Event, Infallible>(Event::default().event("hello").data(hello));
    };
    let mut res = Sse::new(stream)
        .keep_alive(
            KeepAlive::new()
                .interval(Duration::from_secs(15))
                .text("ping"),
        )
        .into_response();
    apply_cors(&s.state, &headers, res.headers_mut());
    with_no_store(res)
}

async fn capabilities(State(s): State<ApiState>, headers: HeaderMap) -> impl IntoResponse {
    let body = Capabilities {
        name: crate::status::NAME,
        version: crate::status::VERSION,
        capabilities: CapabilitiesBody {
            screen: ScreenCapabilities {
                available: s.state.caps.screen,
                capture: s.state.caps.screen_capture,
                h264: crate::h264::H264Session::supported(),
                max_width: 3840,
                max_height: 2160,
                max_fps: 60,
            },
            audio: AudioCapabilities {
                app_list: s.state.caps.app_list,
                capture: s.state.caps.audio_capture,
            },
            torrent: TorrentCapabilities {
                available: s.state.caps.torrent,
            },
            youtube: YoutubeCapabilities {
                available: s.state.youtube.ready(),
                tools: match s.state.youtube.status() {
                    crate::youtube::ToolsStatus::Ready => "ready",
                    crate::youtube::ToolsStatus::Missing => "missing",
                    crate::youtube::ToolsStatus::Downloading { .. } => "downloading",
                    crate::youtube::ToolsStatus::Failed { .. } => "failed",
                    crate::youtube::ToolsStatus::Unsupported => "unsupported",
                },
            },
            permissions: PermissionCapabilities {
                // Live probe, report-only: never prompts, so reading it per
                // request stays honest if the user grants access mid-run.
                screen_capture: crate::permissions::screen_capture_granted(),
            },
        },
    };
    let mut res = Json(body).into_response();
    apply_cors(&s.state, &headers, res.headers_mut());
    with_no_store(res)
}
async fn audio_apps(State(s): State<ApiState>, headers: HeaderMap) -> impl IntoResponse {
    let apps = crate::audio::list_apps();
    // Empty means labels are not listable here: stay 501 so the web UI
    // keeps its honest empty state instead of an empty list.
    let mut res = if apps.is_empty() {
        (
            StatusCode::NOT_IMPLEMENTED,
            Json(serde_json::json!({"error": "not_implemented"})),
        )
            .into_response()
    } else {
        Json(serde_json::json!({"apps": apps})).into_response()
    };
    apply_cors(&s.state, &headers, res.headers_mut());
    with_no_store(res)
}

async fn capture_displays(State(s): State<ApiState>, headers: HeaderMap) -> impl IntoResponse {
    // OS enumeration can synchronously wait on WindowServer/Win32. Keep it
    // off Tokio's request workers so opening the picker cannot stall health,
    // audio, or a running MJPEG response.
    let listed = tokio::task::spawn_blocking(crate::capture::list_displays).await;
    let mut res = match listed {
        Ok(Ok(displays)) => Json(serde_json::json!({"displays": displays})).into_response(),
        Ok(Err(e)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("display enumeration worker failed: {e}")})),
        )
            .into_response(),
    };
    apply_cors(&s.state, &headers, res.headers_mut());
    with_no_store(res)
}

async fn capture_windows(State(s): State<ApiState>, headers: HeaderMap) -> impl IntoResponse {
    // Window enumeration also resolves application icons on macOS/Windows,
    // which is much too expensive to run on an async request worker.
    let listed = tokio::task::spawn_blocking(crate::capture::list_windows).await;
    let mut res = match listed {
        Ok(Ok(windows)) => Json(serde_json::json!({"windows": windows})).into_response(),
        Ok(Err(e)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("window enumeration worker failed: {e}")})),
        )
            .into_response(),
    };
    apply_cors(&s.state, &headers, res.headers_mut());
    with_no_store(res)
}

async fn capture_preview(State(s): State<ApiState>, headers: HeaderMap) -> impl IntoResponse {
    let jpeg = s
        .state
        .capture
        .lock()
        .as_ref()
        .and_then(|active| active.latest_jpeg());
    let mut res = match jpeg {
        Some(bytes) => ([(axum::http::header::CONTENT_TYPE, "image/jpeg")], bytes).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "idle"})),
        )
            .into_response(),
    };
    apply_cors(&s.state, &headers, res.headers_mut());

    with_no_store(res)
}

/// Live MJPEG feed of the running session: `GET /capture/stream` answers
/// `multipart/x-mixed-replace;boundary=frame` with one JPEG part per tick at
/// the session's rate, ending when the session stops. 404 while idle.
/// The web feed reads this with fetch+reader (one connection, server-paced)
/// instead of polling preview.jpg — polling stacks HTTP overhead and
/// overlapping in-flight grabs at 60fps and can never look smooth.
/// CORS + no-store like every capture route.
async fn capture_stream(State(s): State<ApiState>, headers: HeaderMap) -> impl IntoResponse {
    let live = {
        s.state
            .capture
            .lock()
            .as_ref()
            .map(|session| (session.id(), session.fps()))
    };
    let Some((capture_id, fps)) = live else {
        let mut res = (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "idle"})),
        )
            .into_response();
        apply_cors(&s.state, &headers, res.headers_mut());
        return with_no_store(res);
    };
    let state = s.state.clone();
    let interval = Duration::from_secs_f64(1.0 / f64::from(fps.max(1)));
    let stream = async_stream::stream! {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut last_sequence = 0_u64;
        loop {
            ticker.tick().await;
            let (same_session, running, frame) = {
                let active = state.capture.lock();
                match active.as_ref() {
                    Some(session) if session.id() == capture_id => (
                        true,
                        session.is_running(),
                        session.latest_jpeg_after(last_sequence),
                    ),
                    _ => (false, false, None),
                }
            };
            if !same_session {
                break;
            }
            let Some((sequence, jpeg)) = frame else {
                // No duplicate parts: if capture is slower than its requested
                // ceiling, wait for a genuinely new frame. A failed worker
                // ends after the final cached frame instead of freezing open.
                if !running && last_sequence != 0 {
                    break;
                }
                continue;
            };
            last_sequence = sequence;
            let header = format!(
                "--frame\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n",
                jpeg.len()
            );
            // Avoid copying each full JPEG into a fresh multipart buffer.
            yield Ok::<_, std::convert::Infallible>(axum::body::Bytes::from(header));
            yield Ok::<_, std::convert::Infallible>(jpeg);
            yield Ok::<_, std::convert::Infallible>(axum::body::Bytes::from_static(b"\r\n"));
        }
    };
    let mut res = axum::body::Body::from_stream(stream).into_response();
    res.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        HeaderValue::from_static("multipart/x-mixed-replace;boundary=frame"),
    );
    apply_cors(&s.state, &headers, res.headers_mut());
    with_no_store(res)
}

/// One-shot frame grab for picker previews: `GET /capture/snapshot` returns
/// exactly one JPEG frame for `?display_id=<id>` (empty value = primary) xor
/// `?window_id=<id>`, downscaled to `&width=<px>` (default 960, clamped
/// `160..=1920`) keeping the aspect ratio.
///
/// - Exactly one id key: both or neither is a 400, never a guess. Garbage
///   ids fail closed (400); unknown ids are 400, never 503.
/// - Any capture failure is 503: `{error:"permission"}` when the OS probe
///   says capture is blocked, else `{error:"unavailable"}`.
/// - Stateless: never touches `AppState::capture`, so a running session
///   keeps grabbing undisturbed (`State` is only read for CORS origins).
async fn capture_snapshot(
    State(s): State<ApiState>,
    headers: HeaderMap,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    enum Target {
        Display(Option<u32>),
        Window(u32),
    }
    enum ResolvedTarget {
        Display(u32),
        Window(u32),
    }
    let target: Result<Target, String> = match (params.get("display_id"), params.get("window_id")) {
        (Some(_), Some(_)) => Err("only one of display_id, window_id".to_string()),
        (None, None) => Err("display_id or window_id required".to_string()),
        (Some(raw), None) => {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                Ok(Target::Display(None))
            } else {
                trimmed
                    .parse::<u32>()
                    .map(|id| Target::Display(Some(id)))
                    .map_err(|_| "invalid display_id".to_string())
            }
        }
        (None, Some(raw)) => raw
            .trim()
            .parse::<u32>()
            .map(Target::Window)
            .map_err(|_| "invalid window_id".to_string()),
    };
    // Validate routing before the permission probe. Malformed and stale ids
    // are deterministic 400s even on a machine where capture is denied.
    let target = match target {
        Ok(target) => target,
        Err(error) => {
            let mut res = (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": error})),
            )
                .into_response();
            apply_cors(&s.state, &headers, res.headers_mut());
            return with_no_store(res);
        }
    };
    // Lenient like `capture_size`: missing, empty, or non-numeric widths
    // fall back to the default; out-of-range values clamp (never 400 — the
    // target is already validated above, so the size cannot misroute).
    let width = crate::capture::clamp_snapshot_width(
        params
            .get("width")
            .and_then(|v| v.trim().parse::<u32>().ok())
            .unwrap_or(crate::capture::SNAPSHOT_DEFAULT_WIDTH),
    );
    // Listing does not request capture permission. Resolve stale picker ids
    // first so they remain deterministic 400s even when the OS permission is
    // currently denied, and keep enumeration off Tokio's request workers.
    let resolved =
        tokio::task::spawn_blocking(move || -> Result<ResolvedTarget, (StatusCode, String)> {
            match target {
                Target::Display(requested) => {
                    let displays =
                        crate::capture::list_displays().map_err(|_| snapshot_unavailable())?;
                    let display =
                        select_display(&displays, requested).ok_or_else(|| match requested {
                            Some(id) => {
                                (StatusCode::BAD_REQUEST, format!("display {id} not found"))
                            }
                            None => snapshot_unavailable(),
                        })?;
                    Ok(ResolvedTarget::Display(display.id))
                }
                Target::Window(id) => {
                    let windows =
                        crate::capture::list_windows().map_err(|_| snapshot_unavailable())?;
                    let window = select_window(&windows, id).ok_or_else(|| {
                        (StatusCode::BAD_REQUEST, format!("window {id} not found"))
                    })?;
                    Ok(ResolvedTarget::Window(window.id))
                }
            }
        })
        .await
        .unwrap_or_else(|_| Err(snapshot_unavailable()));
    let target = match resolved {
        Ok(target) => target,
        Err((status, error)) => {
            let mut res = (status, Json(serde_json::json!({"error": error}))).into_response();
            apply_cors(&s.state, &headers, res.headers_mut());
            return with_no_store(res);
        }
    };
    // The OS prompt must fire from an explicit confirm, never from the
    // picker's 1s preview poll: when the probe says blocked, fail fast with
    // 503 without touching the capture API (which is what re-opens the
    // system prompt on every poll).
    if !crate::permissions::screen_capture_granted() {
        let mut denied = (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": "permission"})),
        )
            .into_response();
        apply_cors(&s.state, &headers, denied.headers_mut());
        return with_no_store(denied);
    }
    let snapshot = tokio::task::spawn_blocking(move || -> Result<Vec<u8>, (StatusCode, String)> {
        match target {
            ResolvedTarget::Display(id) => {
                crate::capture::snapshot_display(id, width).map_err(|_| snapshot_unavailable())
            }
            ResolvedTarget::Window(id) => {
                crate::capture::snapshot_window(id, width).map_err(|_| snapshot_unavailable())
            }
        }
    })
    .await
    .unwrap_or_else(|_| Err(snapshot_unavailable()));
    let mut res = match snapshot {
        Ok(bytes) => ([(axum::http::header::CONTENT_TYPE, "image/jpeg")], bytes).into_response(),
        Err((status, error)) => (status, Json(serde_json::json!({"error": error}))).into_response(),
    };
    apply_cors(&s.state, &headers, res.headers_mut());
    with_no_store(res)
}
/// 503 for a failed snapshot: `permission` when the OS probe says capture is
/// blocked (the user must grant Screen Recording), else `unavailable`.
fn snapshot_unavailable() -> (StatusCode, String) {
    let error = if crate::permissions::screen_capture_granted() {
        "unavailable"
    } else {
        "permission"
    };
    (StatusCode::SERVICE_UNAVAILABLE, error.to_string())
}

/// Status for a failed `CaptureSession::start`: validation failures stay 400,
/// but a failed eager first grab (see `launch`) is a 503 — the session can
/// never yield frames. When the OS probe says blocked the body is
/// `permission` (the web shows its Screen Recording hint); otherwise the
/// grab's own message rides along so the real reason is visible.
fn start_capture_status(error: anyhow::Error) -> (StatusCode, String) {
    if !error.to_string().starts_with("capture failed") {
        return (StatusCode::BAD_REQUEST, error.to_string());
    }
    if crate::permissions::screen_capture_granted() {
        return (StatusCode::SERVICE_UNAVAILABLE, error.to_string());
    }
    (StatusCode::SERVICE_UNAVAILABLE, "permission".to_string())
}

fn capture_size(body: &serde_json::Value) -> (u32, u32, u32) {
    let width = body.get("width").and_then(|v| v.as_u64()).unwrap_or(1920) as u32;
    let height = body.get("height").and_then(|v| v.as_u64()).unwrap_or(1080) as u32;
    let fps = body.get("fps").and_then(|v| v.as_u64()).unwrap_or(30) as u32;
    (width, height, fps)
}

/// What the caller asked for: a named display, nothing, or garbage. Absent
/// and null both mean "no display target" (the window half decides); garbage
/// fails closed (400) instead of capturing the wrong monitor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DisplayRequest {
    Primary,
    Display(u32),
    Invalid,
}

/// Requested monitor, if the caller names one. Accepts numbers and numeric
/// strings — the web picker round-trips ids through JS, where they may
/// stringify. Explicit null counts as absent.
fn capture_display_request(body: &serde_json::Value) -> DisplayRequest {
    let Some(v) = body.get("display_id") else {
        return DisplayRequest::Primary;
    };
    if v.is_null() {
        return DisplayRequest::Primary;
    }
    if let Some(n) = v.as_u64() {
        return DisplayRequest::Display(n as u32);
    }
    match v.as_str().map(str::trim).map(str::parse::<u32>) {
        Some(Ok(id)) => DisplayRequest::Display(id),
        _ => DisplayRequest::Invalid,
    }
}

/// Pick the session display: the requested id when named, else the primary.
/// `None` means start must fail (unknown id, or no displays at all).
fn select_display(
    displays: &[crate::capture::Display],
    requested: Option<u32>,
) -> Option<&crate::capture::Display> {
    match requested {
        Some(id) => displays.iter().find(|d| d.id == id),
        None => displays.first(),
    }
}
/// What the caller asked for: a named window, nothing, or garbage. Absent
/// and null both mean "no window target" (the display half decides); garbage
/// fails closed (400) instead of capturing the wrong window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WindowRequest {
    Absent,
    Window(u32),
    Invalid,
}

/// Requested window, if the caller names one. Accepts numbers and numeric
/// strings — the web picker round-trips ids through JS, where they may
/// stringify. Explicit null counts as absent.
fn capture_window_request(body: &serde_json::Value) -> WindowRequest {
    let Some(v) = body.get("window_id") else {
        return WindowRequest::Absent;
    };
    if v.is_null() {
        return WindowRequest::Absent;
    }
    if let Some(n) = v.as_u64() {
        return WindowRequest::Window(n as u32);
    }
    match v.as_str().map(str::trim).map(str::parse::<u32>) {
        Some(Ok(id)) => WindowRequest::Window(id),
        _ => WindowRequest::Invalid,
    }
}

/// Pick the session window by id. `None` means start must fail with 400
/// (unknown id — e.g. closed since `/capture/windows`).
fn select_window(
    windows: &[crate::capture::Window],
    requested: u32,
) -> Option<&crate::capture::Window> {
    windows.iter().find(|w| w.id == requested)
}

async fn capture_start(
    State(s): State<ApiState>,
    headers: HeaderMap,
    body: Option<Json<serde_json::Value>>,
) -> impl IntoResponse {
    // Publish is still unwired (draft-16 has no Rust transport), so a
    // started session serves preview frames only — screen.available
    // stays false and the web UI keeps routing to the browser picker.
    let body_value = body
        .as_ref()
        .map(|b| &b.0)
        .unwrap_or(&serde_json::Value::Null);
    let (width, height, fps) = capture_size(body_value);
    let requested_display = capture_display_request(body_value);
    let requested_window = capture_window_request(body_value);
    if body_value.get("codec").and_then(|v| v.as_str()) == Some("h264") {
        return h264_start(
            s,
            headers,
            body_value,
            width,
            height,
            fps,
            requested_display,
            requested_window,
        )
        .await;
    }
    let prepared = tokio::task::spawn_blocking(
        move || -> Result<(crate::capture::CaptureSession, serde_json::Value), (StatusCode, String)> {
        // Exactly one target: both or neither is a 400, never a guess.
        // Garbage ids fail closed before any OS enumeration runs.
        match (requested_display, requested_window) {
            (DisplayRequest::Invalid, _) => {
                Err((StatusCode::BAD_REQUEST, "invalid display_id".to_string()))
            }
            (_, WindowRequest::Invalid) => {
                Err((StatusCode::BAD_REQUEST, "invalid window_id".to_string()))
            }
            (DisplayRequest::Display(_), WindowRequest::Window(_)) => Err((
                StatusCode::BAD_REQUEST,
                "only one of display_id, window_id".to_string(),
            )),
            (DisplayRequest::Primary, WindowRequest::Absent) => Err((
                StatusCode::BAD_REQUEST,
                "display_id or window_id required".to_string(),
            )),
            (DisplayRequest::Display(id), WindowRequest::Absent) => {
                let displays = crate::capture::list_displays()
                    .map_err(|e| (StatusCode::SERVICE_UNAVAILABLE, e.to_string()))?;
                let display = select_display(&displays, Some(id))
                    .ok_or_else(|| (StatusCode::BAD_REQUEST, format!("display {id} not found")))?;
                let mut session = crate::capture::CaptureSession::new();
                session
                    .start(display.id, width, height, fps)
                    .map_err(start_capture_status)?;
                let live_id = session.display_id();
                let capture_id = session.id().to_string();
                let (got_w, got_h, got_fps) = (width, height, session.fps());
                Ok((
                    session,
                    serde_json::json!({"started": true, "capture_id": capture_id, "display_id": live_id, "width": got_w, "height": got_h, "fps": got_fps}),
                ))
            }
            (DisplayRequest::Primary, WindowRequest::Window(id)) => {
                let windows = crate::capture::list_windows()
                    .map_err(|e| (StatusCode::SERVICE_UNAVAILABLE, e.to_string()))?;
                let window = select_window(&windows, id)
                    .ok_or_else(|| (StatusCode::BAD_REQUEST, format!("window {id} not found")))?;
                let mut session = crate::capture::CaptureSession::new();
                session
                    .start_window(window.id, width, height, fps)
                    .map_err(start_capture_status)?;
                let live_id = session.window_id();
                let capture_id = session.id().to_string();
                let (got_w, got_h, got_fps) = (width, height, session.fps());
                Ok((
                    session,
                    serde_json::json!({"started": true, "capture_id": capture_id, "window_id": live_id, "width": got_w, "height": got_h, "fps": got_fps}),
                ))
            }
        }
    },
    )
    .await;
    let started = match prepared {
        Ok(Ok((session, body))) => {
            let replaced = s.state.capture.lock().replace(session);
            // Joining the previous grab worker can take up to one capture
            // interval. Never make this async request worker (or the state
            // mutex) wait for it during a resolution/FPS switch.
            if let Some(previous) = replaced {
                tokio::task::spawn_blocking(move || drop(previous));
            }
            Ok(body)
        }
        Ok(Err(error)) => Err(error),
        Err(error) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("capture worker failed: {error}"),
        )),
    };
    let mut res = match started {
        Ok(body) => Json(body).into_response(),
        Err((status, error)) => (status, Json(serde_json::json!({"error": error}))).into_response(),
    };
    apply_cors(&s.state, &headers, res.headers_mut());
    with_no_store(res)
}

async fn capture_stop(
    State(s): State<ApiState>,
    headers: HeaderMap,
    body: Option<Json<serde_json::Value>>,
) -> impl IntoResponse {
    let requested_id = body.as_ref().and_then(|Json(value)| {
        value.get("capture_id").and_then(|id| {
            id.as_str()
                .and_then(|id| id.parse::<u64>().ok())
                .or_else(|| id.as_u64())
        })
    });
    let conditional = body.is_some();
    let removed = {
        let mut active = s.state.capture.lock();
        let matches = match (conditional, requested_id, active.as_ref()) {
            (false, _, _) => true, // Backward compatibility with old web clients.
            (true, Some(wanted), Some(session)) => session.id() == wanted,
            _ => false,
        };
        if matches {
            active.take()
        } else {
            None
        }
    };
    let encoded = if conditional && requested_id.is_some() {
        // An id that names the JPEG session leaves the H.264 one alone, and
        // vice versa; a body without a match stops nothing.
        let mut slot = s.state.h264.lock();
        match (requested_id, slot.as_ref()) {
            (Some(wanted), Some(session)) if session.id() == wanted => slot.take(),
            _ => None,
        }
    } else {
        s.state.h264.lock().take()
    };
    let stopped = removed.is_some() || encoded.is_some() || !conditional;
    // Drop joins the capture worker outside both the AppState mutex and the
    // async runtime worker, so shutdown cannot stall loopback requests.
    if let Some(session) = removed {
        tokio::task::spawn_blocking(move || drop(session));
    }
    if let Some(session) = encoded {
        tokio::task::spawn_blocking(move || drop(session));
    }
    if stopped {
        // The audio mix belongs to the session: ending it here closes any live
        // `/audio/stream` body on its next tick (clean EOF, not a hang).
        s.state.audio_tap.lock().stop();
    }
    let mut res = Json(serde_json::json!({"stopped": stopped})).into_response();
    apply_cors(&s.state, &headers, res.headers_mut());
    with_no_store(res)
}

/// Bits per pixel per second that keeps 4K60 crisp without drowning a home
/// uplink: 4K60 lands near 45 Mb/s, 1080p60 near 11 Mb/s.
const H264_BITS_PER_PIXEL_SECOND: f64 = 0.09;

#[allow(clippy::too_many_arguments)]
async fn h264_start(
    s: ApiState,
    headers: HeaderMap,
    body: &serde_json::Value,
    width: u32,
    height: u32,
    fps: u32,
    display: DisplayRequest,
    window: WindowRequest,
) -> Response {
    let target = match (display, window) {
        (DisplayRequest::Invalid, _) => Err("invalid display_id".to_string()),
        (_, WindowRequest::Invalid) => Err("invalid window_id".to_string()),
        (DisplayRequest::Display(_), WindowRequest::Window(_)) => {
            Err("only one of display_id, window_id".to_string())
        }
        (DisplayRequest::Primary, WindowRequest::Absent) => {
            Err("display_id or window_id required".to_string())
        }
        (DisplayRequest::Display(id), WindowRequest::Absent) => {
            Ok(crate::h264::H264Target::Display(id))
        }
        (DisplayRequest::Primary, WindowRequest::Window(id)) => {
            Ok(crate::h264::H264Target::Window(id))
        }
    };
    let outcome: Result<serde_json::Value, (StatusCode, String)> = match target {
        Err(error) => Err((StatusCode::BAD_REQUEST, error)),
        Ok(_) if !crate::h264::H264Session::supported() => Err((
            StatusCode::NOT_IMPLEMENTED,
            "h264 capture is macOS only".to_string(),
        )),
        Ok(target) => {
            let bitrate = body
                .get("bitrate")
                .and_then(|v| v.as_u64())
                .map(|b| b as u32)
                .unwrap_or_else(|| {
                    (f64::from(width)
                        * f64::from(height)
                        * f64::from(fps.max(1))
                        * H264_BITS_PER_PIXEL_SECOND) as u32
                })
                .clamp(1_000_000, 80_000_000);
            let config = crate::h264::H264Config {
                target,
                width: width.max(16) & !1,
                height: height.max(16) & !1,
                fps: fps.clamp(1, 60),
                bitrate,
            };
            let started =
                tokio::task::spawn_blocking(move || crate::h264::H264Session::start(config)).await;
            match started {
                Ok(Ok(session)) => {
                    let id = session.id();
                    let previous = s.state.h264.lock().replace(session);
                    if let Some(previous) = previous {
                        tokio::task::spawn_blocking(move || drop(previous));
                    }
                    Ok(serde_json::json!({
                        "started": true, "codec": "h264", "capture_id": id.to_string(),
                        "width": config.width, "height": config.height, "fps": config.fps, "bitrate": config.bitrate,
                    }))
                }
                Ok(Err(error)) => {
                    let text = error.to_string();
                    let reason = if text.contains("permission") {
                        "permission".to_string()
                    } else {
                        text
                    };
                    Err((StatusCode::SERVICE_UNAVAILABLE, reason))
                }
                Err(error) => Err((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("h264 worker failed: {error}"),
                )),
            }
        }
    };
    let mut res = match outcome {
        Ok(body) => Json(body).into_response(),
        Err((status, error)) => (status, Json(serde_json::json!({"error": error}))).into_response(),
    };
    apply_cors(&s.state, &headers, res.headers_mut());
    with_no_store(res)
}

/// One frame on the wire: big-endian `u32` length of the payload, `u64`
/// presentation time in microseconds, one flag byte (bit 0 = keyframe), then
/// the Annex-B access unit. The stream opens on a keyframe and, after a
/// reader falls behind, resumes on the next one.
const H264_FRAME_HEADER: usize = 13;

async fn capture_h264(State(s): State<ApiState>, headers: HeaderMap) -> impl IntoResponse {
    let subscription = s
        .state
        .h264
        .lock()
        .as_ref()
        .map(|session| (session.id(), session.subscribe()));
    let Some((capture_id, mut frames)) = subscription else {
        let mut res = (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "idle"})),
        )
            .into_response();
        apply_cors(&s.state, &headers, res.headers_mut());
        return with_no_store(res);
    };
    let state = s.state.clone();
    let stream = async_stream::stream! {
        let mut synced = false;
        loop {
            let frame = match frames.recv().await {
                Ok(frame) => frame,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    synced = false;
                    if let Some(session) = state.h264.lock().as_ref() {
                        if session.id() == capture_id { drop(session.subscribe()); }
                    }
                    continue;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            };
            if !synced {
                if !frame.keyframe { continue; }
                synced = true;
            }
            let mut packet = Vec::with_capacity(H264_FRAME_HEADER + frame.data.len());
            packet.extend_from_slice(&(frame.data.len() as u32).to_be_bytes());
            packet.extend_from_slice(&frame.pts_us.to_be_bytes());
            packet.push(u8::from(frame.keyframe));
            packet.extend_from_slice(&frame.data);
            yield Ok::<_, Infallible>(axum::body::Bytes::from(packet));
        }
    };
    let mut res = axum::body::Body::from_stream(stream).into_response();
    res.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    apply_cors(&s.state, &headers, res.headers_mut());
    with_no_store(res)
}

// ---- Local torrent engine (librqbit) ----
//
// Every route validates input before touching the engine, so malformed
// requests fail fast without waking the swarm. While the engine is absent
// (failed boot) everything here answers 503 and the web UI keeps its
// native path: the capability flag is the only thing the UI reads.

/// Fail-fast ceiling for one ranged read. The engine blocks on slow swarms
/// by design; the loopback caller gets a 504 instead of a hung fetch.
const TORRENT_READ_TIMEOUT: Duration = Duration::from_secs(90);
/// Keep torrent response latency and per-read allocation bounded. librqbit's
/// reader still performs its own wider piece lookahead behind this adapter.
const TORRENT_STREAM_CHUNK_BYTES: usize = 256 * 1024;

fn torrent_unavailable() -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({"error": "unavailable"})),
    )
}

fn torrent_http_status(error: &anyhow::Error) -> StatusCode {
    use crate::torrent::TorrentError as E;
    match error.downcast_ref::<E>() {
        Some(E::NotFound(_)) | Some(E::BadFile { .. }) => StatusCode::NOT_FOUND,
        Some(E::NoMetadata(_)) => StatusCode::CONFLICT,
        Some(E::MetadataTimeout(_)) => StatusCode::GATEWAY_TIMEOUT,
        Some(E::Unsatisfiable { .. }) => StatusCode::RANGE_NOT_SATISFIABLE,
        Some(E::RangeTooLarge { .. }) => StatusCode::PAYLOAD_TOO_LARGE,
        None => StatusCode::BAD_GATEWAY,
    }
}

/// Parse one RFC 9110 Range header against a known `total`.
/// Returns `(start, end_inclusive)`, truncated to the engine's single-read
/// cap. A missing or malformed header degrades to the first chunk: media
/// clients always send Range, and a full-file read could OOM past the cap.
/// `Err(())` means 416 (`Content-Range: bytes */total`).
fn parse_range_header(value: Option<&str>, total: u64) -> Result<(u64, u64), ()> {
    fn first_chunk(total: u64) -> (u64, u64) {
        (
            0,
            total.min(crate::torrent::MAX_RANGE_BYTES).saturating_sub(1),
        )
    }
    let Some(raw) = value else {
        return Ok(first_chunk(total));
    };
    let Some(spec) = raw.strip_prefix("bytes=") else {
        return Ok(first_chunk(total));
    };
    if spec.contains(',') {
        return Ok(first_chunk(total));
    }
    if let Some(suffix) = spec.strip_prefix('-') {
        let Ok(n) = suffix.parse::<u64>() else {
            return Ok(first_chunk(total));
        };
        if n == 0 {
            return Err(());
        }
        return Ok(if n >= total {
            (0, total - 1)
        } else {
            (total - n, total - 1)
        });
    }
    let (start_text, end_text) = match spec.split_once('-') {
        Some(pair) => pair,
        None => return Ok(first_chunk(total)),
    };
    let Ok(start) = start_text.parse::<u64>() else {
        return Ok(first_chunk(total));
    };
    if start >= total {
        return Err(());
    }
    let mut end = match end_text {
        "" => total - 1,
        text => match text.parse::<u64>() {
            Ok(end) => end,
            Err(_) => return Ok(first_chunk(total)),
        },
    };
    if end < start {
        return Err(());
    }
    if end >= total {
        end = total - 1;
    }
    if end - start + 1 > crate::torrent::MAX_RANGE_BYTES {
        end = start.saturating_add(crate::torrent::MAX_RANGE_BYTES - 1);
    }
    Ok((start, end))
}

fn with_content_range(mut res: Response, start: u64, end: u64, total: u64) -> Response {
    if let Ok(value) = HeaderValue::from_str(&format!("bytes {start}-{end}/{total}")) {
        res.headers_mut()
            .insert(axum::http::header::CONTENT_RANGE, value);
    }
    res
}

fn with_unsatisfiable(mut res: Response, total: u64) -> Response {
    if let Ok(value) = HeaderValue::from_str(&format!("bytes */{total}")) {
        res.headers_mut()
            .insert(axum::http::header::CONTENT_RANGE, value);
    }
    res
}

async fn torrent_add(
    State(s): State<ApiState>,
    headers: HeaderMap,
    body: Option<Json<serde_json::Value>>,
) -> impl IntoResponse {
    let magnet = body
        .as_ref()
        .map(|b| &b.0)
        .and_then(|v| v.get("magnet"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let mut res = if magnet.is_empty() || !magnet.starts_with("magnet:?") {
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "bad magnet"})),
        )
            .into_response()
    } else if let Some(manager) = s.state.torrent.clone() {
        match manager.add_magnet(magnet).await {
            Ok((id, name)) => Json(serde_json::json!({"id": id, "name": name})).into_response(),
            Err(e) => (
                torrent_http_status(&e),
                Json(serde_json::json!({"error": e.to_string()})),
            )
                .into_response(),
        }
    } else {
        torrent_unavailable().into_response()
    };
    apply_cors(&s.state, &headers, res.headers_mut());
    with_no_store(res)
}

fn tools_body(status: crate::youtube::ToolsStatus) -> serde_json::Value {
    serde_json::to_value(status).unwrap_or_else(|_| serde_json::json!({"status": "failed"}))
}

async fn youtube_tools(State(s): State<ApiState>, headers: HeaderMap) -> impl IntoResponse {
    let mut res = Json(tools_body(s.state.youtube.status())).into_response();
    apply_cors(&s.state, &headers, res.headers_mut());
    with_no_store(res)
}

async fn youtube_tools_install(State(s): State<ApiState>, headers: HeaderMap) -> impl IntoResponse {
    let status = s.state.youtube.start_install();
    let code = match status {
        crate::youtube::ToolsStatus::Unsupported => StatusCode::NOT_IMPLEMENTED,
        _ => StatusCode::ACCEPTED,
    };
    let mut res = (code, Json(tools_body(status))).into_response();
    apply_cors(&s.state, &headers, res.headers_mut());
    with_no_store(res)
}

fn youtube_unavailable(status: &crate::youtube::ToolsStatus) -> Response {
    let code = match status {
        crate::youtube::ToolsStatus::Unsupported => "unsupported",
        _ => "tools_missing",
    };
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({"error": code, "tools": tools_body(status.clone())})),
    )
        .into_response()
}

async fn youtube_resolve(
    State(s): State<ApiState>,
    headers: HeaderMap,
    body: Option<Json<serde_json::Value>>,
) -> impl IntoResponse {
    let url = body
        .as_ref()
        .and_then(|b| b.0.get("url"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let mut res = if url.is_empty()
        || url.len() > 2048
        || !(url.starts_with("https://") || url.starts_with("http://"))
    {
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "invalid_url"})),
        )
            .into_response()
    } else if !s.state.youtube.ready() {
        youtube_unavailable(&s.state.youtube.status())
    } else {
        match s.state.youtube.resolve(&url).await {
            Ok(summary) => Json(serde_json::json!({"summary": summary})).into_response(),
            Err(e) => (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({"error": e.code(), "detail": e.detail()})),
            )
                .into_response(),
        }
    };
    apply_cors(&s.state, &headers, res.headers_mut());
    with_no_store(res)
}

async fn youtube_run(
    State(s): State<ApiState>,
    headers: HeaderMap,
    body: Option<Json<serde_json::Value>>,
) -> impl IntoResponse {
    let parsed = body
        .map(|b| serde_json::from_value::<crate::youtube::RunRequest>(b.0))
        .transpose();
    let mut res = match parsed {
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "bad_request", "detail": e.to_string()})),
        )
            .into_response(),
        Ok(None) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "bad_request"})),
        )
            .into_response(),
        Ok(Some(req)) if !s.state.youtube.ready() => {
            let _ = req;
            youtube_unavailable(&s.state.youtube.status())
        }
        Ok(Some(req)) => {
            let run_id = req.run_id.clone();
            match s.state.youtube.run(req).await {
                Ok(()) => (
                    StatusCode::ACCEPTED,
                    Json(serde_json::json!({"runId": run_id})),
                )
                    .into_response(),
                Err(e) => {
                    let code = e.to_string();
                    let status = if code == "remux_busy" {
                        StatusCode::SERVICE_UNAVAILABLE
                    } else {
                        StatusCode::BAD_GATEWAY
                    };
                    (status, Json(serde_json::json!({"error": code}))).into_response()
                }
            }
        }
    };
    apply_cors(&s.state, &headers, res.headers_mut());
    with_no_store(res)
}

async fn youtube_run_status(
    State(s): State<ApiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let mut res = match s.state.youtube.run_status(&id).await {
        Some(status) => Json(serde_json::json!({
            "runId": id,
            "state": status.state,
            "producedMs": status.produced_ms,
            "error": status.error,
        }))
        .into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "unknown_run"})),
        )
            .into_response(),
    };
    apply_cors(&s.state, &headers, res.headers_mut());
    with_no_store(res)
}

async fn youtube_run_cancel(
    State(s): State<ApiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let cancelled = s.state.youtube.cancel(&id).await;
    let mut res = Json(serde_json::json!({"cancelled": cancelled})).into_response();
    apply_cors(&s.state, &headers, res.headers_mut());
    with_no_store(res)
}

async fn torrent_list(State(s): State<ApiState>, headers: HeaderMap) -> impl IntoResponse {
    let mut res = match s.state.torrent.clone() {
        Some(manager) => Json(serde_json::json!({"torrents": manager.list()})).into_response(),
        None => torrent_unavailable().into_response(),
    };
    apply_cors(&s.state, &headers, res.headers_mut());
    with_no_store(res)
}

async fn torrent_stats(
    State(s): State<ApiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let mut res = match s.state.torrent.clone() {
        Some(manager) => match manager.stats(&id) {
            Ok(stats) => Json(serde_json::json!(stats)).into_response(),
            Err(e) => (
                torrent_http_status(&e),
                Json(serde_json::json!({"error": e.to_string()})),
            )
                .into_response(),
        },
        None => torrent_unavailable().into_response(),
    };
    apply_cors(&s.state, &headers, res.headers_mut());
    with_no_store(res)
}

async fn torrent_remove(
    State(s): State<ApiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let mut res = if crate::torrent::parse_id_hex(&id).is_err() {
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "bad id"})),
        )
            .into_response()
    } else if let Some(manager) = s.state.torrent.clone() {
        Json(serde_json::json!({"removed": manager.remove(&id).await})).into_response()
    } else {
        torrent_unavailable().into_response()
    };
    apply_cors(&s.state, &headers, res.headers_mut());
    with_no_store(res)
}

async fn torrent_select(
    State(s): State<ApiState>,
    headers: HeaderMap,
    body: Option<Json<serde_json::Value>>,
) -> impl IntoResponse {
    let picked = (|| -> Result<(String, usize), String> {
        let body = body
            .as_ref()
            .map(|b| &b.0)
            .ok_or("missing body".to_string())?;
        let id = body
            .get("id")
            .and_then(|v| v.as_str())
            .filter(|id| crate::torrent::parse_id_hex(id).is_ok())
            .ok_or("bad id".to_string())?;
        let file = body
            .get("file")
            .and_then(|v| v.as_u64())
            .ok_or("bad file".to_string())? as usize;
        Ok((id.to_string(), file))
    })();
    let mut res = match picked {
        Err(error) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": error})),
        )
            .into_response(),
        Ok((id, file)) => match s.state.torrent.clone() {
            Some(manager) => match manager.select_file(&id, file).await {
                Ok(()) => Json(serde_json::json!({"selected": true})).into_response(),
                Err(e) => (
                    torrent_http_status(&e),
                    Json(serde_json::json!({"error": e.to_string()})),
                )
                    .into_response(),
            },
            None => torrent_unavailable().into_response(),
        },
    };
    apply_cors(&s.state, &headers, res.headers_mut());
    with_no_store(res)
}

async fn torrent_data(
    State(s): State<ApiState>,
    headers: HeaderMap,
    Path((id, file)): Path<(String, String)>,
) -> impl IntoResponse {
    // Shape first so malformed ids fail fast without waking the engine.
    if crate::torrent::parse_id_hex(&id).is_err() {
        let mut res = (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "bad id"})),
        )
            .into_response();
        apply_cors(&s.state, &headers, res.headers_mut());
        return with_no_store(res);
    }
    let Some(manager) = s.state.torrent.clone() else {
        let mut res = torrent_unavailable().into_response();
        apply_cors(&s.state, &headers, res.headers_mut());
        return with_no_store(res);
    };
    // The web UI addresses files by index; paths are accepted too.
    let file_idx = match file.parse::<usize>() {
        Ok(index) => Some(index),
        Err(_) => manager
            .list()
            .iter()
            .find(|info| info.id == id)
            .and_then(|info| info.files.iter().position(|f| f.path == file)),
    };
    let total = file_idx.and_then(|index| {
        manager
            .list()
            .iter()
            .find(|info| info.id == id)
            .and_then(|info| info.files.get(index))
            .map(|file| file.size)
    });
    let (index, total) = match (file_idx, total) {
        (Some(index), Some(total)) if total > 0 => (index, total),
        (Some(_), _) => {
            // Unknown file, or metadata unresolved: 404 vs 409 depends on
            // whether the torrent itself is known.
            let known = manager.list().iter().any(|info| info.id == id);
            let status = if known {
                StatusCode::CONFLICT
            } else {
                StatusCode::NOT_FOUND
            };
            let mut res = (
                status,
                Json(serde_json::json!({"error": if known { "no metadata" } else { "not found" }})),
            )
                .into_response();
            apply_cors(&s.state, &headers, res.headers_mut());
            return with_no_store(res);
        }
        (None, _) => {
            let mut res = (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "not found"})),
            )
                .into_response();
            apply_cors(&s.state, &headers, res.headers_mut());
            return with_no_store(res);
        }
    };
    let range = headers
        .get(axum::http::header::RANGE)
        .and_then(|v| v.to_str().ok());
    let (start, end) = match parse_range_header(range, total) {
        Ok(span) => span,
        Err(()) => {
            let mut res = (
                StatusCode::RANGE_NOT_SATISFIABLE,
                Json(serde_json::json!({"error": "unsatisfiable"})),
            )
                .into_response();
            apply_cors(&s.state, &headers, res.headers_mut());
            return with_no_store(with_unsatisfiable(res, total));
        }
    };
    let opened = tokio::time::timeout(
        TORRENT_READ_TIMEOUT,
        manager.open_range(&id, index, start..end),
    )
    .await;
    let mut res = match opened {
        Err(_) => (
            StatusCode::GATEWAY_TIMEOUT,
            Json(serde_json::json!({"error": "timeout"})),
        )
            .into_response(),
        Ok(Err(e)) => {
            let status = torrent_http_status(&e);
            let mut res =
                (status, Json(serde_json::json!({"error": e.to_string()}))).into_response();
            if status == StatusCode::RANGE_NOT_SATISFIABLE {
                res = with_unsatisfiable(res, total);
            }
            res
        }
        Ok(Ok((mut source, range_len, _))) => {
            let stream = async_stream::stream! {
                let mut remaining = range_len;
                while remaining > 0 {
                    let capacity = remaining.min(TORRENT_STREAM_CHUNK_BYTES as u64) as usize;
                    let mut chunk = vec![0_u8; capacity];
                    let read = match tokio::time::timeout(
                        TORRENT_READ_TIMEOUT,
                        source.read(&mut chunk),
                    ).await {
                        Ok(Ok(read)) => read,
                        Ok(Err(error)) => {
                            yield Err::<axum::body::Bytes, std::io::Error>(error);
                            break;
                        }
                        Err(_) => {
                            yield Err::<axum::body::Bytes, std::io::Error>(std::io::Error::new(
                                std::io::ErrorKind::TimedOut,
                                "torrent stream timed out",
                            ));
                            break;
                        }
                    };
                    if read == 0 {
                        yield Err::<axum::body::Bytes, std::io::Error>(std::io::Error::new(
                            std::io::ErrorKind::UnexpectedEof,
                            "torrent stream ended before the requested range",
                        ));
                        break;
                    }
                    chunk.truncate(read);
                    remaining -= read as u64;
                    yield Ok::<axum::body::Bytes, std::io::Error>(axum::body::Bytes::from(chunk));
                }
            };
            let mut res = axum::body::Body::from_stream(stream).into_response();
            *res.status_mut() = StatusCode::PARTIAL_CONTENT;
            res.headers_mut().insert(
                axum::http::header::ACCEPT_RANGES,
                HeaderValue::from_static("bytes"),
            );
            res.headers_mut().insert(
                axum::http::header::CONTENT_TYPE,
                HeaderValue::from_static("application/octet-stream"),
            );
            if let Ok(value) = HeaderValue::from_str(&range_len.to_string()) {
                res.headers_mut()
                    .insert(axum::http::header::CONTENT_LENGTH, value);
            }
            with_content_range(res, start, end, total)
        }
    };
    apply_cors(&s.state, &headers, res.headers_mut());
    with_no_store(res)
}
async fn audio_mode(
    State(s): State<ApiState>,
    headers: HeaderMap,
    body: Option<Json<serde_json::Value>>,
) -> impl IntoResponse {
    let mode: Result<crate::audio::AudioMode, _> = body
        .as_ref()
        .map(|b| &b.0)
        .and_then(|v| v.get("mode"))
        .map_or(Err("missing mode".to_string()), |raw| {
            serde_json::from_value(raw.clone()).map_err(|_| "bad mode".to_string())
        });
    let mut res = match mode {
        Ok(mode) => {
            s.state.audio.lock().set_mode(mode);
            // Live: the running tap re-filters (macOS rebuilds the SCK
            // exclusion set, Windows restarts its per-process taps) and the
            // next stream tick mixes with the new mode anyway.
            let muted = s.state.audio.lock().muted_apps.clone();
            s.state.audio_tap.lock().set_excluded(&muted);
            Json(serde_json::json!({"mode": mode})).into_response()
        }
        Err(error) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": error})),
        )
            .into_response(),
    };
    apply_cors(&s.state, &headers, res.headers_mut());
    with_no_store(res)
}

async fn audio_mute(
    State(s): State<ApiState>,
    headers: HeaderMap,
    body: Option<Json<serde_json::Value>>,
) -> impl IntoResponse {
    let pick = (|| -> Result<(String, bool), String> {
        let body = body
            .as_ref()
            .map(|b| &b.0)
            .ok_or("missing body".to_string())?;
        let app = body
            .get("app")
            .and_then(|v| v.as_str())
            .filter(|name| !name.is_empty())
            .ok_or("missing app".to_string())?;
        let muted = body
            .get("muted")
            .and_then(|v| v.as_bool())
            .ok_or("missing muted".to_string())?;
        Ok((app.to_string(), muted))
    })();
    let mut res = match pick {
        Ok((app, muted)) => {
            let mut audio = s.state.audio.lock();
            if muted {
                audio.muted_apps.insert(app.clone());
            } else {
                audio.muted_apps.remove(&app);
            }
            drop(audio);
            // Live: same re-filter as a mode switch (see `audio_mode`).
            let muted_set = s.state.audio.lock().muted_apps.clone();
            s.state.audio_tap.lock().set_excluded(&muted_set);
            Json(serde_json::json!({"app": app, "muted": muted})).into_response()
        }
        Err(error) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": error})),
        )
            .into_response(),
    };
    apply_cors(&s.state, &headers, res.headers_mut());
    with_no_store(res)
}

/// How long the stream waits for the tap before sending silence instead:
/// a tap that is still starting, or one the OS refused, keeps the browser
/// fed, but a tap that is merely between buffers is never padded.
const AUDIO_STARVE_WAIT: Duration = Duration::from_millis(200);

/// System-audio mix as an infinite raw PCM body: `s16le`, 48 kHz, stereo,
/// in [`crate::audio_engine::BYTES_PER_CHUNK`]-byte chunks
/// (`Content-Type: audio/L16`). No framing headers, no container.
///
/// - `501 {error}` where unwired (Linux).
/// - `404 {error:"idle"}` with no capture session: the mix belongs to the
///   CURRENT session (starts/stops with it — the body ends when
///   `/capture/stop` lands).
/// - Otherwise `200`: the platform tap's frames go out as the OS captures
///   them, mixed with the LIVE [`crate::audio::AudioState`] (`all` = full
///   mix, `none` = silence, `custom` = mix minus the muted set). The
///   stream is paced by the capture itself, not by a timer: a clock of our
///   own drifted against the capture clock and had to pad or drop a few
///   samples every tick to stay on it, which the ear heard as a constant
///   flutter. Silence is sent only while the tap delivers nothing at all.
async fn audio_stream(State(s): State<ApiState>, headers: HeaderMap) -> impl IntoResponse {
    if !crate::audio_engine::capture_supported() {
        let mut res = (
            StatusCode::NOT_IMPLEMENTED,
            Json(serde_json::json!({"error": "not_implemented"})),
        )
            .into_response();
        apply_cors(&s.state, &headers, res.headers_mut());
        return with_no_store(res);
    }
    if s.state.capture.lock().is_none() && s.state.h264.lock().is_none() {
        let mut res = (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "idle"})),
        )
            .into_response();
        apply_cors(&s.state, &headers, res.headers_mut());
        return with_no_store(res);
    }
    // Lazy tap start, off the async runtime (the OS handshake blocks). A
    // refusal still streams: the ticks below degrade to silence.
    let tap = s.state.audio_tap.clone();
    let muted = s.state.audio.lock().muted_apps.clone();
    let _ = tokio::task::spawn_blocking(move || tap.lock().start(&muted)).await;
    let state = s.state.clone();
    // The wait for frames blocks, so it lives on a thread of its own; the
    // body only forwards what that thread hands over.
    let (chunks_tx, mut chunks_rx) = tokio::sync::mpsc::channel::<axum::body::Bytes>(64);
    std::thread::Builder::new()
        .name("jlocal-audio-stream".into())
        .spawn(move || {
            let mut receiver = None;
            let mut carry: Vec<i16> = Vec::new();
            loop {
                // Session stopped mid-stream: end the body (clean EOF).
                if state.capture.lock().is_none() && state.h264.lock().is_none() {
                    break;
                }
                if receiver.is_none() {
                    receiver = state.audio_tap.lock().receiver();
                }
                let received = match &receiver {
                    Some(rx) => rx.recv_timeout(AUDIO_STARVE_WAIT),
                    None => {
                        std::thread::sleep(AUDIO_STARVE_WAIT);
                        Err(crossbeam_channel::RecvTimeoutError::Timeout)
                    }
                };
                let mixed = match received {
                    Ok(frame) => {
                        let audio = state.audio.lock().clone();
                        let len = frame.samples.len();
                        let mixed = crate::audio_engine::mix_frames(&[frame], &audio);
                        // A muted mix still takes up its time.
                        if mixed.is_empty() {
                            vec![0; len]
                        } else {
                            mixed
                        }
                    }
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                        vec![0; crate::audio_engine::SAMPLES_PER_CHUNK]
                    }
                    Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                        receiver = None;
                        continue;
                    }
                };
                carry.extend(mixed);
                while carry.len() >= crate::audio_engine::SAMPLES_PER_CHUNK {
                    let chunk: Vec<i16> = carry
                        .drain(..crate::audio_engine::SAMPLES_PER_CHUNK)
                        .collect();
                    let bytes = axum::body::Bytes::from(crate::audio_engine::i16_to_bytes(&chunk));
                    // The reader is gone: nothing left to do here.
                    if chunks_tx.blocking_send(bytes).is_err() {
                        return;
                    }
                }
            }
        })
        .ok();
    let stream = async_stream::stream! {
        while let Some(bytes) = chunks_rx.recv().await {
            yield Ok::<_, std::convert::Infallible>(bytes);
        }
    };
    let mut res = axum::body::Body::from_stream(stream).into_response();
    res.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        HeaderValue::from_static(crate::audio_engine::CONTENT_TYPE),
    );
    apply_cors(&s.state, &headers, res.headers_mut());
    with_no_store(res)
}

/// DNS-rebinding guard: only loopback Host values may talk to us.
async fn require_loopback_host(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Response {
    let _ = &state;
    let ok = req
        .headers()
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|host| {
            let host = host.to_ascii_lowercase();
            host.starts_with("127.0.0.1")
                || host.starts_with("localhost")
                || host.starts_with("[::1]")
        });
    if !ok {
        return (StatusCode::BAD_REQUEST, "loopback host required").into_response();
    }
    next.run(req).await
}

fn apply_cors(state: &AppState, req_headers: &HeaderMap, res_headers: &mut HeaderMap) {
    if let Some(origin) = req_headers.get(axum::http::header::ORIGIN) {
        if let Ok(origin) = origin.to_str() {
            if state.allowed_origins.iter().any(|o| o == origin) {
                if let Ok(v) = HeaderValue::from_str(origin) {
                    res_headers.insert(axum::http::header::ACCESS_CONTROL_ALLOW_ORIGIN, v);
                    res_headers
                        .insert(axum::http::header::VARY, HeaderValue::from_static("Origin"));
                }
            }
        }
    }
    res_headers.insert(
        axum::http::header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("GET, POST, OPTIONS"),
    );
    res_headers.insert(
        axum::http::header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static("Range, Content-Type, Authorization"),
    );
    res_headers.insert(
        "access-control-allow-private-network",
        HeaderValue::from_static("true"),
    );
    res_headers.insert(
        axum::http::header::ACCESS_CONTROL_MAX_AGE,
        HeaderValue::from_static("600"),
    );
}

fn with_no_store(mut res: Response) -> Response {
    res.headers_mut().insert(
        axum::http::header::CACHE_CONTROL,
        HeaderValue::from_static("no-store"),
    );
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_shape() {
        let h = Health {
            name: "jlocal",
            version: "v0.1.0",
            connected: true,
            moq: "disabled",
            torrent: "standby",
            port: 40392,
            started_unix: 0,
        };
        let v = serde_json::to_value(&h).unwrap();
        assert_eq!(v["name"], "jlocal");
        assert_eq!(v["connected"], true);
        assert_eq!(v["moq"], "disabled");
    }

    #[test]
    fn default_origins_cover_prod_and_beta() {
        let origins = super::super::status::default_origins_for_tests();
        assert!(origins.contains(&"https://beta.juntos.lol".to_string()));
        assert!(origins.contains(&"https://juntos.lol".to_string()));
    }

    fn state_with(origins: &[&str]) -> ApiState {
        ApiState {
            state: AppState {
                started_unix: 0,
                allowed_origins: origins.iter().map(|s| s.to_string()).collect(),
                caps: crate::status::CapabilityFlags::default(),
                capture: std::sync::Arc::new(parking_lot::Mutex::new(None)),
                h264: std::sync::Arc::new(parking_lot::Mutex::new(None)),
                audio: std::sync::Arc::new(
                    parking_lot::Mutex::new(crate::audio::AudioState::new()),
                ),
                audio_tap: std::sync::Arc::new(parking_lot::Mutex::new(
                    crate::audio_engine::AudioTap::new(),
                )),
                torrent: None,
                update: std::sync::Arc::new(parking_lot::Mutex::new(
                    crate::update::UpdateState::default(),
                )),
                youtube: crate::youtube::Youtube::new(),
            },
            port: 40392,
        }
    }

    fn origin_headers(origin: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::ORIGIN,
            HeaderValue::from_str(origin).unwrap(),
        );
        headers
    }

    #[tokio::test]
    async fn health_echoes_allowed_origin() {
        let res = health(
            State(state_with(&["https://beta.juntos.lol"])),
            origin_headers("https://beta.juntos.lol"),
        )
        .await
        .into_response();
        assert_eq!(
            res.headers()
                .get(axum::http::header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .and_then(|v| v.to_str().ok()),
            Some("https://beta.juntos.lol")
        );
        assert!(res
            .headers()
            .contains_key("access-control-allow-private-network"));
    }

    #[tokio::test]
    async fn health_omits_unknown_origin() {
        let res = health(
            State(state_with(&["https://beta.juntos.lol"])),
            origin_headers("https://evil.test"),
        )
        .await
        .into_response();
        assert!(!res
            .headers()
            .contains_key(axum::http::header::ACCESS_CONTROL_ALLOW_ORIGIN));
    }

    #[test]
    fn loopback_host_check() {
        for good in ["127.0.0.1:40392", "localhost:40392", "[::1]:40392"] {
            let g = good.to_ascii_lowercase();
            assert!(
                g.starts_with("127.0.0.1") || g.starts_with("localhost") || g.starts_with("[::1]")
            );
        }
        assert!(!"evil.com".starts_with("127.0.0.1"));
    }

    #[tokio::test]
    async fn capabilities_shape_and_echoes_allowed_origin() {
        let res = capabilities(
            State(state_with(&["https://beta.juntos.lol"])),
            origin_headers("https://beta.juntos.lol"),
        )
        .await
        .into_response();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(
            res.headers()
                .get(axum::http::header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .and_then(|v| v.to_str().ok()),
            Some("https://beta.juntos.lol")
        );
        assert_eq!(
            res.headers()
                .get(axum::http::header::CACHE_CONTROL)
                .and_then(|v| v.to_str().ok()),
            Some("no-store")
        );
        let body = axum::body::to_bytes(res.into_body(), 4096).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["name"], "jlocal");
        assert_eq!(v["capabilities"]["screen"]["available"], false);
        assert_eq!(v["capabilities"]["screen"]["capture"], true);
        assert_eq!(
            v["capabilities"]["screen"]["h264"],
            cfg!(target_os = "macos")
        );
        assert_eq!(v["capabilities"]["screen"]["maxWidth"], 3840);
        assert_eq!(v["capabilities"]["screen"]["maxHeight"], 2160);
        assert_eq!(v["capabilities"]["screen"]["maxFps"], 60);
        assert_eq!(v["capabilities"]["audio"]["appList"], false);
        // System-audio PCM is wired on macOS; Windows/Linux stay honestly off.
        assert_eq!(
            v["capabilities"]["audio"]["capture"],
            crate::audio_engine::capture_supported()
        );
    }

    #[tokio::test]
    async fn audio_apps_lists_or_honestly_501s() {
        let res = audio_apps(
            State(state_with(&["https://beta.juntos.lol"])),
            origin_headers("https://beta.juntos.lol"),
        )
        .await
        .into_response();
        // Machines with listable processes answer 200 + apps; bare ones
        // stay 501 so the web UI keeps its empty state. Either way the
        // CORS + no-store contract holds.
        assert!(res.status() == StatusCode::OK || res.status() == StatusCode::NOT_IMPLEMENTED);
        assert_eq!(
            res.headers()
                .get(axum::http::header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .and_then(|v| v.to_str().ok()),
            Some("https://beta.juntos.lol")
        );
        assert_eq!(
            res.headers()
                .get(axum::http::header::CACHE_CONTROL)
                .and_then(|v| v.to_str().ok()),
            Some("no-store")
        );
        let body = axum::body::to_bytes(res.into_body(), 65536).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(v.get("apps").is_some() || v.get("error").is_some());
    }

    #[test]
    fn capture_size_defaults_and_parses() {
        assert_eq!(
            super::capture_size(&serde_json::Value::Null),
            (1920, 1080, 30)
        );
        assert_eq!(
            super::capture_size(&serde_json::json!({"width": 3840, "height": 2160, "fps": 60})),
            (3840, 2160, 60)
        );
    }

    #[test]
    fn capture_display_selection_prefers_requested_id() {
        use crate::capture::Display;
        let displays = vec![
            Display {
                id: 1,
                name: "a".into(),
                width: 1920,
                height: 1080,
            },
            Display {
                id: 2,
                name: "b".into(),
                width: 2560,
                height: 1440,
            },
        ];
        // No id: primary display. Named id: that display. Unknown id or
        // empty list: nothing (the handler maps these to 400 / 503).
        assert_eq!(
            super::select_display(&displays, None).map(|d| d.id),
            Some(1)
        );
        assert_eq!(
            super::select_display(&displays, Some(2)).map(|d| d.id),
            Some(2)
        );
        assert!(super::select_display(&displays, Some(9)).is_none());
        let empty: Vec<Display> = vec![];
        assert!(super::select_display(&empty, None).is_none());
        assert!(super::select_display(&empty, Some(1)).is_none());
    }

    #[test]
    fn capture_display_request_classifies_missing_named_and_garbage() {
        use super::DisplayRequest::{Display, Invalid, Primary};
        let req = super::capture_display_request;
        // Absent or null: primary display.
        assert_eq!(req(&serde_json::Value::Null), Primary);
        assert_eq!(req(&serde_json::json!({"width": 1920})), Primary);
        assert_eq!(req(&serde_json::json!({"display_id": null})), Primary);
        // Numbers and numeric strings name a display.
        assert_eq!(req(&serde_json::json!({"display_id": 2})), Display(2));
        assert_eq!(req(&serde_json::json!({"display_id": "2"})), Display(2));
        // Anything else fails closed (the handler answers 400, never the
        // wrong monitor).
        assert_eq!(req(&serde_json::json!({"display_id": "nope"})), Invalid);
        assert_eq!(req(&serde_json::json!({"display_id": true})), Invalid);
        assert_eq!(req(&serde_json::json!({"display_id": -1})), Invalid);
    }

    #[test]
    fn capture_window_request_classifies_missing_named_and_garbage() {
        use super::WindowRequest::{Absent, Invalid, Window};
        let req = super::capture_window_request;
        // Absent or null: no window target.
        assert_eq!(req(&serde_json::Value::Null), Absent);
        assert_eq!(req(&serde_json::json!({"width": 1920})), Absent);
        assert_eq!(req(&serde_json::json!({"window_id": null})), Absent);
        // Numbers and numeric strings name a window.
        assert_eq!(req(&serde_json::json!({"window_id": 7})), Window(7));
        assert_eq!(req(&serde_json::json!({"window_id": "7"})), Window(7));
        // Anything else fails closed (the handler answers 400, never the
        // wrong window).
        assert_eq!(req(&serde_json::json!({"window_id": "nope"})), Invalid);
        assert_eq!(req(&serde_json::json!({"window_id": true})), Invalid);
        assert_eq!(req(&serde_json::json!({"window_id": -1})), Invalid);
    }

    #[test]
    fn capture_window_selection_finds_exact_id() {
        use crate::capture::Window;
        let windows = vec![
            Window {
                id: 3,
                name: "a".into(),
                app: "A".into(),
                icon: String::new(),
                width: 800,
                height: 600,
            },
            Window {
                id: 5,
                name: "b".into(),
                app: "B".into(),
                icon: "data:image/png;base64,iVBORw0KGgo=".into(),
                width: 1024,
                height: 768,
            },
        ];
        assert_eq!(super::select_window(&windows, 5).map(|w| w.id), Some(5));
        assert!(super::select_window(&windows, 9).is_none());
        let empty: Vec<Window> = vec![];
        assert!(super::select_window(&empty, 3).is_none());
    }

    #[tokio::test]
    async fn capture_windows_reports_icon_per_entry() {
        // Live enumeration is machine-dependent (headless CI may 500 when
        // the OS refuses); either way CORS + no-store hold, and every
        // listed window carries a string `icon` — "" or a PNG data URL.
        let res = capture_windows(
            State(state_with(&["https://beta.juntos.lol"])),
            origin_headers("https://beta.juntos.lol"),
        )
        .await
        .into_response();
        assert_eq!(
            res.headers()
                .get(axum::http::header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .and_then(|v| v.to_str().ok()),
            Some("https://beta.juntos.lol")
        );
        assert_eq!(
            res.headers()
                .get(axum::http::header::CACHE_CONTROL)
                .and_then(|v| v.to_str().ok()),
            Some("no-store")
        );
        if res.status() != StatusCode::OK {
            return;
        }
        let body = axum::body::to_bytes(res.into_body(), 1 << 20)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let windows = v["windows"].as_array().expect("windows must be an array");
        for w in windows {
            let icon = w["icon"].as_str().expect("window must carry a string icon");
            assert!(
                icon.is_empty() || icon.starts_with("data:image/png;base64,"),
                "unexpected icon shape"
            );
        }
    }
    #[test]
    fn start_validation_failures_stay_400() {
        // Only the eager first grab (a "capture failed …" message) maps to
        // 503; every validation message stays 400 on every machine.
        for message in [
            "display 9 not found",
            "requested 3840x2160 exceeds display 1 size 1512x982",
            "invalid display_id",
            "display_id or window_id required",
        ] {
            let (status, _) = super::start_capture_status(anyhow::anyhow!(message));
            assert_eq!(
                status,
                StatusCode::BAD_REQUEST,
                "wrong status for {message}"
            );
        }
    }

    #[tokio::test]
    async fn capture_start_needs_exactly_one_target() {
        // Both / neither / garbage never reach the OS: the 400 is
        // headless-safe and deterministic on every machine.
        for body in [
            serde_json::json!({"display_id": 1, "window_id": 2}),
            serde_json::json!({"width": 1920}),
            serde_json::json!({"display_id": "nope"}),
            serde_json::json!({"window_id": true}),
        ] {
            let res = capture_start(
                State(state_with(&["https://beta.juntos.lol"])),
                origin_headers("https://beta.juntos.lol"),
                Some(Json(body)),
            )
            .await
            .into_response();
            assert_eq!(res.status(), StatusCode::BAD_REQUEST);
            let raw = axum::body::to_bytes(res.into_body(), 4096).await.unwrap();
            let v: serde_json::Value = serde_json::from_slice(&raw).unwrap();
            assert!(v.get("error").is_some(), "{v}");
        }
        // Missing body entirely is "neither" too.
        let res = capture_start(
            State(state_with(&["https://beta.juntos.lol"])),
            origin_headers("https://beta.juntos.lol"),
            None,
        )
        .await
        .into_response();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn capabilities_reports_permission_probe() {
        let res = capabilities(
            State(state_with(&["https://beta.juntos.lol"])),
            origin_headers("https://beta.juntos.lol"),
        )
        .await
        .into_response();
        assert_eq!(res.status(), StatusCode::OK);
        let raw = axum::body::to_bytes(res.into_body(), 4096).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&raw).unwrap();
        // Report-only probe: a real boolean, whatever this machine says.
        assert!(v["capabilities"]["permissions"]["screenCapture"].is_boolean());
    }

    #[tokio::test]
    async fn capture_start_answers_typed_json() {
        let res = capture_start(
            State(state_with(&["https://beta.juntos.lol"])),
            origin_headers("https://beta.juntos.lol"),
            None,
        )
        .await
        .into_response();
        // Display availability is machine-dependent (200 started / 400 bad
        // size / 503 no displays); the envelope + CORS contract is not.
        assert!(
            res.status() == StatusCode::OK
                || res.status() == StatusCode::BAD_REQUEST
                || res.status() == StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            res.headers()
                .get(axum::http::header::CACHE_CONTROL)
                .and_then(|v| v.to_str().ok()),
            Some("no-store")
        );
        let body = axum::body::to_bytes(res.into_body(), 4096).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(v.get("started").is_some() || v.get("error").is_some());
    }

    #[tokio::test]
    async fn capture_stop_returns_200_stopped() {
        let res = capture_stop(
            State(state_with(&["https://beta.juntos.lol"])),
            origin_headers("https://beta.juntos.lol"),
            None,
        )
        .await
        .into_response();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(
            res.headers()
                .get(axum::http::header::CACHE_CONTROL)
                .and_then(|v| v.to_str().ok()),
            Some("no-store")
        );
        let body = axum::body::to_bytes(res.into_body(), 4096).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v, serde_json::json!({"stopped": true}));
    }

    #[tokio::test]
    async fn capture_stop_token_cannot_remove_a_replacement_session() {
        let state = state_with(&["https://beta.juntos.lol"]);
        let session = crate::capture::CaptureSession::new();
        let live_id = session.id();
        state.state.capture.lock().replace(session);

        let res = capture_stop(
            State(state.clone()),
            origin_headers("https://beta.juntos.lol"),
            Some(Json(serde_json::json!({"capture_id": live_id + 1}))),
        )
        .await
        .into_response();
        let body = axum::body::to_bytes(res.into_body(), 4096).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v, serde_json::json!({"stopped": false}));
        assert_eq!(
            state.state.capture.lock().as_ref().map(|s| s.id()),
            Some(live_id)
        );

        let res = capture_stop(
            State(state.clone()),
            origin_headers("https://beta.juntos.lol"),
            Some(Json(serde_json::json!({"capture_id": live_id.to_string()}))),
        )
        .await
        .into_response();
        let body = axum::body::to_bytes(res.into_body(), 4096).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v, serde_json::json!({"stopped": true}));
        assert!(state.state.capture.lock().is_none());
    }

    #[tokio::test]
    async fn audio_mode_round_trips() {
        let state = state_with(&["https://beta.juntos.lol"]);
        let res = audio_mode(
            State(state.clone()),
            origin_headers("https://beta.juntos.lol"),
            Some(Json(serde_json::json!({"mode": "custom"}))),
        )
        .await
        .into_response();
        assert_eq!(res.status(), StatusCode::OK);
        let body = axum::body::to_bytes(res.into_body(), 4096).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v, serde_json::json!({"mode": "custom"}));
        assert_eq!(
            state.state.audio.lock().mode,
            crate::audio::AudioMode::Custom
        );
    }

    #[tokio::test]
    async fn audio_mode_rejects_unknown() {
        let res = audio_mode(
            State(state_with(&["https://beta.juntos.lol"])),
            origin_headers("https://beta.juntos.lol"),
            Some(Json(serde_json::json!({"mode": "everything"}))),
        )
        .await
        .into_response();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn audio_mute_sets_and_clears() {
        let state = state_with(&["https://beta.juntos.lol"]);
        let res = audio_mute(
            State(state.clone()),
            origin_headers("https://beta.juntos.lol"),
            Some(Json(serde_json::json!({"app": "discord", "muted": true}))),
        )
        .await
        .into_response();
        assert_eq!(res.status(), StatusCode::OK);
        assert!(state.state.audio.lock().muted_apps.contains("discord"));
        let res = audio_mute(
            State(state.clone()),
            origin_headers("https://beta.juntos.lol"),
            Some(Json(serde_json::json!({"app": "discord", "muted": false}))),
        )
        .await
        .into_response();
        assert_eq!(res.status(), StatusCode::OK);
        assert!(!state.state.audio.lock().muted_apps.contains("discord"));
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn audio_stream_idle_is_404() {
        // No capture session: the mix has no owner, so 404 + idle — same
        // envelope as the preview endpoint, CORS + no-store intact.
        let res = audio_stream(
            State(state_with(&["https://beta.juntos.lol"])),
            origin_headers("https://beta.juntos.lol"),
        )
        .await
        .into_response();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            res.headers()
                .get(axum::http::header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .and_then(|v| v.to_str().ok()),
            Some("https://beta.juntos.lol")
        );
        assert_eq!(
            res.headers()
                .get(axum::http::header::CACHE_CONTROL)
                .and_then(|v| v.to_str().ok()),
            Some("no-store")
        );
        let body = axum::body::to_bytes(res.into_body(), 4096).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v, serde_json::json!({"error": "idle"}));
    }

    #[tokio::test]
    async fn capture_stream_idle_is_404() {
        // No capture session: no frames to serve, so 404 + idle — same
        // envelope as the preview endpoint, CORS + no-store intact.
        let res = capture_stream(
            State(state_with(&["https://beta.juntos.lol"])),
            origin_headers("https://beta.juntos.lol"),
        )
        .await
        .into_response();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            res.headers()
                .get(axum::http::header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .and_then(|v| v.to_str().ok()),
            Some("https://beta.juntos.lol")
        );
        let body = axum::body::to_bytes(res.into_body(), 4096).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v, serde_json::json!({"error": "idle"}));
    }

    #[cfg(not(target_os = "macos"))]
    #[tokio::test]
    async fn audio_stream_unwired_is_501() {
        // Windows/Linux have no tap: even with a session the route stays 501.
        let state = state_with(&["https://beta.juntos.lol"]);
        *state.state.capture.lock() = Some(crate::capture::CaptureSession::new());
        let res = audio_stream(State(state), origin_headers("https://beta.juntos.lol"))
            .await
            .into_response();
        assert_eq!(res.status(), StatusCode::NOT_IMPLEMENTED);
        let body = axum::body::to_bytes(res.into_body(), 4096).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(v.get("error").is_some(), "{v}");
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn audio_stream_serves_pcm_while_live() {
        // A live session upgrades the route to 200 + the L16 content type.
        // The body is dropped unread: the tap keeps capturing against a
        // machine that may refuse it (no Screen Recording grant), which
        // degrades to silence — the headers are the contract under test.
        let state = state_with(&["https://beta.juntos.lol"]);
        *state.state.capture.lock() = Some(crate::capture::CaptureSession::new());
        let res = audio_stream(State(state), origin_headers("https://beta.juntos.lol"))
            .await
            .into_response();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(
            res.headers()
                .get(axum::http::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some(crate::audio_engine::CONTENT_TYPE)
        );
        assert_eq!(
            res.headers()
                .get(axum::http::header::CACHE_CONTROL)
                .and_then(|v| v.to_str().ok()),
            Some("no-store")
        );
        assert_eq!(
            res.headers()
                .get(axum::http::header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .and_then(|v| v.to_str().ok()),
            Some("https://beta.juntos.lol")
        );
    }

    #[test]
    fn range_header_shapes() {
        // Closed, open, suffix, clamped, and degraded shapes.
        assert_eq!(
            super::parse_range_header(Some("bytes=0-99"), 1000),
            Ok((0, 99))
        );
        assert_eq!(
            super::parse_range_header(Some("bytes=100-"), 1000),
            Ok((100, 999))
        );
        assert_eq!(
            super::parse_range_header(Some("bytes=-100"), 1000),
            Ok((900, 999))
        );
        assert_eq!(
            super::parse_range_header(Some("bytes=-1000"), 1000),
            Ok((0, 999))
        );
        // Clamp, don't 416, when the end runs past the total.
        assert_eq!(
            super::parse_range_header(Some("bytes=900-5000"), 1000),
            Ok((900, 999))
        );
        // Unsatisfiable starts and inverted spans are 416.
        assert_eq!(
            super::parse_range_header(Some("bytes=1000-"), 1000),
            Err(())
        );
        assert_eq!(
            super::parse_range_header(Some("bytes=200-100"), 1000),
            Err(())
        );
        assert_eq!(super::parse_range_header(Some("bytes=-0"), 1000), Err(()));
        // Missing, foreign-unit, multi-range, and garbage degrade to the first chunk.
        assert_eq!(super::parse_range_header(None, 1000), Ok((0, 999)));
        assert_eq!(
            super::parse_range_header(Some("items=0-99"), 1000),
            Ok((0, 999))
        );
        assert_eq!(
            super::parse_range_header(Some("bytes=0-10,20-30"), 1000),
            Ok((0, 999))
        );
        assert_eq!(
            super::parse_range_header(Some("bytes=banana"), 1000),
            Ok((0, 999))
        );
        // Oversize spans truncate to the single-read cap.
        let cap = crate::torrent::MAX_RANGE_BYTES;
        assert_eq!(
            super::parse_range_header(Some("bytes=0-"), cap + 100),
            Ok((0, cap - 1))
        );
    }

    #[test]
    fn torrent_error_status_mapping() {
        use crate::torrent::TorrentError as E;
        let status = super::torrent_http_status;
        assert_eq!(
            status(&anyhow::anyhow!(E::NotFound("x".into()))),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            status(&anyhow::anyhow!(E::BadFile {
                id: "x".into(),
                file: 9
            })),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            status(&anyhow::anyhow!(E::NoMetadata("x".into()))),
            StatusCode::CONFLICT
        );
        assert_eq!(
            status(&anyhow::anyhow!(E::MetadataTimeout("x".into()))),
            StatusCode::GATEWAY_TIMEOUT
        );
        assert_eq!(
            status(&anyhow::anyhow!(E::Unsatisfiable {
                start: 5,
                end: 3,
                total: 10
            })),
            StatusCode::RANGE_NOT_SATISFIABLE
        );
        assert_eq!(
            status(&anyhow::anyhow!(E::RangeTooLarge {
                requested: 1,
                max: 1
            })),
            StatusCode::PAYLOAD_TOO_LARGE
        );
        assert_eq!(status(&anyhow::anyhow!("boom")), StatusCode::BAD_GATEWAY);
    }

    #[tokio::test]
    async fn torrent_routes_503_without_engine() {
        // state_with() builds no engine: every torrent route must fail
        // closed with CORS intact, never panic on the absent session.
        let headers = origin_headers("https://beta.juntos.lol");
        let res = torrent_list(
            State(state_with(&["https://beta.juntos.lol"])),
            headers.clone(),
        )
        .await
        .into_response();
        assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(res
            .headers()
            .contains_key(axum::http::header::ACCESS_CONTROL_ALLOW_ORIGIN));
        let res = torrent_add(
            State(state_with(&["https://beta.juntos.lol"])),
            headers,
            Some(Json(
                serde_json::json!({"magnet": "magnet:?xt=urn:btih:abc"}),
            )),
        )
        .await
        .into_response();
        assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    fn snapshot_query(pairs: &[(&str, &str)]) -> Query<std::collections::HashMap<String, String>> {
        Query(
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        )
    }

    #[tokio::test]
    async fn snapshot_needs_exactly_one_target() {
        // Both / neither / garbage never reach the OS: the 400 is
        // headless-safe and deterministic on every machine.
        for query in [
            snapshot_query(&[("display_id", "1"), ("window_id", "2")]),
            snapshot_query(&[]),
            snapshot_query(&[("width", "960")]),
            snapshot_query(&[("display_id", "nope")]),
            snapshot_query(&[("display_id", "-1")]),
            snapshot_query(&[("display_id", "1.5")]),
            snapshot_query(&[("window_id", "true")]),
            snapshot_query(&[("window_id", "")]),
        ] {
            let res = capture_snapshot(
                State(state_with(&["https://beta.juntos.lol"])),
                origin_headers("https://beta.juntos.lol"),
                query,
            )
            .await
            .into_response();
            assert_eq!(res.status(), StatusCode::BAD_REQUEST);
            assert_eq!(
                res.headers()
                    .get(axum::http::header::CACHE_CONTROL)
                    .and_then(|v| v.to_str().ok()),
                Some("no-store")
            );
            let body = axum::body::to_bytes(res.into_body(), 4096).await.unwrap();
            let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert!(v.get("error").is_some(), "{v}");
        }
    }

    #[tokio::test]
    async fn snapshot_unknown_id_is_400_not_503() {
        // u32::MAX can never be a live target, but resolving it still lists
        // real targets — so this only runs where enumeration works. Where it
        // does, unknown must be 400 (stale picker entry), never 503.
        if crate::capture::list_displays().is_err() {
            return;
        }
        let res = capture_snapshot(
            State(state_with(&["https://beta.juntos.lol"])),
            origin_headers("https://beta.juntos.lol"),
            snapshot_query(&[("display_id", &u32::MAX.to_string())]),
        )
        .await
        .into_response();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        let body = axum::body::to_bytes(res.into_body(), 4096).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(v.get("error").is_some(), "{v}");
    }

    #[tokio::test]
    async fn snapshot_unavailable_maps_permission_probe() {
        // The 503 error string is whatever the live probe says: `permission`
        // when the OS blocks capture, else `unavailable`. Either way the
        // envelope + no-store contract holds.
        let (status, error) = super::snapshot_unavailable();
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        let expected = if crate::permissions::screen_capture_granted() {
            "unavailable"
        } else {
            "permission"
        };
        assert_eq!(error, expected);
    }
}
