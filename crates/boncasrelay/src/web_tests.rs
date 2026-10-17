use super::*;
use axum::body::{to_bytes, Body};
use boncasrelay_core::{
    backend::MockBackend, config::Config, router::Router as CardRouter, state::State as CardState,
};
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::atomic::AtomicBool,
};
use tower::ServiceExt;

struct Fixture {
    _dir: tempfile::TempDir,
    control: Arc<Control>,
}
fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            address: "127.0.0.1".parse().unwrap(),
            port: free_port(),
            mock: true,
            config_path: Some(dir.path().join("server.ini")),
            ..Config::default()
        };
        let state = CardState::default();
        let router = Arc::new(
            CardRouter::new(
                Arc::new(MockBackend::new(state.clone())),
                config.clone(),
                state,
                Arc::new(AtomicBool::new(false)),
            )
            .unwrap(),
        );
        let control = Control::new(config, router).unwrap();
        Self { _dir: dir, control }
    }
    fn web(&self) -> Web {
        Web {
            control: self.control.clone(),
            address: "127.0.0.1:6980".parse().unwrap(),
            token: String::new(),
        }
    }
    fn settings(&self) -> Settings {
        Settings::from(&*self.control.config.read())
    }
}
fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}
async fn call(
    web: Web,
    method: &str,
    path: &str,
    body: Value,
    extra: &[(&str, &str)],
    peer: &str,
) -> (StatusCode, Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::HOST, web.address.to_string())
        .header(header::CONTENT_TYPE, "application/json");
    for (name, value) in extra {
        request = request.header(*name, *value);
    }
    let mut request = request.body(Body::from(body.to_string())).unwrap();
    request
        .extensions_mut()
        .insert(ConnectInfo(peer.parse::<SocketAddr>().unwrap()));
    let response = app(web).oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}
async fn put(f: &Fixture, body: Value) -> (StatusCode, Value) {
    call(
        f.web(),
        "PUT",
        "/api/config",
        body,
        &[("x-boncas-request", "1")],
        "127.0.0.1:12345",
    )
    .await
}
fn assert_error(status: StatusCode, body: &Value, expected: StatusCode) {
    assert_eq!(status, expected, "{body}");
    assert!(
        body["error"].as_str().is_some_and(|s| !s.is_empty()),
        "{body}"
    );
}

#[test]
fn state_readers_and_config_expose_mock_data() {
    let f = Fixture::new();
    f.control.router.refresh().unwrap();
    runtime().block_on(async {
        for path in ["/api/state", "/api/readers", "/api/config"] {
            let (status, body) =
                call(f.web(), "GET", path, Value::Null, &[], "127.0.0.1:12345").await;
            assert_eq!(status, StatusCode::OK, "{body}");
            match path {
                "/api/state" => {
                    assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
                    assert_eq!(body["snapshot"]["readers"].as_array().unwrap().len(), 3);
                    assert_eq!(body["settings"]["port"], f.settings().port);
                }
                "/api/readers" => {
                    assert_eq!(body["readers"].as_array().unwrap().len(), 3);
                    assert!(body["classifications"]
                        .as_object()
                        .unwrap()
                        .contains_key("Mock B-CAS"));
                    assert_eq!(body["routes"].as_array().unwrap().len(), 2);
                }
                _ => {
                    assert_eq!(body["config_path"], f.control.path.to_str().unwrap());
                    assert!(body["cli_overrides"].is_array());
                    assert!(body["restart_note"].is_string());
                }
            }
            assert!(body.get("token").is_none());
        }
    });
}

#[test]
fn valid_update_persists_and_reloads_settings() {
    let f = Fixture::new();
    let mut settings = f.settings();
    settings.verbose = true;
    settings.allow = vec!["127.0.0.1/32".into(), "::1/128".into()];
    settings.pins = ["Mock B-CAS".into(), "Mock ACAS".into()];
    let wanted = serde_json::to_value(&settings).unwrap();
    let (status, body) = runtime().block_on(put(&f, wanted.clone()));
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!({"ok":true}));
    assert_eq!(serde_json::to_value(f.settings()).unwrap(), wanted);
    let mut loaded = Config::default();
    loaded.load_file(&f.control.path).unwrap();
    assert_eq!(
        serde_json::to_value(Settings::from(&loaded)).unwrap(),
        wanted
    );
    // Replacing an existing file must also produce one complete loadable config.
    settings.verbose = false;
    f.control.apply(settings).unwrap();
    let mut loaded = Config::default();
    loaded.load_file(&f.control.path).unwrap();
    assert!(!loaded.verbose);
    assert_eq!(std::fs::read_dir(f._dir.path()).unwrap().count(), 1);
}

#[test]
fn malformed_updates_return_json_errors_without_mutation() {
    let f = Fixture::new();
    f.control.config.read().save(&f.control.path).unwrap();
    let original = serde_json::to_value(f.settings()).unwrap();
    let saved = std::fs::read(&f.control.path).unwrap();
    let mut cases = Vec::new();
    let mut value = original.clone();
    value["allow"] = json!(["127.0.0.1/99"]);
    cases.push(value);
    let mut value = original.clone();
    value["port"] = json!(0);
    cases.push(value);
    let mut value = original.clone();
    value["fixed"] = json!([{"port":6901,"reader":"a"},{"port":6901,"reader":"b"}]);
    cases.push(value);
    let mut value = original.clone();
    value["fixed"] = json!([{"port":f.settings().port,"reader":"a"}]);
    cases.push(value);
    let mut value = original.clone();
    value["fixed"] = json!([{"port":0,"reader":"a"}]);
    cases.push(value);
    let mut value = original.clone();
    value["pins"] = json!(["reader\n[Web]\nToken=evil", ""]);
    cases.push(value);
    let mut value = original.clone();
    value["fixed"] = json!([{"port":6901,"reader":"reader\r\n[Web]"}]);
    cases.push(value);
    let mut value = original.clone();
    value["unknown"] = json!(true);
    cases.push(value);
    cases.push(json!({"port":"invalid"}));
    runtime().block_on(async {
        for value in cases {
            let (status, body) = put(&f, value.clone()).await;
            assert_error(status, &body, StatusCode::BAD_REQUEST);
            assert_eq!(
                serde_json::to_value(f.settings()).unwrap(),
                original,
                "{value}"
            );
            assert_eq!(std::fs::read(&f.control.path).unwrap(), saved);
        }
    });
}

#[test]
fn remote_api_requires_token_and_checks_full_bearer() {
    let f = Fixture::new();
    runtime().block_on(async {
        let mut web = f.web();
        web.address = "192.0.2.1:6980".parse().unwrap();
        let (status, body) = call(
            web.clone(),
            "GET",
            "/api/state",
            Value::Null,
            &[],
            "192.0.2.2:12345",
        )
        .await;
        assert_error(status, &body, StatusCode::UNAUTHORIZED);
        web.token = "secret-token".into();
        for token in [
            None,
            Some("Bearer secret"),
            Some("Bearer secret-token-extra"),
            Some("bearer secret-token"),
        ] {
            let headers = token
                .map(|v| vec![("authorization", v)])
                .unwrap_or_default();
            let (status, body) = call(
                web.clone(),
                "GET",
                "/api/state",
                Value::Null,
                &headers,
                "192.0.2.2:12345",
            )
            .await;
            assert_error(status, &body, StatusCode::UNAUTHORIZED);
        }
        let (status, body) = call(
            web,
            "GET",
            "/api/state",
            Value::Null,
            &[("authorization", "Bearer secret-token")],
            "192.0.2.2:12345",
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (status, body) = call(
            f.web(),
            "GET",
            "/api/state",
            Value::Null,
            &[],
            "192.0.2.2:12345",
        )
        .await;
        assert_error(status, &body, StatusCode::UNAUTHORIZED);
    });
}

#[test]
fn csrf_host_origin_and_custom_header_are_enforced() {
    let f = Fixture::new();
    let body = serde_json::to_value(f.settings()).unwrap();
    runtime().block_on(async {
        for headers in [
            vec![
                ("origin", "https://evil.example"),
                ("x-boncas-request", "1"),
            ],
            vec![("sec-fetch-site", "cross-site"), ("x-boncas-request", "1")],
            vec![],
            vec![("x-boncas-request", "0")],
        ] {
            let (status, response) = call(
                f.web(),
                "PUT",
                "/api/config",
                body.clone(),
                &headers,
                "127.0.0.1:12345",
            )
            .await;
            assert_error(status, &response, StatusCode::FORBIDDEN);
        }
        // Construct this request directly so there is only one hostile Host header.
        for host in ["evil.example:6980", "127.0.0.1:6981", "192.0.2.1:6980", ""] {
            let mut request = Request::builder()
                .uri("/api/state")
                .header(header::HOST, host)
                .body(Body::empty())
                .unwrap();
            request.extensions_mut().insert(ConnectInfo(
                "127.0.0.1:12345".parse::<SocketAddr>().unwrap(),
            ));
            let response = app(f.web()).oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::FORBIDDEN, "Host {host}");
            let value: Value =
                serde_json::from_slice(&to_bytes(response.into_body(), 4096).await.unwrap())
                    .unwrap();
            assert!(value["error"].is_string());
        }
        let (status, response) = call(
            f.web(),
            "PUT",
            "/api/config",
            body,
            &[
                ("origin", "http://127.0.0.1:6980"),
                ("x-boncas-request", "1"),
            ],
            "127.0.0.1:12345",
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{response}");
    });
}

#[test]
fn listener_change_keeps_established_connection_and_releases_old_port() {
    let f = Fixture::new();
    let old_port = f.settings().port;
    let mut client = TcpStream::connect(("127.0.0.1", old_port)).unwrap();
    let listener = f.control.listeners.lock()[0].0.try_clone().unwrap();
    let mut accepted = loop {
        match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::yield_now()
            }
            Err(error) => panic!("accept: {error}"),
        }
    };
    drop(listener);
    accepted.set_nonblocking(false).unwrap();
    accepted
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut settings = f.settings();
    settings.port = free_port();
    let new_port = settings.port;
    f.control.apply(settings).unwrap();
    assert_eq!(
        f.control.listeners.lock()[0].0.local_addr().unwrap().port(),
        new_port
    );
    let _new_client = TcpStream::connect(("127.0.0.1", new_port)).unwrap();
    // The old listening socket closed while the accepted stream stayed alive.
    assert!(TcpStream::connect(("127.0.0.1", old_port)).is_err());
    client.write_all(b"alive").unwrap();
    let mut bytes = [0; 5];
    accepted.read_exact(&mut bytes).unwrap();
    assert_eq!(&bytes, b"alive");
    accepted.write_all(b"reply").unwrap();
    client.read_exact(&mut bytes).unwrap();
    assert_eq!(&bytes, b"reply");
}

#[test]
fn bind_failure_rolls_back_settings_file_and_live_listener() {
    let f = Fixture::new();
    f.control.config.read().save(&f.control.path).unwrap();
    let saved = std::fs::read(&f.control.path).unwrap();
    let original = serde_json::to_value(f.settings()).unwrap();
    let occupied = TcpListener::bind("127.0.0.1:0").unwrap();
    let mut settings = f.settings();
    settings.port = occupied.local_addr().unwrap().port();
    settings.verbose = true;
    let (status, body) = runtime().block_on(put(&f, serde_json::to_value(settings).unwrap()));
    assert_error(status, &body, StatusCode::BAD_REQUEST);
    assert_eq!(serde_json::to_value(f.settings()).unwrap(), original);
    assert_eq!(std::fs::read(&f.control.path).unwrap(), saved);
    assert!(TcpStream::connect(("127.0.0.1", f.settings().port)).is_ok());
}

#[test]
fn persistence_failure_rolls_back_and_closes_staged_listener() {
    let f = Fixture::new();
    // A directory at the destination produces a deterministic rename failure,
    // including when tests run with permissions that bypass chmod restrictions.
    std::fs::create_dir(&f.control.path).unwrap();
    let original = serde_json::to_value(f.settings()).unwrap();
    let mut settings = f.settings();
    settings.port = free_port();
    settings.verbose = true;
    let staged_port = settings.port;
    let (status, body) = runtime().block_on(put(&f, serde_json::to_value(settings).unwrap()));
    assert_error(status, &body, StatusCode::BAD_REQUEST);
    assert_eq!(serde_json::to_value(f.settings()).unwrap(), original);
    assert!(f.control.path.is_dir());
    assert!(TcpStream::connect(("127.0.0.1", f.settings().port)).is_ok());
    assert!(TcpListener::bind(("127.0.0.1", staged_port)).is_ok());
    assert_eq!(std::fs::read_dir(f._dir.path()).unwrap().count(), 1);
}

#[test]
fn same_port_address_changes_between_specific_and_wildcard() {
    let f = Fixture::new();
    let port = f.settings().port;
    for address in ["0.0.0.0", "127.0.0.1"] {
        let mut settings = f.settings();
        settings.address = address.parse().unwrap();
        f.control.apply(settings).unwrap();
        let listeners = f.control.listeners.lock();
        assert_eq!(listeners.len(), 1);
        assert_eq!(
            listeners[0].0.local_addr().unwrap(),
            SocketAddr::new(address.parse().unwrap(), port)
        );
        drop(listeners);
        assert!(TcpStream::connect(("127.0.0.1", port)).is_ok());
        let mut loaded = Config::default();
        loaded.load_file(&f.control.path).unwrap();
        assert_eq!(loaded.address.to_string(), address);
        assert_eq!(loaded.port, port);
    }
}

#[test]
fn same_port_address_change_restores_released_socket_on_save_failure() {
    let f = Fixture::new();
    std::fs::create_dir(&f.control.path).unwrap();
    let previous = serde_json::to_value(f.settings()).unwrap();
    let port = f.settings().port;
    let mut settings = f.settings();
    settings.address = "0.0.0.0".parse().unwrap();
    let error = f.control.apply(settings).unwrap_err();
    assert!(!error.contains("listener rollback failed"), "{error}");
    assert_eq!(serde_json::to_value(f.settings()).unwrap(), previous);
    assert_eq!(
        f.control.listeners.lock()[0].0.local_addr().unwrap(),
        SocketAddr::from(([127, 0, 0, 1], port))
    );
    assert!(TcpStream::connect(("127.0.0.1", port)).is_ok());
    assert!(f.control.path.is_dir());
    assert_eq!(std::fs::read_dir(f._dir.path()).unwrap().count(), 1);
}

#[test]
fn same_port_address_change_restores_released_socket_on_bind_failure() {
    let f = Fixture::new();
    f.control.config.read().save(&f.control.path).unwrap();
    let saved = std::fs::read(&f.control.path).unwrap();
    let previous = serde_json::to_value(f.settings()).unwrap();
    let port = f.settings().port;
    let occupied = TcpListener::bind("0.0.0.0:0").unwrap();
    let mut settings = f.settings();
    settings.address = "0.0.0.0".parse().unwrap();
    settings.fixed.push(crate::control::FixedPort {
        port: occupied.local_addr().unwrap().port(),
        reader: "Mock B-CAS".into(),
    });
    let error = f.control.apply(settings).unwrap_err();
    assert!(!error.contains("listener rollback failed"), "{error}");
    assert_eq!(serde_json::to_value(f.settings()).unwrap(), previous);
    assert_eq!(std::fs::read(&f.control.path).unwrap(), saved);
    assert_eq!(
        f.control.listeners.lock()[0].0.local_addr().unwrap(),
        SocketAddr::from(([127, 0, 0, 1], port))
    );
    assert!(TcpStream::connect(("127.0.0.1", port)).is_ok());
}

#[test]
fn nonloopback_token_bootstrap_persists_and_reuses_random_secret() {
    let f = Fixture::new();
    f.control.config.write().web_listen = "0.0.0.0:6980".parse().unwrap();
    ensure_token(&f.control).unwrap();
    let token = f.control.config.read().web_token.clone();
    assert_eq!(token.len(), 64);
    assert!(token.bytes().all(|b| b.is_ascii_hexdigit()));
    let mut loaded = Config::default();
    loaded.load_file(&f.control.path).unwrap();
    assert_eq!(loaded.web_token, token);
    let saved = std::fs::read(&f.control.path).unwrap();
    ensure_token(&f.control).unwrap();
    assert_eq!(f.control.config.read().web_token, token);
    assert_eq!(std::fs::read(&f.control.path).unwrap(), saved);
    // Simulate another process starting without an in-memory token.
    f.control.config.write().web_token.clear();
    ensure_token(&f.control).unwrap();
    assert_eq!(f.control.config.read().web_token, token);
    assert_eq!(std::fs::read(&f.control.path).unwrap(), saved);
}

#[test]
fn token_bootstrap_does_not_persist_effective_cli_overrides() {
    let f = Fixture::new();
    let original = Config::default();
    original.save(&f.control.path).unwrap();
    {
        let mut effective = f.control.config.write();
        effective.port = 54321;
        effective.address = "127.0.0.2".parse().unwrap();
        effective.pins = ["temporary B-CAS".into(), "temporary ACAS".into()];
        effective.verbose = true;
        effective.allow = vec![boncasrelay_core::config::cidr("192.0.2.0/24").unwrap()];
        effective.web_listen = "0.0.0.0:16980".parse().unwrap();
        effective.cli_overrides = vec!["--port".into(), "--listen".into(), "--web-listen".into()];
    }
    let effective_settings = serde_json::to_value(f.settings()).unwrap();
    ensure_token(&f.control).unwrap();
    let mut loaded = Config::default();
    loaded.load_file(&f.control.path).unwrap();
    assert_eq!(
        serde_json::to_value(Settings::from(&loaded)).unwrap(),
        serde_json::to_value(Settings::from(&original)).unwrap()
    );
    assert_eq!(loaded.web_listen, original.web_listen);
    assert_eq!(loaded.no_web, original.no_web);
    assert_eq!(loaded.web_token, f.control.config.read().web_token);
    assert_eq!(loaded.web_token.len(), 64);
    assert_eq!(
        serde_json::to_value(f.settings()).unwrap(),
        effective_settings
    );
    assert_eq!(
        f.control.config.read().web_listen,
        "0.0.0.0:16980".parse::<SocketAddr>().unwrap()
    );
}

#[test]
fn local_or_disabled_web_skips_token_generation_and_file_writes() {
    for (address, disabled) in [
        ("127.0.0.1:6980", false),
        ("[::1]:6980", false),
        ("0.0.0.0:6980", true),
    ] {
        let f = Fixture::new();
        {
            let mut config = f.control.config.write();
            config.web_listen = address.parse().unwrap();
            config.no_web = disabled;
        }
        ensure_token(&f.control).unwrap();
        assert!(f.control.config.read().web_token.is_empty());
        assert!(!f.control.path.exists());
        // Existing content must also remain byte-for-byte intact.
        let sentinel = b"deliberately not an INI config";
        std::fs::write(&f.control.path, sentinel).unwrap();
        ensure_token(&f.control).unwrap();
        assert_eq!(std::fs::read(&f.control.path).unwrap(), sentinel);
    }
}

#[test]
fn refresh_api_publishes_snapshot_and_throttles_immediate_repeat() {
    let f = Fixture::new();
    runtime().block_on(async {
        let (status, body) = call(
            f.web(),
            "POST",
            "/api/refresh",
            Value::Null,
            &[("x-boncas-request", "1")],
            "127.0.0.1:12345",
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body, json!({"ok": true}));
        let snapshot = f.control.router.state.snapshot();
        assert_eq!(snapshot.readers.len(), 3);
        assert!(snapshot.classifications.contains_key("Mock B-CAS"));
        assert_eq!(snapshot.routes[0].as_deref(), Some("Mock B-CAS"));
        let (status, body) = call(
            f.web(),
            "POST",
            "/api/refresh",
            Value::Null,
            &[("x-boncas-request", "1")],
            "127.0.0.1:12345",
        )
        .await;
        assert_error(status, &body, StatusCode::TOO_MANY_REQUESTS);
        assert!(body["error"].as_str().unwrap().contains("throttled"));
    });
}

#[test]
fn wildcard_and_ipv6_numeric_hosts_work_with_authenticated_peer() {
    let f = Fixture::new();
    runtime().block_on(async {
        for (listen, host, peer) in [
            ("0.0.0.0:6980", "192.0.2.1:6980", "192.0.2.2:12345"),
            ("[::]:6980", "[::1]:6980", "[::1]:12345"),
            ("[::1]:6980", "[::1]:6980", "[::1]:12345"),
        ] {
            let mut web = f.web();
            web.address = listen.parse().unwrap();
            web.token = "test-token".into();
            let mut request = Request::builder()
                .uri("/api/state")
                .header(header::HOST, host)
                .header(header::AUTHORIZATION, "Bearer test-token")
                .body(Body::empty())
                .unwrap();
            request
                .extensions_mut()
                .insert(ConnectInfo(peer.parse::<SocketAddr>().unwrap()));
            let response = app(web).oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{listen} with {host}");
        }
    });
}
