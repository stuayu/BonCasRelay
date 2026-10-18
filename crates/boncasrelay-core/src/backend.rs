use crate::{is_auth, state::State, Result};
use std::{
    ffi::CString,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

#[cfg(unix)]
#[allow(unsafe_code)]
mod dynamic_pcsc {
    use super::*;
    use libloading::Library;
    use std::{
        ffi::{c_char, c_void},
        fs,
        path::{Path, PathBuf},
        sync::Arc,
    };

    #[cfg(target_os = "macos")]
    type Long = i32;
    #[cfg(not(target_os = "macos"))]
    use std::ffi::c_long;
    #[cfg(not(target_os = "macos"))]
    type Long = c_long;
    #[cfg(target_os = "macos")]
    type Dword = u32;
    #[cfg(not(target_os = "macos"))]
    type Dword = std::os::raw::c_ulong;
    type Context = Long;
    type Handle = Long;

    type Establish =
        unsafe extern "C" fn(Dword, *const c_void, *const c_void, *mut Context) -> Long;
    type Release = unsafe extern "C" fn(Context) -> Long;
    type ListReaders =
        unsafe extern "C" fn(Context, *const c_char, *mut c_char, *mut Dword) -> Long;
    type Connect =
        unsafe extern "C" fn(Context, *const c_char, Dword, Dword, *mut Handle, *mut Dword) -> Long;
    type Disconnect = unsafe extern "C" fn(Handle, Dword) -> Long;
    type Begin = unsafe extern "C" fn(Handle) -> Long;
    type End = unsafe extern "C" fn(Handle, Dword) -> Long;
    type Transmit = unsafe extern "C" fn(
        Handle,
        *const IoRequest,
        *const u8,
        Dword,
        *mut IoRequest,
        *mut u8,
        *mut Dword,
    ) -> Long;

    #[repr(C)]
    struct IoRequest {
        protocol: Dword,
        length: Dword,
    }

    struct Api {
        _library: Library,
        establish: Establish,
        release: Release,
        list_readers: ListReaders,
        connect: Connect,
        disconnect: Disconnect,
        begin: Begin,
        end: End,
        transmit: Transmit,
        source: PathBuf,
    }

    impl Api {
        // SAFETY: The caller must supply a PC/SC-compatible library whose exported symbols use
        // the ABI declared by these function pointer types. The Library remains owned by Api.
        unsafe fn load(path: &Path) -> std::result::Result<Arc<Self>, String> {
            let library = Library::new(path).map_err(|e| e.to_string())?;
            let establish = *library
                .get::<Establish>(b"SCardEstablishContext\0")
                .map_err(|e| e.to_string())?;
            let release = *library
                .get::<Release>(b"SCardReleaseContext\0")
                .map_err(|e| e.to_string())?;
            let list_readers = *library
                .get::<ListReaders>(b"SCardListReaders\0")
                .or_else(|_| library.get::<ListReaders>(b"SCardListReadersA\0"))
                .map_err(|e| e.to_string())?;
            let connect = *library
                .get::<Connect>(b"SCardConnect\0")
                .or_else(|_| library.get::<Connect>(b"SCardConnectA\0"))
                .map_err(|e| e.to_string())?;
            let disconnect = *library
                .get::<Disconnect>(b"SCardDisconnect\0")
                .map_err(|e| e.to_string())?;
            let begin = *library
                .get::<Begin>(b"SCardBeginTransaction\0")
                .map_err(|e| e.to_string())?;
            let end = *library
                .get::<End>(b"SCardEndTransaction\0")
                .map_err(|e| e.to_string())?;
            let transmit = *library
                .get::<Transmit>(b"SCardTransmit\0")
                .map_err(|e| e.to_string())?;
            Ok(Arc::new(Self {
                _library: library,
                establish,
                release,
                list_readers,
                connect,
                disconnect,
                begin,
                end,
                transmit,
                source: path.to_owned(),
            }))
        }

        fn context(&self) -> Result<Context> {
            let mut context = 0;
            // SAFETY: PC/SC accepts null reserved pointers and writes the context to this valid out-pointer.
            let status =
                unsafe { (self.establish)(2, std::ptr::null(), std::ptr::null(), &mut context) };
            if status == 0 {
                Ok(context)
            } else {
                Err(format!("PC/SC error: 0x{:08x}", status as u32))
            }
        }
    }

    pub struct Backend {
        api: Arc<Api>,
    }

    impl Backend {
        pub fn detect() -> Option<Self> {
            for path in candidates() {
                // SAFETY: Candidate paths are loaded only as PC/SC libraries; Api retains the library
                // for as long as any copied function pointer can be called.
                if let Ok(api) = unsafe { Api::load(&path) } {
                    return Some(Self { api });
                }
            }
            None
        }

        pub fn source(&self) -> &Path {
            &self.api.source
        }
    }

    impl super::Backend for Backend {
        fn readers(&self) -> Result<Vec<String>> {
            const NO_READERS: Long = 0x8010_002E_u32 as Long;
            let context = self.api.context()?;
            let mut length: Dword = 0;
            // SAFETY: This is the documented size query: null reader buffer and valid length out-pointer.
            let status = unsafe {
                (self.api.list_readers)(
                    context,
                    std::ptr::null(),
                    std::ptr::null_mut(),
                    &mut length,
                )
            };
            let result = if status == NO_READERS {
                Ok(vec![])
            } else if status != 0 && status != 0x8010_0008_u32 as Long {
                Err(format!("PC/SC error: 0x{:08x}", status as u32))
            } else if length == 0 {
                Ok(vec![])
            } else {
                let mut buffer = vec![0u8; length as usize];
                // SAFETY: The allocated buffer has `length` bytes and the API receives its capacity
                // through the initialized in/out length value.
                let status = unsafe {
                    (self.api.list_readers)(
                        context,
                        std::ptr::null(),
                        buffer.as_mut_ptr().cast(),
                        &mut length,
                    )
                };
                if status != 0 {
                    Err(format!("PC/SC error: 0x{:08x}", status as u32))
                } else {
                    let mut readers = Vec::new();
                    for name in buffer
                        .split(|b| *b == 0)
                        .take_while(|name| !name.is_empty())
                    {
                        readers.push(
                            std::str::from_utf8(name)
                                .map_err(|_| "PC/SC reader name is not UTF-8".to_string())?
                                .to_owned(),
                        );
                    }
                    Ok(readers)
                }
            };
            // SAFETY: `context` was successfully established and is released once after enumeration.
            unsafe {
                (self.api.release)(context);
            }
            result
        }

        fn open(&self, name: &str) -> Result<Box<dyn Card>> {
            let name = CString::new(name).map_err(|e| e.to_string())?;
            let context = self.api.context()?;
            let mut card = 0;
            let mut protocol = 0;
            // SAFETY: name is NUL-terminated; card and protocol are valid output pointers.
            let status = unsafe {
                (self.api.connect)(context, name.as_ptr(), 2, 2, &mut card, &mut protocol)
            };
            if status != 0 {
                // SAFETY: `context` was successfully established and has not yet been released.
                unsafe {
                    (self.api.release)(context);
                }
                return Err(format!("PC/SC error: 0x{:08x}", status as u32));
            }
            Ok(Box::new(DynamicCard {
                api: self.api.clone(),
                context,
                card,
                protocol,
            }))
        }
    }

    struct DynamicCard {
        api: Arc<Api>,
        context: Context,
        card: Handle,
        protocol: Dword,
    }
    impl Drop for DynamicCard {
        fn drop(&mut self) {
            // SAFETY: Both handles were returned by successful PC/SC calls; Drop runs once.
            unsafe {
                (self.api.disconnect)(self.card, 0);
                (self.api.release)(self.context);
            }
        }
    }
    impl Card for DynamicCard {
        fn session(&mut self) -> Result<Box<dyn Session + '_>> {
            // SAFETY: `card` is a live handle owned by this DynamicCard.
            let status = unsafe { (self.api.begin)(self.card) };
            if status != 0 {
                return Err(format!("PC/SC error: 0x{:08x}", status as u32));
            }
            Ok(Box::new(DynamicSession {
                api: self.api.clone(),
                card: self.card,
                protocol: self.protocol,
            }))
        }
    }
    struct DynamicSession {
        api: Arc<Api>,
        card: Handle,
        protocol: Dword,
    }
    impl Drop for DynamicSession {
        fn drop(&mut self) {
            // SAFETY: `card` has an active transaction begun before this session was created.
            unsafe {
                (self.api.end)(self.card, 0);
            }
        }
    }
    impl Session for DynamicSession {
        fn transmit(&mut self, apdu: &[u8]) -> Result<Vec<u8>> {
            let send_pci = IoRequest {
                protocol: self.protocol,
                length: std::mem::size_of::<IoRequest>() as Dword,
            };
            let mut receive_pci = IoRequest {
                protocol: 0,
                length: 0,
            };
            let mut response = [0u8; 4096];
            let mut response_length = response.len() as Dword;
            // SAFETY: Input pointers reference live slices/structs, output pointers reference writable
            // stack storage, and the advertised response capacity matches the buffer size.
            let status = unsafe {
                (self.api.transmit)(
                    self.card,
                    &send_pci,
                    apdu.as_ptr(),
                    apdu.len() as Dword,
                    &mut receive_pci,
                    response.as_mut_ptr(),
                    &mut response_length,
                )
            };
            if status == 0 {
                Ok(response[..response_length as usize].to_vec())
            } else {
                Err(format!("PC/SC error: 0x{:08x}", status as u32))
            }
        }
    }

    fn candidates() -> Vec<PathBuf> {
        let mut found = Vec::new();
        if let Ok(cwd) = std::env::current_dir() {
            if let Ok(entries) = fs::read_dir(cwd) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_file() && is_library_file(&path) {
                        found.push(path);
                    }
                }
            }
        }
        let mut dirs: Vec<PathBuf> = std::env::var_os("LD_LIBRARY_PATH")
            .into_iter()
            .chain(std::env::var_os("DYLD_LIBRARY_PATH"))
            .flat_map(|v| std::env::split_paths(&v).collect::<Vec<_>>())
            .collect();
        dirs.extend(
            [
                "/usr/lib",
                "/usr/lib64",
                "/usr/local/lib",
                "/opt/homebrew/lib",
                "/opt/local/lib",
            ]
            .into_iter()
            .map(PathBuf::from),
        );
        for dir in dirs {
            if let Ok(entries) = fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                    if name.starts_with("libpcsckai.so") || name.starts_with("libpcsckai.dylib") {
                        found.push(path);
                    } else if path.is_dir() {
                        if let Ok(nested) = fs::read_dir(path) {
                            for child in nested.flatten() {
                                let candidate = child.path();
                                let name =
                                    candidate.file_name().and_then(|n| n.to_str()).unwrap_or("");
                                if name.starts_with("libpcsckai.so")
                                    || name.starts_with("libpcsckai.dylib")
                                {
                                    found.push(candidate);
                                }
                            }
                        }
                    }
                }
            }
        }
        found.sort();
        found.dedup();
        found
    }
    fn is_library_file(path: &Path) -> bool {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        name.ends_with(".so")
            || name.contains(".so.")
            || name.ends_with(".dylib")
            || name.ends_with(".dll")
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
mod dynamic_pcsc {
    use super::*;
    use libloading::Library;
    use std::{
        ffi::{c_char, c_void},
        fs,
        path::{Path, PathBuf},
        sync::Arc,
    };

    type Long = i32;
    type Dword = u32;
    type Context = usize;
    type Handle = usize;

    type Establish =
        unsafe extern "system" fn(Dword, *const c_void, *const c_void, *mut Context) -> Long;
    type Release = unsafe extern "system" fn(Context) -> Long;
    type ListReaders =
        unsafe extern "system" fn(Context, *const c_char, *mut c_char, *mut Dword) -> Long;
    type Connect = unsafe extern "system" fn(
        Context,
        *const c_char,
        Dword,
        Dword,
        *mut Handle,
        *mut Dword,
    ) -> Long;
    type Disconnect = unsafe extern "system" fn(Handle, Dword) -> Long;
    type Begin = unsafe extern "system" fn(Handle) -> Long;
    type End = unsafe extern "system" fn(Handle, Dword) -> Long;
    type Transmit = unsafe extern "system" fn(
        Handle,
        *const IoRequest,
        *const u8,
        Dword,
        *mut IoRequest,
        *mut u8,
        *mut Dword,
    ) -> Long;

    #[repr(C)]
    struct IoRequest {
        protocol: Dword,
        length: Dword,
    }

    struct Api {
        _library: Library,
        establish: Establish,
        release: Release,
        list_readers: ListReaders,
        connect: Connect,
        disconnect: Disconnect,
        begin: Begin,
        end: End,
        transmit: Transmit,
        source: PathBuf,
    }

    impl Api {
        // SAFETY: The caller must supply a PC/SC-compatible library whose exported symbols use
        // the ABI declared by these function pointer types. The Library remains owned by Api.
        unsafe fn load(path: &Path) -> std::result::Result<Arc<Self>, String> {
            let library = Library::new(path).map_err(|e| e.to_string())?;
            let establish = *library
                .get::<Establish>(b"SCardEstablishContext\0")
                .map_err(|e| e.to_string())?;
            let release = *library
                .get::<Release>(b"SCardReleaseContext\0")
                .map_err(|e| e.to_string())?;
            let list_readers = *library
                .get::<ListReaders>(b"SCardListReadersA\0")
                .or_else(|_| library.get::<ListReaders>(b"SCardListReaders\0"))
                .map_err(|e| e.to_string())?;
            let connect = *library
                .get::<Connect>(b"SCardConnectA\0")
                .or_else(|_| library.get::<Connect>(b"SCardConnect\0"))
                .map_err(|e| e.to_string())?;
            let disconnect = *library
                .get::<Disconnect>(b"SCardDisconnect\0")
                .map_err(|e| e.to_string())?;
            let begin = *library
                .get::<Begin>(b"SCardBeginTransaction\0")
                .map_err(|e| e.to_string())?;
            let end = *library
                .get::<End>(b"SCardEndTransaction\0")
                .map_err(|e| e.to_string())?;
            let transmit = *library
                .get::<Transmit>(b"SCardTransmit\0")
                .map_err(|e| e.to_string())?;
            Ok(Arc::new(Self {
                _library: library,
                establish,
                release,
                list_readers,
                connect,
                disconnect,
                begin,
                end,
                transmit,
                source: path.to_owned(),
            }))
        }

        fn context(&self) -> Result<Context> {
            let mut context = 0;
            // SAFETY: PC/SC accepts null reserved pointers and writes the context to this valid out-pointer.
            let status =
                unsafe { (self.establish)(2, std::ptr::null(), std::ptr::null(), &mut context) };
            if status == 0 {
                Ok(context)
            } else {
                Err(format!("PC/SC error: 0x{:08x}", status as u32))
            }
        }
    }

    pub struct Backend {
        api: Arc<Api>,
    }

    impl Backend {
        pub fn detect() -> Option<Self> {
            for path in candidates() {
                // SAFETY: Candidate paths are loaded only as PC/SC libraries; Api retains the library
                // for as long as any copied function pointer can be called.
                if let Ok(api) = unsafe { Api::load(&path) } {
                    return Some(Self { api });
                }
            }
            None
        }
        pub fn source(&self) -> &Path {
            &self.api.source
        }
    }

    impl super::Backend for Backend {
        fn readers(&self) -> Result<Vec<String>> {
            const NO_READERS: Long = 0x8010_002E_u32 as Long;
            let context = self.api.context()?;
            let mut length = 0;
            // SAFETY: This is the documented size query: null reader buffer and valid length out-pointer.
            let status = unsafe {
                (self.api.list_readers)(
                    context,
                    std::ptr::null(),
                    std::ptr::null_mut(),
                    &mut length,
                )
            };
            let result = if status == NO_READERS {
                Ok(vec![])
            } else if status != 0 && status != 0x8010_0008_u32 as Long {
                Err(format!("PC/SC error: 0x{:08x}", status as u32))
            } else if length == 0 {
                Ok(vec![])
            } else {
                let mut buffer = vec![0u8; length as usize];
                // SAFETY: The allocated buffer has `length` bytes and the API receives its capacity
                // through the initialized in/out length value.
                let status = unsafe {
                    (self.api.list_readers)(
                        context,
                        std::ptr::null(),
                        buffer.as_mut_ptr().cast(),
                        &mut length,
                    )
                };
                if status != 0 {
                    Err(format!("PC/SC error: 0x{:08x}", status as u32))
                } else {
                    let mut readers = Vec::new();
                    for name in buffer
                        .split(|b| *b == 0)
                        .take_while(|name| !name.is_empty())
                    {
                        readers.push(String::from_utf8(name.to_vec()).map_err(|_| {
                            "PC/SC reader name is not valid Windows ANSI/UTF-8".to_string()
                        })?);
                    }
                    Ok(readers)
                }
            };
            // SAFETY: `context` was successfully established and is released once after enumeration.
            unsafe {
                (self.api.release)(context);
            }
            result
        }

        fn open(&self, name: &str) -> Result<Box<dyn Card>> {
            let name = CString::new(name).map_err(|e| e.to_string())?;
            let context = self.api.context()?;
            let mut card = 0;
            let mut protocol = 0;
            // SAFETY: name is NUL-terminated; card and protocol are valid output pointers.
            let status = unsafe {
                (self.api.connect)(context, name.as_ptr(), 2, 2, &mut card, &mut protocol)
            };
            if status != 0 {
                // SAFETY: `context` was successfully established and has not yet been released.
                unsafe {
                    (self.api.release)(context);
                }
                return Err(format!("PC/SC error: 0x{:08x}", status as u32));
            }
            Ok(Box::new(DynamicCard {
                api: self.api.clone(),
                context,
                card,
                protocol,
            }))
        }
    }

    struct DynamicCard {
        api: Arc<Api>,
        context: Context,
        card: Handle,
        protocol: Dword,
    }
    impl Drop for DynamicCard {
        fn drop(&mut self) {
            // SAFETY: Both handles were returned by successful PC/SC calls; Drop runs once.
            unsafe {
                (self.api.disconnect)(self.card, 0);
                (self.api.release)(self.context);
            }
        }
    }
    impl Card for DynamicCard {
        fn session(&mut self) -> Result<Box<dyn Session + '_>> {
            // SAFETY: `card` is a live handle owned by this DynamicCard.
            let status = unsafe { (self.api.begin)(self.card) };
            if status != 0 {
                return Err(format!("PC/SC error: 0x{:08x}", status as u32));
            }
            Ok(Box::new(DynamicSession {
                api: self.api.clone(),
                card: self.card,
                protocol: self.protocol,
            }))
        }
    }
    struct DynamicSession {
        api: Arc<Api>,
        card: Handle,
        protocol: Dword,
    }
    impl Drop for DynamicSession {
        fn drop(&mut self) {
            // SAFETY: `card` has an active transaction begun before this session was created.
            unsafe {
                (self.api.end)(self.card, 0);
            }
        }
    }
    impl Session for DynamicSession {
        fn transmit(&mut self, apdu: &[u8]) -> Result<Vec<u8>> {
            let send_pci = IoRequest {
                protocol: self.protocol,
                length: std::mem::size_of::<IoRequest>() as Dword,
            };
            let mut receive_pci = IoRequest {
                protocol: 0,
                length: 0,
            };
            let mut response = [0u8; 4096];
            let mut response_length = response.len() as Dword;
            // SAFETY: Input pointers reference live slices/structs, output pointers reference writable
            // stack storage, and the advertised response capacity matches the buffer size.
            let status = unsafe {
                (self.api.transmit)(
                    self.card,
                    &send_pci,
                    apdu.as_ptr(),
                    apdu.len() as Dword,
                    &mut receive_pci,
                    response.as_mut_ptr(),
                    &mut response_length,
                )
            };
            if status == 0 {
                Ok(response[..response_length as usize].to_vec())
            } else {
                Err(format!("PC/SC error: 0x{:08x}", status as u32))
            }
        }
    }

    fn candidates() -> Vec<PathBuf> {
        let Ok(cwd) = std::env::current_dir() else {
            return vec![];
        };
        let Ok(entries) = fs::read_dir(cwd) else {
            return vec![];
        };
        let mut found: Vec<_> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|path| {
                path.is_file()
                    && path
                        .extension()
                        .is_some_and(|ext| ext.eq_ignore_ascii_case("dll"))
            })
            .collect();
        found.sort_by_key(|path| {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            (
                !name.eq_ignore_ascii_case("winscard.dll"),
                name.to_ascii_lowercase(),
            )
        });
        found
    }
}
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
pub struct NativeBackend {
    #[cfg(any(unix, windows))]
    dynamic: Option<dynamic_pcsc::Backend>,
}
impl NativeBackend {
    pub fn detect() -> Self {
        Self {
            #[cfg(any(unix, windows))]
            dynamic: dynamic_pcsc::Backend::detect(),
        }
    }

    #[cfg(any(unix, windows))]
    pub fn detected_library(&self) -> Option<&std::path::Path> {
        self.dynamic.as_ref().map(dynamic_pcsc::Backend::source)
    }
}
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
        #[cfg(any(unix, windows))]
        if let Some(dynamic) = &self.dynamic {
            return dynamic.readers();
        }
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
        #[cfg(any(unix, windows))]
        if let Some(dynamic) = &self.dynamic {
            return dynamic.open(name);
        }
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
