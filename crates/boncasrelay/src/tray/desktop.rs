//! Native tray UI. This entry point must run on the process main thread.
use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use tao::{
    event::{Event, StartCause},
    event_loop::{ControlFlow, EventLoop},
    platform::run_return::EventLoopExtRunReturn,
};
use tray_icon::{
    menu::{AboutMetadata, Menu, MenuEvent, MenuItem, PredefinedMenuItem},
    Icon, TrayIcon, TrayIconBuilder,
};

fn load_icon(bytes: &[u8]) -> boncasrelay_core::Result<Icon> {
    let image = image::load_from_memory_with_format(bytes, image::ImageFormat::Ico)
        .map_err(|e| format!("tray icon decode: {e}"))?
        .into_rgba8();
    let (width, height) = image.dimensions();
    Icon::from_rgba(image.into_raw(), width, height).map_err(|e| format!("tray icon: {e}"))
}

fn settings_url(control: &crate::control::Control) -> Option<String> {
    let mut address = (*control.web_address.read())?;
    if address.ip().is_unspecified() {
        address.set_ip(match address.ip() {
            IpAddr::V4(_) => IpAddr::V4(Ipv4Addr::LOCALHOST),
            IpAddr::V6(_) => IpAddr::V6(Ipv6Addr::LOCALHOST),
        });
    }
    Some(format!("http://{address}/"))
}

pub fn run(control: Arc<crate::control::Control>) -> boncasrelay_core::Result<()> {
    let idle = load_icon(include_bytes!("../../../../assets/idle.ico"))?;
    let connected = load_icon(include_bytes!("../../../../assets/connected.ico"))?;
    let mut event_loop = EventLoop::new();
    #[cfg(target_os = "macos")]
    {
        use tao::platform::macos::{ActivationPolicy, EventLoopExtMacOS};
        event_loop.set_activation_policy(ActivationPolicy::Accessory);
    }

    let mut tray: Option<TrayIcon> = None;
    let mut count_item: Option<MenuItem> = None;
    let mut settings: Option<MenuItem> = None;
    let mut refresh: Option<MenuItem> = None;
    let mut exit: Option<MenuItem> = None;
    let mut failure = None;
    let refreshing = Arc::new(AtomicBool::new(false));
    let mut refresh_worker: Option<std::thread::JoinHandle<()>> = None;
    let mut previous_clients = None;
    let mut previous_web_available = None;
    let mut next_poll = Instant::now();

    let exit_code = event_loop.run_return(|event, _, flow| {
        *flow = ControlFlow::WaitUntil(next_poll);
        if control.stop.load(Ordering::Acquire) {
            *flow = ControlFlow::Exit;
            return;
        }
        // Cocoa requires creation after the main event loop has actually started.
        if matches!(event, Event::NewEvents(StartCause::Init)) {
            let menu = Menu::new();
            let count = MenuItem::new("接続中のクライアント: 0", false, None);
            let open_settings = MenuItem::new("設定を開く", true, None);
            let refresh_readers = MenuItem::new("判定し直す", true, None);
            let about = PredefinedMenuItem::about(
                Some("バージョン情報"),
                Some(AboutMetadata {
                    name: Some("BonCasRelay".into()),
                    version: Some(env!("CARGO_PKG_VERSION").into()),
                    ..Default::default()
                }),
            );
            let quit = MenuItem::new("終了", true, None);
            let result = menu
                .append_items(&[&count, &open_settings, &refresh_readers, &about, &quit])
                .map_err(|e| format!("tray menu: {e}"))
                .and_then(|()| {
                    TrayIconBuilder::new()
                        .with_menu(Box::new(menu))
                        .with_icon(idle.clone())
                        .with_tooltip("BonCasRelay")
                        .build()
                        .map_err(|e| format!("tray startup: {e}"))
                });
            match result {
                Ok(icon) => {
                    control
                        .router
                        .state
                        .log("Tray started (blue idle / red connected)");
                    tray = Some(icon);
                    count_item = Some(count);
                    settings = Some(open_settings);
                    refresh = Some(refresh_readers);
                    exit = Some(quit);
                }
                Err(error) => {
                    failure = Some(error);
                    *flow = ControlFlow::Exit;
                    return;
                }
            }
        }
        if !matches!(event, Event::NewEvents(_)) {
            return;
        }
        next_poll = Instant::now() + Duration::from_millis(250);
        *flow = ControlFlow::WaitUntil(next_poll);
        while let Ok(event) = MenuEvent::receiver().try_recv() {
            if settings.as_ref().is_some_and(|item| item.id() == &event.id) {
                if let Some(url) = settings_url(&control) {
                    if let Err(error) = open::that_detached(url) {
                        eprintln!("設定画面を開けません: {error}");
                    }
                } else {
                    eprintln!("設定画面は利用できません: Web UI が無効、または起動していません");
                }
            } else if refresh.as_ref().is_some_and(|item| item.id() == &event.id) {
                if !refreshing.swap(true, Ordering::AcqRel) {
                    let router = Arc::clone(&control.router);
                    let busy = Arc::clone(&refreshing);
                    if let Some(worker) = refresh_worker.take() {
                        let _ = worker.join();
                    }
                    match std::thread::Builder::new()
                        .name("tray-reader-refresh".into())
                        .spawn(move || {
                            if let Err(error) = router.refresh() {
                                router
                                    .state
                                    .log(format!("カードリーダー再判定に失敗しました: {error}"));
                            }
                            busy.store(false, Ordering::Release);
                        }) {
                        Ok(worker) => refresh_worker = Some(worker),
                        Err(error) => {
                            refreshing.store(false, Ordering::Release);
                            control
                                .router
                                .state
                                .log(format!("再判定処理を開始できません: {error}"));
                        }
                    }
                }
            } else if exit.as_ref().is_some_and(|item| item.id() == &event.id) {
                control.stop.store(true, Ordering::Release);
                *flow = ControlFlow::Exit;
                return;
            }
        }
        if let Some(item) = &refresh {
            item.set_enabled(!refreshing.load(Ordering::Acquire));
        }
        let web_available = control.web_address.read().is_some();
        if previous_web_available != Some(web_available) {
            if let Some(item) = &settings {
                item.set_enabled(web_available);
            }
            if !web_available {
                eprintln!("設定画面は利用できません: Web UI が無効、または起動していません");
            }
            previous_web_available = Some(web_available);
        }
        let clients = control.router.state.snapshot().clients.len();
        if previous_clients != Some(clients) {
            if let Some(item) = &count_item {
                item.set_text(format!("接続中のクライアント: {clients}"));
            }
            if let Some(icon) = &tray {
                let image = if clients == 0 { &idle } else { &connected };
                if let Err(error) = icon.set_icon(Some(image.clone())) {
                    eprintln!("トレイアイコンを更新できません: {error}");
                }
                if let Err(error) =
                    icon.set_tooltip(Some(format!("BonCasRelay — 接続数: {clients}")))
                {
                    eprintln!("トレイの接続数を更新できません: {error}");
                }
            }
            previous_clients = Some(clients);
        }
    });
    // Return to the caller so its server worker can shut down and join cleanly.
    control.stop.store(true, Ordering::Release);
    drop(tray);
    if let Some(worker) = refresh_worker {
        let _ = worker.join();
    }
    control.router.state.log("Tray stopped");
    if let Some(error) = failure {
        Err(error)
    } else if exit_code != 0 {
        Err(format!("tray event loop exited with code {exit_code}"))
    } else {
        Ok(())
    }
}
