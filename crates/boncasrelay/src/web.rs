use crate::control::{Control, Settings};
use axum::{
    extract::{DefaultBodyLimit, State},
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
}
fn error(status: StatusCode, message: impl ToString) -> Response {
    (status, Json(json!({"error":message.to_string()}))).into_response()
}
async fn response_headers(request: Request<axum::body::Body>, next: Next) -> Response {
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
        .layer(middleware::from_fn(response_headers))
        .with_state(web)
}
pub fn start(control: Arc<Control>) -> Result<Option<thread::JoinHandle<()>>> {
    let c = control.config.read();
    if c.no_web {
        return Ok(None);
    }
    let address = c.web_listen;
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
    if !address.ip().is_loopback() {
        control.router.state.log(format!("Web 設定画面は認証なしで {address} に公開されています。ファイアウォールでアクセス元を制限してください"));
    }
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
