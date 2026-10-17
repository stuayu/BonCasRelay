use boncasrelay_core::{
    config::{cidr, Config},
    router::Router,
    Result,
};
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use std::{
    net::{SocketAddr, TcpListener},
    path::PathBuf,
    sync::{atomic::AtomicBool, Arc},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FixedPort {
    pub port: u16,
    pub reader: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    pub address: std::net::IpAddr,
    pub port: u16,
    pub allow: Vec<String>,
    pub pins: [String; 2],
    pub fixed: Vec<FixedPort>,
    pub verbose: bool,
}
impl From<&Config> for Settings {
    fn from(c: &Config) -> Self {
        Self {
            address: c.address,
            port: c.port,
            allow: c.allow.iter().map(ToString::to_string).collect(),
            pins: c.pins.clone(),
            fixed: c
                .fixed
                .iter()
                .map(|(p, n)| FixedPort {
                    port: *p,
                    reader: n.clone(),
                })
                .collect(),
            verbose: c.verbose,
        }
    }
}
pub struct Control {
    pub router: Arc<Router>,
    pub config: RwLock<Config>,
    pub stop: Arc<AtomicBool>,
    pub web_address: RwLock<Option<SocketAddr>>,
    pub listeners: Mutex<Vec<(TcpListener, String)>>,
    updates: Mutex<()>,
    pub path: PathBuf,
}
impl Control {
    pub fn new(c: Config, router: Arc<Router>) -> Result<Arc<Self>> {
        let path = c
            .config_path
            .clone()
            .map(Ok)
            .unwrap_or_else(Config::standard_path)?;
        let listeners = Self::stage(&c, &[])?;
        Ok(Arc::new(Self {
            stop: router.stop.clone(),
            router,
            config: RwLock::new(c),
            web_address: RwLock::new(None),
            listeners: Mutex::new(listeners),
            updates: Mutex::new(()),
            path,
        }))
    }
    fn stage(c: &Config, old: &[(TcpListener, String)]) -> Result<Vec<(TcpListener, String)>> {
        std::iter::once((c.port, String::new()))
            .chain(c.fixed.iter().map(|(p, n)| (*p, n.clone())))
            .map(|(port, name)| {
                let addr = SocketAddr::new(c.address, port);
                let listener = if let Some((listener, _)) =
                    old.iter().find(|(l, _)| l.local_addr().ok() == Some(addr))
                {
                    listener.try_clone().map_err(|e| e.to_string())?
                } else {
                    super::listen(c, port)?
                };
                Ok((listener, name))
            })
            .collect()
    }
    pub fn apply(&self, settings: Settings) -> Result<()> {
        let _update = self.updates.lock();
        let previous = self.config.read().clone();
        let mut next = previous.clone();
        next.address = settings.address;
        next.port = settings.port;
        next.pins = settings.pins;
        next.verbose = settings.verbose;
        next.allow = settings
            .allow
            .iter()
            .map(|s| cidr(s))
            .collect::<Result<_>>()?;
        next.fixed.clear();
        for f in settings.fixed {
            if next.fixed.insert(f.port, f.reader).is_some() {
                return Err("Duplicate fixed port".into());
            }
        }
        next.validate()?;
        let mut listeners = self.listeners.lock();
        // Accepted streams remain independent of listening sockets. Wildcard IP
        // changes require releasing overlapping listeners before binding anew.
        let wanted: Vec<_> = std::iter::once(next.port)
            .chain(next.fixed.keys().copied())
            .collect();
        let mut released = Vec::new();
        let mut index = 0;
        while index < listeners.len() {
            let address = listeners[index].0.local_addr().map_err(|e| e.to_string())?;
            let overlap = address.ip() != next.address
                && wanted.contains(&address.port())
                && address.ip().is_ipv4() == next.address.is_ipv4()
                && (address.ip().is_unspecified() || next.address.is_unspecified());
            if overlap {
                let (socket, name) = listeners.remove(index);
                released.push((address.port(), name));
                drop(socket);
            } else {
                index += 1;
            }
        }
        let restore = |listeners: &mut Vec<(TcpListener, String)>| -> Result<()> {
            for (port, name) in &released {
                listeners.push((super::listen(&previous, *port)?, name.clone()));
            }
            Ok(())
        };
        let staged = match Self::stage(&next, &listeners) {
            Ok(staged) => staged,
            Err(error) => {
                restore(&mut listeners)
                    .map_err(|rollback| format!("{error}; listener rollback failed: {rollback}"))?;
                return Err(error);
            }
        };
        if let Err(error) = next.save(&self.path) {
            drop(staged);
            restore(&mut listeners)
                .map_err(|rollback| format!("{error}; listener rollback failed: {rollback}"))?;
            return Err(error);
        }
        if next.pins != previous.pins {
            self.router.replace_pins(next.pins.clone());
        }
        *self.config.write() = next;
        *listeners = staged;
        self.router
            .state
            .log("Settings saved and applied; existing clients preserved");
        Ok(())
    }
}
