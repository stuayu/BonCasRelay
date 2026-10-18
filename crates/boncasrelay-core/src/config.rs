use crate::Result;
use ipnet::IpNet;
use std::{
    collections::BTreeMap,
    net::IpAddr,
    path::{Path, PathBuf},
};
#[derive(Clone, Debug)]
pub struct Config {
    pub address: IpAddr,
    pub port: u16,
    pub pins: [String; 2],
    pub fixed: BTreeMap<u16, String>,
    pub allow: Vec<IpNet>,
    pub verbose: bool,
    pub mock: bool,
    pub list: bool,
    pub help: bool,
    pub web_listen: std::net::SocketAddr,
    pub no_web: bool,
    pub tray: bool,
    pub config_path: Option<PathBuf>,
    pub cli_overrides: Vec<String>,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            address: IpAddr::from([0, 0, 0, 0]),
            port: 6900,
            pins: Default::default(),
            fixed: Default::default(),
            allow: vec![],
            verbose: false,
            mock: false,
            list: false,
            help: false,
            web_listen: "127.0.0.1:6980".parse().expect("constant socket address"),
            no_web: false,
            tray: false,
            config_path: None,
            cli_overrides: vec![],
        }
    }
}
fn port(s: &str) -> Result<u16> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("Invalid port: {s}"));
    }
    s.parse::<u16>()
        .ok()
        .filter(|p| *p != 0)
        .ok_or_else(|| "Port out of range".into())
}
pub fn cidr(s: &str) -> Result<IpNet> {
    if s.contains('/') {
        s.parse().map_err(|e| format!("Invalid CIDR: {e}"))
    } else {
        s.parse::<IpAddr>()
            .map(IpNet::from)
            .map_err(|e| format!("Invalid address: {e}"))
    }
}
impl Config {
    pub fn load_file(&mut self, path: &Path) -> Result<()> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("Cannot read config: {}: {e}", path.display()))?;
        self.apply_ini(&text)
    }
    pub fn apply_ini(&mut self, text: &str) -> Result<()> {
        let mut section = "";
        for line in text.trim_start_matches('\u{feff}').lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with(['#', ';']) {
                continue;
            }
            if line.starts_with('[') && line.ends_with(']') {
                section = &line[1..line.len() - 1];
                continue;
            }
            let (key, value) = line
                .split_once('=')
                .ok_or_else(|| format!("Invalid INI line: {line}"))?;
            let (key, value) = (key.trim(), value.trim());
            match (section, key) {
                ("Web", "Listen") => {
                    self.web_listen = value.parse().map_err(|_| "Invalid web listen address")?
                }
                ("Web", "Token") => {}
                ("Web", "Enabled") => {
                    self.no_web = match value {
                        "true" | "1" => false,
                        "false" | "0" => true,
                        _ => return Err("Invalid Web.Enabled".into()),
                    }
                }
                ("General", "ServerPort") => self.port = port(value)?,
                ("General", "ListenAddress") => {
                    self.address = value.parse().map_err(|_| "Invalid listen address")?
                }
                ("General", "Verbose") => {
                    self.verbose = match value {
                        "true" | "1" => true,
                        "false" | "0" => false,
                        _ => return Err("Expected true/false or 1/0".into()),
                    }
                }
                ("General", "Allow") => self.allow.push(cidr(value)?),
                ("CardReader", "BCAS") => self.pins[0] = value.into(),
                ("CardReader", "ACAS") => self.pins[1] = value.into(),
                ("CardPort", _) => {
                    self.fixed.insert(port(key)?, value.into());
                }
                _ => return Err(format!("Unknown config key: {section}.{key}")),
            }
        }
        Ok(())
    }
    pub fn parse(args: &[String]) -> Result<Self> {
        let mut c = Self::default();
        if args.iter().any(|a| a == "--tray") && !args.iter().any(|a| a == "--config") {
            let path = Self::standard_path()?;
            if path.exists() {
                c.load_file(&path)?;
            }
            c.config_path = Some(path);
        }
        for (i, a) in args.iter().enumerate() {
            if a == "--config" {
                let path = Path::new(args.get(i + 1).ok_or("Missing config path")?);
                c.load_file(path)?;
                c.config_path = Some(path.to_owned());
            }
        }
        let (mut allow_seen, mut fixed_seen) = (false, false);
        let original_args = args;
        let mut args = args.iter();
        while let Some(a) = args.next() {
            match a.as_str() {
                "--tray" => c.tray = true,
                "--no-web" => c.no_web = true,
                "--mock" => c.mock = true,
                "--verbose" => c.verbose = true,
                "--list-readers" => c.list = true,
                "--help" | "-h" => c.help = true,
                "--config" | "--listen" | "--port" | "--bcas-reader" | "--acas-reader"
                | "--allow" | "--reader-port" | "--web-listen" => {
                    let v = args.next().ok_or_else(|| format!("Missing value: {a}"))?;
                    match a.as_str() {
                        "--config" => {}
                        "--web-listen" => {
                            c.web_listen = v.parse().map_err(|_| "Invalid web listen address")?
                        }
                        "--listen" => {
                            c.address = v.parse().map_err(|_| "Invalid listen address")?
                        }
                        "--port" => c.port = port(v)?,
                        "--bcas-reader" => c.pins[0] = v.clone(),
                        "--acas-reader" => c.pins[1] = v.clone(),
                        "--allow" => {
                            if !allow_seen {
                                c.allow.clear();
                                allow_seen = true;
                            }
                            c.allow.push(cidr(v)?);
                        }
                        _ => {
                            if !fixed_seen {
                                c.fixed.clear();
                                fixed_seen = true;
                            }
                            let (name, p) = v
                                .rsplit_once('=')
                                .filter(|(n, _)| !n.is_empty())
                                .ok_or("Expected READER=PORT")?;
                            if c.fixed.insert(port(p)?, name.into()).is_some() {
                                return Err("Duplicate reader port".into());
                            }
                        }
                    }
                }
                _ => return Err(format!("Unknown option: {a}")),
            }
        }
        c.cli_overrides = original_args
            .iter()
            .filter(|a| {
                matches!(
                    a.as_str(),
                    "--listen"
                        | "--port"
                        | "--allow"
                        | "--reader-port"
                        | "--bcas-reader"
                        | "--acas-reader"
                        | "--verbose"
                        | "--web-listen"
                        | "--no-web"
                )
            })
            .cloned()
            .collect();
        c.validate()?;
        Ok(c)
    }
    pub fn standard_path() -> Result<PathBuf> {
        #[cfg(target_os = "windows")]
        let base = std::env::var_os("APPDATA").map(PathBuf::from);
        #[cfg(target_os = "macos")]
        let base =
            std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Application Support"));
        #[cfg(not(any(target_os = "windows", target_os = "macos")))]
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")));
        let base = base.ok_or("Cannot locate user config directory")?;
        #[cfg(target_os = "linux")]
        let directory = "boncasrelay";
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        let directory = "BonCasRelay";
        Ok(base.join(directory).join("boncasrelay.ini"))
    }
    pub fn save(&self, path: &Path) -> Result<()> {
        use std::io::Write;
        self.validate()?;
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        let mut file = tempfile::NamedTempFile::new_in(parent).map_err(|e| e.to_string())?;
        write!(
            file,
            "[General]\nListenAddress={}\nServerPort={}\nVerbose={}\n",
            self.address, self.port, self.verbose
        )
        .map_err(|e| e.to_string())?;
        for net in &self.allow {
            writeln!(file, "Allow={net}").map_err(|e| e.to_string())?;
        }
        write!(
            file,
            "[CardReader]\nBCAS={}\nACAS={}\n[CardPort]\n",
            self.pins[0], self.pins[1]
        )
        .map_err(|e| e.to_string())?;
        for (port, name) in &self.fixed {
            writeln!(file, "{port}={name}").map_err(|e| e.to_string())?;
        }
        write!(
            file,
            "[Web]\nListen={}\nEnabled={}\n",
            self.web_listen, !self.no_web
        )
        .map_err(|e| e.to_string())?;
        file.as_file().sync_all().map_err(|e| e.to_string())?;
        file.persist(path).map_err(|e| e.to_string())?;
        Ok(())
    }
    pub fn validate(&self) -> Result<()> {
        if self
            .pins
            .iter()
            .chain(self.fixed.values())
            .any(|s| s.contains(['\r', '\n']) || s.trim() != s)
        {
            return Err("INI values must be single lines without surrounding whitespace".into());
        }
        if self.web_listen.port() == 0 {
            return Err("Web port must be nonzero".into());
        }
        if self.port == 0 || self.fixed.contains_key(&self.port) {
            return Err("Reader port collides with automatic port or port is zero".into());
        }
        if self.fixed.iter().any(|(p, n)| *p == 0 || n.is_empty()) {
            return Err("Invalid fixed reader".into());
        }
        Ok(())
    }
}
