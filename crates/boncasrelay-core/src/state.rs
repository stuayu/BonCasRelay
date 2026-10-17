use crate::Probe;
use parking_lot::Mutex;
use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
#[derive(Clone, Default, Debug, serde::Serialize)]
pub struct Snapshot {
    pub clients: BTreeMap<u64, ClientState>,
    pub readers: Vec<String>,
    pub classifications: BTreeMap<String, Probe>,
    pub routes: [Option<String>; 2],
    pub fixed_routes: BTreeMap<String, Option<String>>,
    pub logs: VecDeque<String>,
}
#[derive(Clone, Debug, serde::Serialize)]
pub struct ClientState {
    pub peer: String,
    pub connected_at: u64,
    pub reader: Option<String>,
}
/// Cloned snapshots never retain a lock while the caller renders or serializes.
#[derive(Clone, Default)]
pub struct State(Arc<Mutex<Snapshot>>);
impl State {
    pub fn snapshot(&self) -> Snapshot {
        self.0.lock().clone()
    }
    pub fn update(&self, f: impl FnOnce(&mut Snapshot)) {
        f(&mut self.0.lock());
    }
    pub fn log(&self, message: impl Into<String>) {
        let message = message.into();
        // Serialize stderr output with history publication for complete log lines.
        let mut state = self.0.lock();
        eprintln!("{message}");
        if state.logs.len() == 256 {
            state.logs.pop_front();
        }
        state.logs.push_back(message);
    }
}
