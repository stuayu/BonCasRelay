//! Linux StatusNotifierItem tray. D-Bus service runs on ksni's worker thread.
use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::Duration,
};

use ksni::{blocking::TrayMethods, menu::StandardItem, Icon, ToolTip};

fn load_icon(bytes: &[u8]) -> boncasrelay_core::Result<Icon> {
    let image = image::load_from_memory_with_format(bytes, image::ImageFormat::Png)
        .map_err(|e| format!("tray icon decode: {e}"))?
        .into_rgba8();
    let (width, height) = image.dimensions();
    let data = image
        .pixels()
        .flat_map(|pixel| [pixel[3], pixel[0], pixel[1], pixel[2]])
        .collect();
    Ok(Icon {
        width: width as i32,
        height: height as i32,
        data,
    })
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

struct LinuxTray {
    control: Arc<crate::control::Control>,
    idle: Icon,
    connected: Icon,
    clients: usize,
    refreshing: Arc<AtomicBool>,
}

impl ksni::Tray for LinuxTray {
    fn id(&self) -> String {
        env!("CARGO_PKG_NAME").into()
    }

    fn title(&self) -> String {
        "BonCasRelay".into()
    }

    fn icon_pixmap(&self) -> Vec<Icon> {
        vec![if self.clients == 0 {
            self.idle.clone()
        } else {
            self.connected.clone()
        }]
    }

    fn tool_tip(&self) -> ToolTip {
        ToolTip {
            title: "BonCasRelay".into(),
            description: format!("接続数: {}", self.clients),
            ..Default::default()
        }
    }

    fn watcher_offline(&self, reason: ksni::OfflineReason) -> bool {
        let message = format!("StatusNotifierWatcher が利用できません: {reason:?}");
        eprintln!("{message}。サーバーは継続します");
        self.control.router.state.log(message);
        true
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        let count = StandardItem {
            label: format!("接続中のクライアント: {}", self.clients),
            enabled: false,
            ..Default::default()
        };
        let settings_enabled = self.control.web_address.read().is_some();
        let settings = StandardItem {
            label: "設定を開く".into(),
            enabled: settings_enabled,
            activate: Box::new(|tray| {
                if let Some(url) = settings_url(&tray.control) {
                    if let Err(error) = open::that_detached(url) {
                        eprintln!("設定画面を開けません: {error}");
                    }
                }
            }),
            ..Default::default()
        };
        let refreshing = Arc::clone(&self.refreshing);
        let control = Arc::clone(&self.control);
        let refresh = StandardItem {
            label: "判定し直す".into(),
            enabled: !self.refreshing.load(Ordering::Acquire),
            activate: Box::new(move |_| {
                if !refreshing.swap(true, Ordering::AcqRel) {
                    let router = Arc::clone(&control.router);
                    let busy = Arc::clone(&refreshing);
                    let spawn = thread::Builder::new()
                        .name("tray-reader-refresh".into())
                        .spawn(move || {
                            if let Err(error) = router.refresh() {
                                router
                                    .state
                                    .log(format!("カードリーダー再判定に失敗しました: {error}"));
                            }
                            busy.store(false, Ordering::Release);
                        });
                    if let Err(error) = spawn {
                        refreshing.store(false, Ordering::Release);
                        control
                            .router
                            .state
                            .log(format!("再判定処理を開始できません: {error}"));
                    }
                }
            }),
            ..Default::default()
        };
        let version = StandardItem {
            label: format!("バージョン情報: BonCasRelay {}", env!("CARGO_PKG_VERSION")),
            enabled: false,
            ..Default::default()
        };
        let control = Arc::clone(&self.control);
        let quit = StandardItem {
            label: "終了".into(),
            activate: Box::new(move |_| control.stop.store(true, Ordering::Release)),
            ..Default::default()
        };
        vec![
            count.into(),
            settings.into(),
            refresh.into(),
            version.into(),
            quit.into(),
        ]
    }
}

pub fn run(control: Arc<crate::control::Control>) -> boncasrelay_core::Result<()> {
    let idle = load_icon(include_bytes!("../../../../assets/idle.png"))?;
    let connected = load_icon(include_bytes!("../../../../assets/connected.png"))?;
    let refreshing = Arc::new(AtomicBool::new(false));
    let tray = LinuxTray {
        control: Arc::clone(&control),
        idle,
        connected,
        clients: 0,
        refreshing: Arc::clone(&refreshing),
    };
    let handle = match tray.spawn() {
        Ok(handle) => {
            control
                .router
                .state
                .log("Linux StatusNotifierItem tray started");
            Some(handle)
        }
        Err(error) => {
            eprintln!("システムトレイを利用できません。サーバーは継続します: {error}");
            control
                .router
                .state
                .log(format!("システムトレイを利用できません: {error}"));
            None
        }
    };

    let mut previous_clients = None;
    while !control.stop.load(Ordering::Acquire) {
        if let Some(handle) = &handle {
            let clients = control.router.state.snapshot().clients.len();
            if previous_clients != Some(clients) {
                let _ = handle.update(|tray| tray.clients = clients);
                previous_clients = Some(clients);
            }
        }
        thread::sleep(Duration::from_millis(250));
    }
    if let Some(handle) = handle {
        handle.shutdown().wait();
    }
    control.stop.store(true, Ordering::Release);
    control.router.state.log("Tray stopped");
    Ok(())
}
