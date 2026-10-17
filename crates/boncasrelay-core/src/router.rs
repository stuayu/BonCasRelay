use crate::{
    backend::{Backend, Card},
    config::Config,
    state::State,
    *,
};
use parking_lot::{Mutex, RwLock};
use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
pub struct Reader {
    pub name: String,
    pub card: Arc<Mutex<Option<Box<dyn Card>>>>,
    needs_probe: AtomicBool,
}
#[derive(Default)]
struct Fixed {
    reader: Option<Arc<Reader>>,
    time: Option<Instant>,
    invalid: bool,
    revision: u64,
}
#[derive(Default)]
struct Cache {
    states: BTreeMap<String, Arc<Reader>>,
    results: BTreeMap<String, Probe>,
    routes: [Option<Arc<Reader>>; 2],
    time: Option<Instant>,
    invalid: bool,
    revision: u64,
    fixed: BTreeMap<String, Fixed>,
}
impl Cache {
    fn reader(&mut self, n: &str) -> Arc<Reader> {
        self.states
            .entry(n.into())
            .or_insert_with(|| {
                Arc::new(Reader {
                    name: n.into(),
                    card: Arc::new(Mutex::new(None)),
                    needs_probe: AtomicBool::new(true),
                })
            })
            .clone()
    }
    fn due(&self, k: Kind, interval: Duration) -> bool {
        self.time.is_none_or(|t| {
            (self.invalid || self.routes[k as usize].is_none()) && t.elapsed() >= interval
        })
    }
}
pub struct Router {
    pub backend: Arc<dyn Backend>,
    pins: RwLock<[String; 2]>,
    cache: Mutex<Cache>,
    detection: Mutex<()>,
    resolution: Mutex<()>,
    interval: Duration,
    pub state: State,
    pub stop: Arc<AtomicBool>,
}
pub fn match_reader(names: &[String], selector: &str) -> Result<Option<String>> {
    if names.iter().any(|n| n == selector) {
        return Ok(Some(selector.into()));
    }
    let mut matches = names.iter().filter(|n| n.contains(selector));
    let name = matches.next().cloned();
    if matches.next().is_some() {
        return Err(format!("Ambiguous reader selector: {selector}"));
    }
    Ok(name)
}
impl Router {
    pub fn new(
        backend: Arc<dyn Backend>,
        config: Config,
        state: State,
        stop: Arc<AtomicBool>,
    ) -> Result<Self> {
        let mut interval = Duration::from_secs(5);
        if config.mock {
            if let Ok(s) = std::env::var("BONCASRELAY_MOCK_DETECT_INTERVAL_MS") {
                let ms = s
                    .parse::<u64>()
                    .ok()
                    .filter(|n| (1..=60000).contains(n) && s.bytes().all(|b| b.is_ascii_digit()))
                    .ok_or("Invalid mock detection interval")?;
                interval = Duration::from_millis(ms);
            }
        }
        Ok(Self {
            backend,
            pins: RwLock::new(config.pins),
            cache: Mutex::new(Cache::default()),
            detection: Mutex::new(()),
            resolution: Mutex::new(()),
            interval,
            state,
            stop,
        })
    }
    pub fn select(&self, k: Kind, fixed: &str) -> Result<Arc<Reader>> {
        if !fixed.is_empty() {
            return self.fixed_select(fixed);
        }
        let (due, cached) = {
            let c = self.cache.lock();
            (c.due(k, self.interval), c.routes[k as usize].is_some())
        };
        if due || !cached {
            let guard = if cached {
                self.detection.try_lock()
            } else {
                Some(self.detection.lock())
            };
            if guard.is_some() && self.cache.lock().due(k, self.interval) {
                if let Err(e) = self.detect(k) {
                    self.state.log(format!("Detection failed: {e}"));
                    self.cache.lock().invalid = true;
                }
            }
        }
        self.cache.lock().routes[k as usize]
            .clone()
            .ok_or_else(|| "No suitable card reader (detection pending or unavailable)".into())
    }
    fn detect(&self, k: Kind) -> Result<()> {
        let configured_pins = self.pins.read().clone();
        let (revision, force, mut results) = {
            let mut c = self.cache.lock();
            c.time = Some(Instant::now());
            (
                c.revision,
                c.routes[k as usize].is_none() && configured_pins[k as usize].is_empty(),
                c.results.clone(),
            )
        };
        let names = self.backend.readers()?;
        let readers: Vec<_> = {
            let mut c = self.cache.lock();
            names.iter().map(|n| c.reader(n)).collect()
        };
        let mut pins = [None, None];
        for (i, pin) in configured_pins.iter().enumerate() {
            if !pin.is_empty() {
                match match_reader(&names, pin) {
                    Ok(name) => pins[i] = names.iter().position(|n| Some(n) == name.as_ref()),
                    Err(e) => self.state.log(e),
                }
            }
        }
        let mut pending = false;
        let mut probes = vec![];
        for (i, r) in readers.iter().enumerate() {
            if self.stop.load(Ordering::Relaxed) {
                return Err("Server stopping".into());
            }
            if pins.contains(&Some(i)) {
                probes.push(Probe::default());
                continue;
            }
            let mut result = results.get(&r.name).copied().unwrap_or_default();
            if force || r.needs_probe.load(Ordering::Relaxed) || !result.openable {
                if let Some(mut card) = r.card.try_lock_for(Duration::from_millis(100)) {
                    if force {
                        *card = None;
                    }
                    let probe = (|| -> Result<Probe> {
                        if card.is_none() {
                            *card = Some(self.backend.open(&r.name)?);
                        }
                        let mut session = card.as_mut().ok_or("Card unavailable")?.session()?;
                        let mut random = [0; 8];
                        getrandom::getrandom(&mut random).map_err(|e| e.to_string())?;
                        let a = self.backend.probe(&mut *session, &auth_probe(random))?;
                        let acas = acas_response(&a);
                        let bcas = if acas {
                            false
                        } else {
                            bcas_response(&self.backend.probe(&mut *session, &INIT_PROBE)?)
                        };
                        Ok(Probe {
                            openable: true,
                            bcas,
                            acas,
                        })
                    })();
                    match probe {
                        Ok(p) => {
                            result = p;
                            r.needs_probe.store(false, Ordering::Relaxed);
                            self.state.log(format!(
                                "Reader classified: {} {}",
                                r.name,
                                if p.acas {
                                    "ACAS"
                                } else if p.bcas {
                                    "B-CAS"
                                } else {
                                    "other"
                                }
                            ));
                        }
                        Err(e) => {
                            self.state.log(format!("{}: {e}", r.name));
                            *card = None;
                            r.needs_probe.store(true, Ordering::Relaxed);
                            pending = true;
                            result = Probe::default();
                        }
                    }
                } else {
                    r.needs_probe.store(true, Ordering::Relaxed);
                    pending = true;
                }
            }
            results.insert(r.name.clone(), result);
            probes.push(result);
        }
        let mut c = self.cache.lock();
        c.invalid = pending || c.revision != revision;
        c.results = results;
        for (i, kind) in [Kind::Bcas, Kind::Acas].iter().enumerate() {
            let selected = if !configured_pins[i].is_empty() && pins[i].is_none() {
                None
            } else {
                select_reader(*kind, &probes, pins[i]).map(|j| readers[j].clone())
            };
            if c.routes[i].as_ref().map(|r| &r.name) != selected.as_ref().map(|r| &r.name) {
                self.state.log(format!(
                    "Route {}: {}",
                    if i == 0 { "B-CAS" } else { "ACAS" },
                    selected.as_ref().map_or("none", |r| r.name.as_str())
                ));
            }
            c.routes[i] = selected;
        }
        if self.state.snapshot().readers != names {
            for n in &names {
                self.state.log(format!("Reader discovered: {n}"));
            }
        }
        self.state.update(|s| {
            s.readers = names;
            s.classifications = c.results.clone();
            s.routes = c
                .routes
                .each_ref()
                .map(|r| r.as_ref().map(|r| r.name.clone()));
        });
        Ok(())
    }
    fn fixed_select(&self, selector: &str) -> Result<Arc<Reader>> {
        let due = |f: &Fixed| {
            f.time
                .is_none_or(|t| (f.invalid || f.reader.is_none()) && t.elapsed() >= self.interval)
        };
        let (refresh, cached) = {
            let mut c = self.cache.lock();
            let f = c.fixed.entry(selector.into()).or_default();
            (due(f), f.reader.is_some())
        };
        if refresh || !cached {
            let guard = if cached {
                self.resolution.try_lock()
            } else {
                Some(self.resolution.lock())
            };
            if guard.is_some() {
                let rev = {
                    let mut c = self.cache.lock();
                    let f = c.fixed.entry(selector.into()).or_default();
                    if due(f) {
                        f.time = Some(Instant::now());
                        Some(f.revision)
                    } else {
                        None
                    }
                };
                if let Some(rev) = rev {
                    let resolved = self.backend.readers().and_then(|names| {
                        self.state.update(|s| s.readers = names.clone());
                        match_reader(&names, selector)
                    });
                    let mut c = self.cache.lock();
                    let (reader, error) = match resolved {
                        Ok(n) => (n.map(|n| c.reader(&n)), false),
                        Err(e) => {
                            self.state
                                .log(format!("Fixed reader resolution failed: {e}"));
                            (None, true)
                        }
                    };
                    let f = c.fixed.entry(selector.into()).or_default();
                    f.reader = reader;
                    f.invalid = error || f.revision != rev;
                    self.state.update(|s| {
                        s.fixed_routes
                            .insert(selector.into(), f.reader.as_ref().map(|r| r.name.clone()));
                    });
                }
            }
        }
        self.cache
            .lock()
            .fixed
            .get(selector)
            .and_then(|f| f.reader.clone())
            .ok_or_else(|| format!("Fixed reader unavailable: {selector}"))
    }
    /// Replace automatic reader pins without interrupting an active transaction.
    /// A future settings controller can call this after validating/saving Config.
    /// Listener address, allow rules and ports belong to the server lifecycle.
    pub fn replace_pins(&self, pins: [String; 2]) {
        let _detection = self.detection.lock();
        *self.pins.write() = pins;
        let mut cache = self.cache.lock();
        cache.routes = Default::default();
        cache.results.clear();
        cache.time = None;
        cache.invalid = true;
        cache.revision += 1;
        for reader in cache.states.values() {
            reader.needs_probe.store(true, Ordering::Relaxed);
        }
        self.state.update(|s| {
            s.routes = Default::default();
            s.classifications.clear();
        });
    }
    /// Explicit refresh respects the same minimum interval as command detection.
    pub fn refresh(&self) -> Result<()> {
        let _guard = self.detection.lock();
        {
            let mut c = self.cache.lock();
            if c.time.is_some_and(|t| t.elapsed() < self.interval) {
                return Err("Detection throttled; retry after the minimum interval".into());
            }
            for reader in c.states.values() {
                reader.needs_probe.store(true, Ordering::Relaxed);
            }
            c.invalid = true;
        }
        self.detect(Kind::Bcas)
    }
    pub fn invalidate(&self, r: &Reader) {
        r.needs_probe.store(true, Ordering::Relaxed);
        let mut c = self.cache.lock();
        c.revision += 1;
        c.invalid = true;
        for f in c.fixed.values_mut() {
            if f.reader
                .as_ref()
                .is_some_and(|reader| reader.name == r.name)
            {
                f.invalid = true;
                f.revision += 1;
            }
        }
    }
}
