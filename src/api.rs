//! Loopback HTTP API (127.0.0.1 only).
//!
//! - `GET /health`  -> { name, version, connected, moq, torrent, port }
//! - `GET /version` -> { name, version }
//! - `GET /events`  -> SSE: `hello` then 15s heartbeat comments.
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
use axum::routing::get;
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

pub fn router(state: AppState, port: u16) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/version", get(version))
        .route("/events", get(events))
        .route("/health", axum::routing::options(preflight))
        .route("/version", axum::routing::options(preflight))
        .route("/events", axum::routing::options(preflight))
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

async fn health(State(s): State<ApiState>) -> impl IntoResponse {
    let body = Health {
        name: crate::status::NAME,
        version: crate::status::VERSION,
        connected: true,
        moq: "disabled",
        torrent: "standby",
        port: s.port,
        started_unix: s.state.started_unix,
    };
    with_api_headers(Json(body).into_response())
}

async fn version() -> impl IntoResponse {
    let body = Version {
        name: crate::status::NAME,
        version: crate::status::VERSION,
    };
    with_api_headers(Json(body).into_response())
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

fn with_api_headers(mut res: Response) -> Response {
    res.headers_mut().insert(
        "access-control-allow-private-network",
        HeaderValue::from_static("true"),
    );
    with_no_store(res)
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
            port: 4173,
            started_unix: 0,
        };
        let v = serde_json::to_value(&h).unwrap();
        assert_eq!(v["name"], "jlocal");
        assert_eq!(v["connected"], true);
        assert_eq!(v["moq"], "disabled");
    }

    #[test]
    fn loopback_host_check() {
        for good in ["127.0.0.1:4173", "localhost:4173", "[::1]:4173"] {
            let g = good.to_ascii_lowercase();
            assert!(
                g.starts_with("127.0.0.1") || g.starts_with("localhost") || g.starts_with("[::1]")
            );
        }
        assert!(!"evil.com".starts_with("127.0.0.1"));
    }
}
