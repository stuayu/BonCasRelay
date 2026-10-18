use super::*;
use axum::{
    body::{to_bytes, Body},
    extract::ConnectInfo,
};
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
    let host = extra
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("host"))
        .map(|(_, value)| (*value).to_owned())
        .unwrap_or_else(|| web.address.to_string());
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::HOST, host)
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
    call(f.web(), "PUT", "/api/config", body, &[], "127.0.0.1:12345").await
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
fn arbitrary_origin_and_host_allow_state_update_and_refresh_without_extra_headers() {
    let f = Fixture::new();
    runtime().block_on(async {
        let hostile = [
            ("host", "untrusted.example:9999"),
            ("origin", "https://elsewhere.example"),
            ("sec-fetch-site", "cross-site"),
        ];
        let (status, body) = call(
            f.web(),
            "GET",
            "/api/state",
            Value::Null,
            &hostile,
            "192.0.2.2:12345",
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");

        let settings = serde_json::to_value(f.settings()).unwrap();
        let (status, body) = call(
            f.web(),
            "PUT",
            "/api/config",
            settings,
            &[("host", "other.example"), ("origin", "null")],
            "192.0.2.2:12345",
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");

        let (status, body) = call(
            f.web(),
            "POST",
            "/api/refresh",
            Value::Null,
            &[
                ("host", "anything.invalid"),
                ("origin", "https://cross-site.example"),
            ],
            "192.0.2.2:12345",
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
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
fn legacy_ini_token_is_ignored_and_not_saved() {
    let f = Fixture::new();
    std::fs::write(
        &f.control.path,
        "[Web]\nListen=0.0.0.0:6980\nEnabled=true\nToken=legacy-secret\n",
    )
    .unwrap();
    let mut loaded = Config::default();
    loaded.load_file(&f.control.path).unwrap();
    assert_eq!(loaded.web_listen, "0.0.0.0:6980".parse().unwrap());
    loaded.save(&f.control.path).unwrap();
    let saved = std::fs::read_to_string(&f.control.path).unwrap();
    assert!(!saved.contains("Token"));
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
            &[],
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
            &[],
            "127.0.0.1:12345",
        )
        .await;
        assert_error(status, &body, StatusCode::TOO_MANY_REQUESTS);
        assert!(body["error"].as_str().unwrap().contains("throttled"));
    });
}

#[test]
fn wildcard_and_ipv6_numeric_hosts_work_without_authentication() {
    let f = Fixture::new();
    runtime().block_on(async {
        for (listen, host, peer) in [
            ("0.0.0.0:6980", "192.0.2.1:6980", "192.0.2.2:12345"),
            ("[::]:6980", "[::1]:6980", "[::1]:12345"),
            ("[::1]:6980", "[::1]:6980", "[::1]:12345"),
        ] {
            let mut web = f.web();
            web.address = listen.parse().unwrap();
            let mut request = Request::builder()
                .uri("/api/state")
                .header(header::HOST, host)
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
