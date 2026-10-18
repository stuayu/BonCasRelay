#![deny(unsafe_code)]
pub mod backend;
pub mod config;
pub mod frame;
pub mod router;
pub mod state;

pub type Result<T> = std::result::Result<T, String>;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Bcas = 0,
    Acas = 1,
}
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Probe {
    pub openable: bool,
    pub bcas: bool,
    pub acas: bool,
}
pub fn is_auth(a: &[u8]) -> bool {
    a.starts_with(&[0x90, 0xa0])
}
pub fn classify(a: &[u8]) -> Kind {
    if is_auth(a) || (a.len() >= 4 && a[0] == 0x90 && a[3] == 1) {
        Kind::Acas
    } else {
        Kind::Bcas
    }
}
pub fn success(r: &[u8]) -> bool {
    r.ends_with(&[0x90, 0])
}
pub fn acas_response(r: &[u8]) -> bool {
    r.len() >= 48 && success(r)
}
pub fn ca_system_id(r: &[u8]) -> u16 {
    if r.len() >= 10 && success(r) {
        u16::from_be_bytes([r[6], r[7]])
    } else {
        0
    }
}
pub fn bcas_response(r: &[u8]) -> bool {
    r.len() >= 57 && ca_system_id(r) == 5
}
pub fn auth_probe(random: [u8; 8]) -> [u8; 22] {
    let mut a = [0; 22];
    a[..13].copy_from_slice(&[0x90, 0xa0, 0, 1, 0x10, 0, 0, 0, 1, 0, 0, 0x8a, 0xf7]);
    a[13..21].copy_from_slice(&random);
    a
}
pub const INIT_PROBE: [u8; 5] = [0x90, 0x30, 0, 0, 0];
pub fn select_reader(kind: Kind, results: &[Probe], pin: Option<usize>) -> Option<usize> {
    pin.filter(|i| *i < results.len())
        .or_else(|| {
            results
                .iter()
                .position(|r| r.openable && if kind == Kind::Acas { r.acas } else { r.bcas })
        })
        .or_else(|| {
            results
                .iter()
                .position(|r| r.openable && !r.acas && !r.bcas)
        })
}
