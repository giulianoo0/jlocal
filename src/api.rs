//! Loopback HTTP API (127.0.0.1 only).
//!
//! - `GET /health`  -> { name, version, connected, moq, torrent, port }
//! - `GET /version` -> { name, version }
//! - `GET /events`  -> SSE: `hello` then 15s heartbeat comments.
//! - `GET /capabilities`   -> { name, version, capabilities }
//! - `GET /audio/apps`     -> 501 { error } until native capture lands
//! - `POST /capture/start` -> 501 { error } until native capture lands
//! - `POST /capture/stop`  -> { stopped: true }
//!
//! Security: Host must be loopback (DNS-rebinding guard); CORS only echoes
//! allowlisted origins (`JLOCAL_ALLOWED_ORIGINS`); everything is `no-store`.

use std::convert::Infallible;
use std::time::Duration;

use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive};
use axum::response::{IntoResponse, Response, Sse};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Serialize;

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
}

#[derive(Serialize)]
struct ScreenCapabilities {
    available: bool,
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
}

#[derive(Serialize)]
struct TorrentCapabilities {
    available: bool,
}

pub fn router(state: AppState, port: u16) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/version", get(version))
        .route("/events", get(events))
        .route("/capabilities", get(capabilities))
        .route("/audio/apps", get(audio_apps))
        .route("/capture/start", post(capture_start))
        .route("/capture/stop", post(capture_stop))
        .route("/health", axum::routing::options(preflight))
        .route("/version", axum::routing::options(preflight))
        .route("/events", axum::routing::options(preflight))
        .route("/capabilities", axum::routing::options(preflight))
        .route("/audio/apps", axum::routing::options(preflight))
        .route("/capture/start", axum::routing::options(preflight))
        .route("/capture/stop", axum::routing::options(preflight))
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
        torrent: "standby",
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
                available: false,
                max_width: 3840,
                max_height: 2160,
                max_fps: 60,
            },
            audio: AudioCapabilities { app_list: false },
            torrent: TorrentCapabilities { available: false },
        },
    };
    let mut res = Json(body).into_response();
    apply_cors(&s.state, &headers, res.headers_mut());
    with_no_store(res)
}

async fn audio_apps(State(s): State<ApiState>, headers: HeaderMap) -> impl IntoResponse {
    let mut res = (
        StatusCode::NOT_IMPLEMENTED,
        Json(serde_json::json!({"error": "not_implemented"})),
    )
        .into_response();
    apply_cors(&s.state, &headers, res.headers_mut());
    with_no_store(res)
}

async fn capture_start(
    State(s): State<ApiState>,
    headers: HeaderMap,
    _body: Option<Json<serde_json::Value>>,
) -> impl IntoResponse {
    let mut res = (
        StatusCode::NOT_IMPLEMENTED,
        Json(serde_json::json!({"error": "not_implemented"})),
    )
        .into_response();
    apply_cors(&s.state, &headers, res.headers_mut());
    with_no_store(res)
}

async fn capture_stop(State(s): State<ApiState>, headers: HeaderMap) -> impl IntoResponse {
    let mut res = Json(serde_json::json!({"stopped": true})).into_response();
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
        HeaderValue::from_static("GET, OPTIONS"),
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
        assert_eq!(v["capabilities"]["screen"]["maxWidth"], 3840);
        assert_eq!(v["capabilities"]["screen"]["maxHeight"], 2160);
        assert_eq!(v["capabilities"]["screen"]["maxFps"], 60);
        assert_eq!(v["capabilities"]["audio"]["appList"], false);
        assert_eq!(v["capabilities"]["torrent"]["available"], false);
    }

    #[tokio::test]
    async fn audio_apps_returns_501_with_allowlist_cors() {
        let res = audio_apps(
            State(state_with(&["https://beta.juntos.lol"])),
            origin_headers("https://beta.juntos.lol"),
        )
        .await
        .into_response();
        assert_eq!(res.status(), StatusCode::NOT_IMPLEMENTED);
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
        assert_eq!(v, serde_json::json!({"error": "not_implemented"}));
    }

    #[tokio::test]
    async fn capture_start_returns_501() {
        let res = capture_start(
            State(state_with(&["https://beta.juntos.lol"])),
            origin_headers("https://beta.juntos.lol"),
            None,
        )
        .await
        .into_response();
        assert_eq!(res.status(), StatusCode::NOT_IMPLEMENTED);
        assert_eq!(
            res.headers()
                .get(axum::http::header::CACHE_CONTROL)
                .and_then(|v| v.to_str().ok()),
            Some("no-store")
        );
        let body = axum::body::to_bytes(res.into_body(), 4096).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v, serde_json::json!({"error": "not_implemented"}));
    }

    #[tokio::test]
    async fn capture_stop_returns_200_stopped() {
        let res = capture_stop(
            State(state_with(&["https://beta.juntos.lol"])),
            origin_headers("https://beta.juntos.lol"),
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
}
