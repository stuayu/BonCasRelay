use crate::{is_auth, state::State, Result};
use std::{
    ffi::CString,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
pub trait Session {
    fn transmit(&mut self, apdu: &[u8]) -> Result<Vec<u8>>;
}
pub trait Card: Send {
    fn session(&mut self) -> Result<Box<dyn Session + '_>>;
}
pub trait Backend: Send + Sync {
    fn readers(&self) -> Result<Vec<String>>;
    fn open(&self, name: &str) -> Result<Box<dyn Card>>;
    fn probe(&self, session: &mut dyn Session, apdu: &[u8]) -> Result<Vec<u8>> {
        session.transmit(apdu)
    }
}
pub struct NativeBackend;
struct NativeCard(Option<pcsc::Card>);
impl Drop for NativeCard {
    fn drop(&mut self) {
        // pcsc::Card defaults to ResetCard on drop; match the C++ LeaveCard policy.
        if let Some(card) = self.0.take() {
            let _ = card.disconnect(pcsc::Disposition::LeaveCard);
        }
    }
}
struct NativeSession<'a>(pcsc::Transaction<'a>);
impl Session for NativeSession<'_> {
    fn transmit(&mut self, apdu: &[u8]) -> Result<Vec<u8>> {
        let mut reply = [0; 4096];
        self.0
            .transmit(apdu, &mut reply)
            .map(|r| r.to_vec())
            .map_err(|e| format!("PC/SC error: {e}"))
    }
}
impl Card for NativeCard {
    fn session(&mut self) -> Result<Box<dyn Session + '_>> {
        self.0
            .as_mut()
            .ok_or("Card unavailable")?
            .transaction()
            .map(|t| Box::new(NativeSession(t)) as Box<dyn Session>)
            .map_err(|e| format!("PC/SC error: {e}"))
    }
}
impl Backend for NativeBackend {
    fn readers(&self) -> Result<Vec<String>> {
        let ctx = pcsc::Context::establish(pcsc::Scope::System).map_err(|e| e.to_string())?;
        let names = match ctx.list_readers_owned() {
            Ok(names) => names,
            Err(pcsc::Error::NoReadersAvailable) => return Ok(vec![]),
            Err(error) => return Err(error.to_string()),
        };
        names
            .iter()
            .map(|name| {
                // Never replace invalid bytes: a lossy label could select another reader.
                name.to_str().map(str::to_owned).map_err(|_| {
                    "PC/SC reader name is not UTF-8 (Windows pcsc uses the ANSI API)".into()
                })
            })
            .collect()
    }

    fn open(&self, name: &str) -> Result<Box<dyn Card>> {
        let ctx = pcsc::Context::establish(pcsc::Scope::System).map_err(|e| e.to_string())?;
        let name = CString::new(name).map_err(|e| e.to_string())?;
        ctx.connect(&name, pcsc::ShareMode::Shared, pcsc::Protocols::T1)
            .map(|c| Box::new(NativeCard(Some(c))) as Box<dyn Card>)
            .map_err(|e| e.to_string())
    }
}
const NAMES: [&str; 3] = ["Mock B-CAS", "Mock ACAS", "Mock C-CAS"];
pub struct MockBackend {
    enumerations: Arc<AtomicUsize>,
    probes: AtomicUsize,
    state: State,
}
impl MockBackend {
    pub fn new(state: State) -> Self {
        Self {
            enumerations: Arc::new(AtomicUsize::new(0)),
            probes: AtomicUsize::new(0),
            state,
        }
    }
}
struct MockCard {
    kind: usize,
    authenticated: bool,
    token: u8,
    replacement: Option<Arc<AtomicUsize>>,
}
impl Card for MockCard {
    fn session(&mut self) -> Result<Box<dyn Session + '_>> {
        Ok(Box::new(MockSession(self)))
    }
}
struct MockSession<'a>(&'a mut MockCard);
impl Session for MockSession<'_> {
    fn transmit(&mut self, a: &[u8]) -> Result<Vec<u8>> {
        let c = &mut self.0;
        if c.replacement
            .as_ref()
            .is_some_and(|n| n.load(Ordering::Relaxed) >= 2)
        {
            c.kind = 1;
        }
        if a.get(1) == Some(&0xfe) {
            return Err("mock reset/removal/service failure".into());
        }
        if a.get(1) == Some(&0xfd) {
            return Ok(vec![0; 256]);
        }
        if is_auth(a) {
            if c.kind != 1 {
                return Ok(vec![0x90, 0]);
            }
            c.authenticated = true;
            c.token = a.last().copied().unwrap_or(0);
            let mut r = vec![0; 48];
            r[0] = 0xa1;
            r[46] = 0x90;
            return Ok(r);
        }
        if a.get(1) == Some(&0x30) {
            let mut r = vec![0; 59];
            r[0] = 0xb0 + c.kind as u8;
            r[7] = if c.kind == 2 { 2 } else { 5 };
            r[57] = 0x90;
            return Ok(r);
        }
        if c.kind == 1 && !c.authenticated {
            return Ok(vec![0x69, 0x82]);
        }
        c.authenticated = false;
        Ok(if c.kind == 1 {
            vec![0xb1, c.token, 0x90, 0]
        } else {
            vec![0xb0 + c.kind as u8, 0x90, 0]
        })
    }
}
impl Backend for MockBackend {
    fn readers(&self) -> Result<Vec<String>> {
        let count = self.enumerations.fetch_add(1, Ordering::Relaxed) + 1;
        self.state.log(format!("MOCK enumerate {count}"));
        let mut names: Vec<String> =
            if std::env::var_os("BONCASRELAY_MOCK_SWAP_BCAS_TO_ACAS").is_some() {
                vec![NAMES[0].into()]
            } else {
                NAMES.iter().map(|s| s.to_string()).collect()
            };
        if count >= 2 && std::env::var_os("BONCASRELAY_MOCK_AMBIGUOUS_FIXED").is_some() {
            names.push("Mock C-CAS extra".into());
        }
        Ok(names)
    }
    fn open(&self, name: &str) -> Result<Box<dyn Card>> {
        let kind = NAMES
            .iter()
            .position(|n| *n == name)
            .ok_or("Reader unavailable")?;
        Ok(Box::new(MockCard {
            kind,
            authenticated: false,
            token: 0,
            replacement: if kind == 0
                && std::env::var_os("BONCASRELAY_MOCK_SWAP_BCAS_TO_ACAS").is_some()
            {
                Some(self.enumerations.clone())
            } else {
                None
            },
        }))
    }
    fn probe(&self, s: &mut dyn Session, a: &[u8]) -> Result<Vec<u8>> {
        self.state.log(format!(
            "MOCK probe {}",
            self.probes.fetch_add(1, Ordering::Relaxed) + 1
        ));
        if let Ok(ms) = std::env::var("BONCASRELAY_MOCK_PROBE_DELAY_MS")
            .unwrap_or_default()
            .parse::<u64>()
        {
            if (1..=1000).contains(&ms) {
                std::thread::sleep(Duration::from_millis(ms));
            }
        }
        s.transmit(a)
    }
}
