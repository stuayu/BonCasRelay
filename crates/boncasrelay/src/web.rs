use crate::control::{Control, Settings};
use axum::{
    extract::{ConnectInfo, DefaultBodyLimit, State},
    http::{header, Request, StatusCode},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use boncasrelay_core::Result;
use serde_json::{json, Value};
use std::{
    net::SocketAddr,
    sync::{atomic::Ordering, Arc},
    thread,
    time::Duration,
};

#[derive(Clone)]
struct Web {
    control: Arc<Control>,
    address: SocketAddr,
    token: String,
}
fn error(status: StatusCode, message: impl ToString) -> Response {
    (status, Json(json!({"error":message.to_string()}))).into_response()
}
// Numeric authorities prevent DNS rebinding. On wildcard listeners accept only the
// numeric destination IP, never a DNS name or forwarded authority.
async fn guard(State(web): State<Web>, request: Request<axum::body::Body>, next: Next) -> Response {
    let host = request
        .headers()
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    let host_addr = host.parse::<SocketAddr>().ok();
    let valid_host = host_addr.is_some_and(|h| {
        h.port() == web.address.port()
            && if web.address.ip().is_unspecified() {
                !h.ip().is_unspecified() && !h.ip().is_multicast()
            } else {
                h.ip() == web.address.ip()
            }
    });
    if !valid_host {
        return error(
            StatusCode::FORBIDDEN,
            "Invalid Host; use a numeric IP address and port",
        );
    }
    let expected_origin = format!("http://{host}");
    if request
        .headers()
        .get(header::ORIGIN)
        .is_some_and(|o| o.as_bytes() != expected_origin.as_bytes())
        || request
            .headers()
            .get("sec-fetch-site")
            .is_some_and(|v| v == "cross-site")
    {
        return error(StatusCode::FORBIDDEN, "Cross-origin request denied");
    }
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|p| p.0.ip());
    let api = request.uri().path().starts_with("/api/");
    if api {
        if web.token.is_empty() {
            if !web.address.ip().is_loopback() || !peer.is_some_and(|ip| ip.is_loopback()) {
                return error(StatusCode::UNAUTHORIZED, "Loopback access required");
            }
        } else {
            let expected = format!("Bearer {}", web.token);
            let actual = request
                .headers()
                .get(header::AUTHORIZATION)
                .map(|v| v.as_bytes())
                .unwrap_or_default();
            // Fixed-length comparison avoids early exits on a token prefix.
            let difference = actual
                .iter()
                .zip(expected.as_bytes())
                .fold(actual.len() ^ expected.len(), |d, (a, b)| {
                    d | usize::from(a ^ b)
                });
            if difference != 0 {
                return error(StatusCode::UNAUTHORIZED, "Bearer token required");
            }
        }
        if request.method() != axum::http::Method::GET
            && request
                .headers()
                .get("x-boncas-request")
                .is_none_or(|v| v != "1")
        {
            return error(StatusCode::FORBIDDEN, "X-BonCas-Request: 1 required");
        }
    }
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(
        header::CACHE_CONTROL,
        "no-store".parse().expect("constant header"),
    );
    headers.insert(
        "x-content-type-options",
        "nosniff".parse().expect("constant header"),
    );
    headers.insert(
        "referrer-policy",
        "no-referrer".parse().expect("constant header"),
    );
    headers.insert(header::CONTENT_SECURITY_POLICY, "default-src 'self'; script-src 'self'; style-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'".parse().expect("constant header"));
    response
}
async fn state(State(web): State<Web>) -> Json<Value> {
    Json(
        json!({"snapshot":web.control.router.state.snapshot(), "settings":Settings::from(&*web.control.config.read()), "web_listen":web.address, "version":env!("CARGO_PKG_VERSION")}),
    )
}
async fn config(State(web): State<Web>) -> Json<Value> {
    let c = web.control.config.read();
    Json(
        json!({"settings":Settings::from(&*c),"cli_overrides":c.cli_overrides,"config_path":web.control.path,"restart_note":"CLI overrides apply again on the next startup"}),
    )
}
async fn update(
    State(web): State<Web>,
    settings: std::result::Result<Json<Settings>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let settings = match settings {
        Ok(Json(s)) => s,
        Err(e) => return error(StatusCode::BAD_REQUEST, e.body_text()),
    };
    match tokio::task::spawn_blocking(move || web.control.apply(settings)).await {
        Ok(Ok(())) => Json(json!({"ok":true})).into_response(),
        Ok(Err(e)) => error(StatusCode::BAD_REQUEST, e),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}
async fn refresh(State(web): State<Web>) -> Response {
    match tokio::task::spawn_blocking(move || web.control.router.refresh()).await {
        Ok(Ok(())) => Json(json!({"ok":true})).into_response(),
        Ok(Err(e)) => error(StatusCode::TOO_MANY_REQUESTS, e),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}
async fn readers(State(web): State<Web>) -> Json<Value> {
    let s = web.control.router.state.snapshot();
    Json(
        json!({"readers":s.readers,"classifications":s.classifications,"routes":s.routes,"fixed_routes":s.fixed_routes}),
    )
}
fn app(web: Web) -> Router {
    Router::new()
        .route(
            "/",
            get(|| async { Html(include_str!("../assets/index.html")) }),
        )
        .route(
            "/app.js",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
                    include_str!("../assets/app.js"),
                )
            }),
        )
        .route(
            "/style.css",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
                    include_str!("../assets/style.css"),
                )
            }),
        )
        .route(
            "/favicon.png",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "image/png")],
                    include_bytes!("../../../assets/idle.png").as_slice(),
                )
            }),
        )
        .route("/api/state", get(state))
        .route("/api/config", get(config).put(update))
        .route("/api/readers", get(readers))
        .route("/api/refresh", post(refresh))
        .layer(DefaultBodyLimit::max(32 * 1024))
        .layer(middleware::from_fn_with_state(web.clone(), guard))
        .with_state(web)
}
fn ensure_token(control: &Control) -> Result<()> {
    let mut c = control.config.write();
    if c.no_web || c.web_listen.ip().is_loopback() || !c.web_token.is_empty() {
        return Ok(());
    }
    // Token bootstrap must not accidentally persist temporary CLI overrides.
    let mut saved = boncasrelay_core::config::Config::default();
    if control.path.exists() {
        saved.load_file(&control.path)?;
    }
    if saved.web_token.is_empty() {
        let mut bytes = [0; 32];
        getrandom::getrandom(&mut bytes).map_err(|e| e.to_string())?;
        saved.web_token = bytes.iter().map(|b| format!("{b:02x}")).collect();
        saved.save(&control.path)?;
        control.router.state.log(format!(
            "Web token generated and saved to {}: {}",
            control.path.display(),
            saved.web_token
        ));
    }
    c.web_token = saved.web_token;
    Ok(())
}
pub fn start(control: Arc<Control>) -> Result<Option<thread::JoinHandle<()>>> {
    ensure_token(&control)?;
    let c = control.config.read();
    if c.no_web {
        return Ok(None);
    }
    let address = c.web_listen;
    let token = c.web_token.clone();
    drop(c);
    let listener = match std::net::TcpListener::bind(address) {
        Ok(l) => l,
        Err(e) => {
            control.router.state.log(format!(
                "Web unavailable on {address}: {e}; TCP server continues"
            ));
            return Ok(None);
        }
    };
    listener.set_nonblocking(true).map_err(|e| e.to_string())?;
    *control.web_address.write() = Some(address);
    control
        .router
        .state
        .log(format!("Web UI: http://{address}/"));
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    let handle = thread::Builder::new()
        .name("boncas-web".into())
        .spawn(move || {
            let shutdown = control.stop.clone();
            let log = control.router.state.clone();
            runtime.block_on(async move {
                let listener = match tokio::net::TcpListener::from_std(listener) {
                    Ok(listener) => listener,
                    Err(error) => {
                        log.log(format!("Web failed: {error}"));
                        *control.web_address.write() = None;
                        return;
                    }
                };
                let service = app(Web {
                    control: control.clone(),
                    address,
                    token,
                })
                .into_make_service_with_connect_info::<SocketAddr>();
                let server = axum::serve(listener, service).with_graceful_shutdown(async move {
                    while !shutdown.load(Ordering::Relaxed) {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                });
                // A partial HTTP request must not prevent tray or signal shutdown.
                let result = tokio::select! {
                    result = async { server.await } => result,
                    () = async {
                        while !control.stop.load(Ordering::Relaxed) {
                            tokio::time::sleep(Duration::from_millis(100)).await;
                        }
                        tokio::time::sleep(Duration::from_secs(2)).await;
                    } => Ok(()),
                };
                if let Err(error) = result {
                    log.log(format!("Web failed: {error}"));
                }
                *control.web_address.write() = None;
            });
        })
        .map_err(|e| e.to_string())?;
    Ok(Some(handle))
}

#[cfg(test)]
#[path = "web_tests.rs"]
mod tests;
