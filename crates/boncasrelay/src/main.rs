#![forbid(unsafe_code)]
mod control;
#[cfg(feature = "tray")]
mod tray;
mod web;
use boncasrelay_core::{
    backend::{Backend, MockBackend, NativeBackend, Session},
    classify,
    config::Config,
    frame, is_auth,
    router::{Reader, Router},
    state::{ClientState, State},
    success, Kind, Result,
};
use control::Control;
use std::{
    net::{SocketAddr, TcpListener, TcpStream},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};
const SECOND: Duration = Duration::from_secs(1);
fn wait_input(
    s: &TcpStream,
    stop: &AtomicBool,
    deadline: Option<Instant>,
) -> std::io::Result<bool> {
    s.set_read_timeout(Some(Duration::from_millis(50)))?;
    loop {
        if stop.load(Ordering::Relaxed) {
            return Ok(false);
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            return Ok(false);
        }
        match s.peek(&mut [0]) {
            Ok(0) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "closed",
                ))
            }
            Ok(_) => return Ok(true),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::TimedOut
                        | std::io::ErrorKind::Interrupted
                ) => {}
            Err(e) => return Err(e),
        }
    }
}
enum CommandError {
    Card(String),
    Protocol(String),
    Wire(std::io::Error),
}
impl From<String> for CommandError {
    fn from(message: String) -> Self {
        Self::Protocol(message)
    }
}
impl From<&str> for CommandError {
    fn from(message: &str) -> Self {
        Self::Protocol(message.into())
    }
}
impl From<std::io::Error> for CommandError {
    fn from(error: std::io::Error) -> Self {
        Self::Wire(error)
    }
}
type CommandResult<T> = std::result::Result<T, CommandError>;
enum Outcome {
    Reply(Vec<u8>),
    Pending(Vec<u8>),
    LeaseExpired,
}
// A native PC/SC Transaction borrows its Card. Keep both the reader mutex and
// transaction on this worker's stack through authentication and its next APDU.
fn session(
    s: &mut TcpStream,
    router: &Router,
    reader: &Reader,
    transaction: &mut dyn Session,
    mut apdu: Vec<u8>,
    control: &Control,
) -> CommandResult<Outcome> {
    loop {
        if control.config.read().verbose {
            router
                .state
                .log(format!("{} APDU > {}", reader.name, hex(&apdu)));
        }
        let response = transaction.transmit(&apdu).map_err(CommandError::Card)?;
        if control.config.read().verbose {
            router
                .state
                .log(format!("{} APDU < {}", reader.name, hex(&response)));
        }
        if response.len() > 255 {
            return Err("Response exceeds one-byte wire length".into());
        }
        if !is_auth(&apdu) || !success(&response) {
            // The caller releases the transaction and reader lock before sending.
            return Ok(Outcome::Reply(response));
        }
        let deadline = Instant::now() + SECOND;
        frame::write_frame(s, &response, deadline, &router.stop)?;
        if !wait_input(s, &router.stop, Some(deadline))? {
            return Ok(Outcome::LeaseExpired);
        }
        apdu = frame::read_frame(s, deadline, &router.stop)?;
        if apdu.is_empty() || classify(&apdu) != Kind::Acas {
            return Ok(Outcome::Pending(apdu));
        }
    }
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x} ")).collect()
}
fn client(mut s: TcpStream, router: Arc<Router>, c: Arc<Control>, fixed: String, id: u64) {
    let mut pending = None;
    let mut last_reader = String::new();
    loop {
        let apdu = if let Some(a) = pending.take() {
            a
        } else {
            if !matches!(wait_input(&s, &router.stop, None), Ok(true)) {
                break;
            }
            match frame::read_frame(&mut s, Instant::now() + SECOND, &router.stop) {
                Ok(a) => a,
                Err(_) => break,
            }
        };
        let command = (|| -> CommandResult<Outcome> {
            if apdu.is_empty() {
                return Err("Empty APDU".into());
            }
            let reader = router.select(classify(&apdu), &fixed)?;
            let mut card = reader
                .card
                .try_lock_for(Duration::from_secs(3))
                .ok_or("Card lock timeout")?;
            if router.stop.load(Ordering::Relaxed) {
                return Err("Server stopping".into());
            }
            let result = (|| -> CommandResult<Outcome> {
                if card.is_none() {
                    *card = Some(
                        router
                            .backend
                            .open(&reader.name)
                            .map_err(CommandError::Card)?,
                    );
                }
                let mut transaction = card
                    .as_mut()
                    .ok_or("Card unavailable")?
                    .session()
                    .map_err(CommandError::Card)?;
                if last_reader != reader.name {
                    last_reader = reader.name.clone();
                    router
                        .state
                        .log(format!("Client reader selected: {last_reader}"));
                }
                router.state.update(|state| {
                    if let Some(client) = state.clients.get_mut(&id) {
                        client.reader = Some(reader.name.clone());
                    }
                });
                session(&mut s, &router, &reader, &mut *transaction, apdu, &c)
            })();
            // The transaction has ended before invalidating or dropping the Card.
            if matches!(result, Err(CommandError::Card(_))) {
                router.invalidate(&reader);
                *card = None;
            }
            result
        })();
        router.state.update(|state| {
            if let Some(client) = state.clients.get_mut(&id) {
                client.reader = None;
            }
        });
        let response = match command {
            Ok(Outcome::Pending(next)) => {
                pending = Some(next);
                None
            }
            Ok(Outcome::LeaseExpired) => None,
            Ok(Outcome::Reply(response)) => Some(response),
            Err(CommandError::Wire(e)) => {
                router.state.log(format!("Client error: {e}"));
                break;
            }
            Err(CommandError::Card(e) | CommandError::Protocol(e)) => {
                router.state.log(format!("Command failed: {e}"));
                Some(vec![])
            }
        };
        if let Some(response) = response {
            if frame::write_frame(&mut s, &response, Instant::now() + SECOND, &router.stop).is_err()
            {
                break;
            }
        }
        if router.stop.load(Ordering::Relaxed) {
            break;
        }
    }
    router.state.update(|state| {
        state.clients.remove(&id);
    });
    router.state.log("Client disconnected");
}
fn listen(c: &Config, port: u16) -> Result<TcpListener> {
    let addr = SocketAddr::new(c.address, port);
    let socket = socket2::Socket::new(
        socket2::Domain::for_address(addr),
        socket2::Type::STREAM,
        Some(socket2::Protocol::TCP),
    )
    .map_err(|e| e.to_string())?;
    #[cfg(not(windows))]
    socket.set_reuse_address(true).map_err(|e| e.to_string())?;
    if c.address.is_ipv6() {
        socket.set_only_v6(true).map_err(|e| e.to_string())?;
    }
    socket
        .bind(&addr.into())
        .map_err(|e| format!("Cannot listen on {addr}: {e}"))?;
    // Windows needs a larger accept queue for short connection bursts. i32::MAX
    // is Winsock's SOMAXCONN value; cap the portable Unix backlog at 128.
    let backlog = if cfg!(windows) { i32::MAX } else { 128 };
    socket.listen(backlog).map_err(|e| e.to_string())?;
    socket.set_nonblocking(true).map_err(|e| e.to_string())?;
    Ok(socket.into())
}
fn setup(c: Config) -> Result<Option<Arc<Control>>> {
    if c.help {
        println!("boncasrelay [--config FILE] [--listen IP] [--port PORT]\n  --reader-port READER=PORT  repeatable; exact or unique substring\n  --bcas-reader NAME  --acas-reader NAME\n  --allow CIDR  repeatable IPv4/IPv6; default all hosts\n  --list-readers  --verbose  --help\n  --web-listen IP:PORT  --no-web  --tray (requires tray feature)\n  --mock  deterministic test cards; never use for real decoding\nDefaults: 0.0.0.0:6900; CLI overrides UTF-8 INI settings");
        return Ok(None);
    }
    let state = State::default();
    let backend: Arc<dyn Backend> = if c.mock {
        Arc::new(MockBackend::new(state.clone()))
    } else {
        Arc::new(NativeBackend)
    };
    if c.list {
        match backend.readers() {
            Ok(names) => {
                if names.is_empty() {
                    println!("No readers found");
                }
                for name in names {
                    println!("{name}");
                }
            }
            Err(e) => state.log(format!("Reader enumeration unavailable: {e}")),
        }
        return Ok(None);
    }
    let stop = Arc::new(AtomicBool::new(false));
    let signal = stop.clone();
    ctrlc::set_handler(move || signal.store(true, Ordering::Relaxed)).map_err(|e| e.to_string())?;
    let c = Arc::new(c);
    let router = Arc::new(Router::new(
        backend,
        (*c).clone(),
        state.clone(),
        stop.clone(),
    )?);
    if c.allow.is_empty() {
        state
            .log("WARNING: no --allow restriction; plaintext unauthenticated access from any host");
    }
    if c.mock {
        state.log("WARNING: mock backend enabled (no real card)");
    }
    let control = Control::new((*c).clone(), router)?;
    for (listener, fixed) in &*control.listeners.lock() {
        state.log(format!(
            "Listening {} {}",
            listener.local_addr().map_err(|e| e.to_string())?,
            if fixed.is_empty() { "automatic" } else { fixed }
        ));
    }
    Ok(Some(control))
}
fn run(control: Arc<Control>) -> Result<()> {
    let state = control.router.state.clone();
    let stop = control.stop.clone();
    let router = control.router.clone();
    let web = web::start(control.clone())?;
    let mut workers: Vec<thread::JoinHandle<()>> = vec![];
    let mut id = 0;
    let result = (|| -> Result<()> {
        while !stop.load(Ordering::Relaxed) {
            let mut i = 0;
            while i < workers.len() {
                if workers[i].is_finished() {
                    let _ = workers.swap_remove(i).join();
                } else {
                    i += 1;
                }
            }
            for (listener, fixed) in &*control.listeners.lock() {
                match listener.accept() {
                    Ok((s, peer)) => {
                        let c = control.config.read();
                        if workers.len() >= 128
                            || (!c.allow.is_empty()
                                && !c.allow.iter().any(|n| n.contains(&peer.ip())))
                        {
                            state.log(format!("Client rejected: {}", peer.ip()));
                            continue;
                        }
                        drop(c);
                        id += 1;
                        let client_id = id;
                        state.log(format!("Client connected: {}", peer.ip()));
                        state.update(|state| {
                            state.clients.insert(
                                id,
                                ClientState {
                                    peer: peer.to_string(),
                                    connected_at: std::time::SystemTime::now()
                                        .duration_since(std::time::UNIX_EPOCH)
                                        .unwrap_or_default()
                                        .as_secs(),
                                    reader: None,
                                },
                            );
                        });
                        let (r, c, f) = (router.clone(), control.clone(), fixed.clone());
                        workers.push(
                            thread::Builder::new()
                                .name(format!("boncas-client-{id}"))
                                .spawn(move || client(s, r, c, f, client_id))
                                .map_err(|e| e.to_string())?,
                        );
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(e) => return Err(e.to_string()),
                }
            }
            thread::sleep(Duration::from_millis(10));
        }
        Ok(())
    })();
    stop.store(true, Ordering::Relaxed);
    control.listeners.lock().clear();
    for worker in workers {
        let _ = worker.join();
    }
    if let Some(web) = web {
        let _ = web.join();
    }
    state.log("Server stopped");
    result
}
fn launch(c: Config) -> Result<()> {
    if c.tray {
        #[cfg(not(feature = "tray"))]
        return Err("--tray requires a build with --features tray".into());
        #[cfg(feature = "tray")]
        {
            if c.no_web {
                return Err("--tray requires Web UI; remove --no-web".into());
            }
            if let Some(control) = setup(c)? {
                let worker_control = control.clone();
                let worker = thread::Builder::new()
                    .name("boncas-server".into())
                    .spawn(move || {
                        let result = run(worker_control.clone());
                        worker_control.stop.store(true, Ordering::Relaxed);
                        result
                    })
                    .map_err(|e| e.to_string())?;
                let tray = tray::run(control.clone());
                control.stop.store(true, Ordering::Relaxed);
                let result = worker
                    .join()
                    .map_err(|_| "Server thread failed".to_string())?;
                tray?;
                result?;
            }
            return Ok(());
        }
    }
    if let Some(control) = setup(c)? {
        run(control)?;
    }
    Ok(())
}
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Err(e) = Config::parse(&args).and_then(launch) {
        eprintln!("Fatal: {e}");
        std::process::exit(1);
    }
}
