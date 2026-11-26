//! One local endpoint for every plugin, so outside programs reach plugins without each one opening a port:
//! a named pipe on Windows, a Unix socket elsewhere, only usable by the user running Rona.
//!
//! Every frame is a 4 byte little endian length and that many bytes of UTF-8 JSON, the same framing as browser
//! native messaging. A program first sends `{ "rona": 1, "plugin": "<id>", "channel": "..." }` to talk to a
//! plugin, or `{ "rona": 1, "install": "<path>" }` to ask the user to install a plugin. Rona answers with
//! `{ "ok": true, ... }` or `{ "ok": false, "error": "..." }`.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};

use crate::logging::{debug, info, warn};

use super::runtime::{PipeConn, Shared, lock};

pub const MAX_FRAME: usize = 1024 * 1024;
pub const MAX_CONNECTIONS_PER_PLUGIN: usize = 16;
const OUTGOING_QUEUE: usize = 64;
const PROTOCOL_VERSION: u64 = 1;
const ACCEPT_BUDGET: Duration = Duration::from_secs(2);
const REPLY_TIMEOUT: Duration = Duration::from_secs(10);
const STARTUP_WAIT: Duration = Duration::from_secs(20);

pub use platform::{Peer, Stream};

pub fn read_frame(stream: &Stream) -> io::Result<Option<Vec<u8>>> {
    let mut length = [0u8; 4];
    if !read_exact(stream, &mut length)? {
        return Ok(None);
    }
    let length = u32::from_le_bytes(length) as usize;
    if length > MAX_FRAME {
        return Err(io::Error::other(format!("a frame of {length} bytes is over the 1 MiB limit")));
    }
    let mut frame = vec![0; length];
    if !read_exact(stream, &mut frame)? {
        return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
    }
    Ok(Some(frame))
}

// False when the other side closed before anything of this read arrived.
fn read_exact(stream: &Stream, buffer: &mut [u8]) -> io::Result<bool> {
    let mut filled = 0;
    while filled < buffer.len() {
        let read = stream.read(&mut buffer[filled..])?;
        if read == 0 {
            return if filled == 0 { Ok(false) } else { Err(io::Error::from(io::ErrorKind::UnexpectedEof)) };
        }
        filled += read;
    }
    Ok(true)
}

pub fn write_frame(stream: &Stream, frame: &[u8]) -> io::Result<()> {
    let length = u32::try_from(frame.len()).map_err(|_| io::Error::other("frame too large"))?;
    let mut bytes = Vec::with_capacity(frame.len() + 4);
    bytes.extend_from_slice(&length.to_le_bytes());
    bytes.extend_from_slice(frame);
    stream.write_all(&bytes)
}

fn write_json(stream: &Stream, value: &Value) -> io::Result<()> {
    write_frame(stream, value.to_string().as_bytes())
}

struct Connection {
    plugin: String,
    writer: SyncSender<Vec<u8>>,
    stream: Arc<Stream>,
}

static CONNECTIONS: Mutex<Option<HashMap<u32, Connection>>> = Mutex::new(None);
static NEXT_ID: AtomicU32 = AtomicU32::new(1);

fn with_connections<T>(action: impl FnOnce(&mut HashMap<u32, Connection>) -> T) -> T {
    action(lock(&CONNECTIONS).get_or_insert_with(HashMap::new))
}

/// Queues a message for a program connected to `plugin`. A program that stops reading is dropped.
pub fn send(plugin: &str, connection: u32, message: Vec<u8>) -> Result<(), String> {
    if message.len() > MAX_FRAME {
        return Err("A message can be at most 1 MiB".into());
    }
    let writer = with_connections(|connections| match connections.get(&connection) {
        Some(found) if found.plugin == plugin => Ok(found.writer.clone()),
        _ => Err("That connection is closed".to_string()),
    })?;
    match writer.try_send(message) {
        Ok(()) => Ok(()),
        Err(TrySendError::Full(_)) => {
            close(plugin, connection);
            Err("The program stopped reading, so the connection was closed".into())
        }
        Err(TrySendError::Disconnected(_)) => Err("That connection is closed".into()),
    }
}

pub fn close(plugin: &str, connection: u32) {
    let removed = with_connections(|connections| match connections.get(&connection) {
        Some(found) if found.plugin == plugin => connections.remove(&connection),
        _ => None,
    });
    if let Some(removed) = removed {
        removed.stream.shutdown();
    }
}

/// Closes every program's connection to `plugin`, when it stops or loses `ipc.serve`.
pub fn close_all(plugin: &str) {
    let removed: Vec<Connection> = with_connections(|connections| {
        let ids: Vec<u32> = connections.iter().filter(|(_, found)| found.plugin == plugin).map(|(id, _)| *id).collect();
        ids.into_iter().filter_map(|id| connections.remove(&id)).collect()
    });
    for connection in removed {
        connection.stream.shutdown();
    }
}

fn connection_count(plugin: &str) -> usize {
    with_connections(|connections| connections.values().filter(|found| found.plugin == plugin).count())
}

fn spawn_writer(stream: Arc<Stream>, outgoing: Receiver<Vec<u8>>) {
    std::thread::spawn(move || {
        while let Ok(frame) = outgoing.recv() {
            if write_frame(&stream, &frame).is_err() {
                break;
            }
        }
        // The plugin's last messages reach the program before the pipe closes.
        stream.finish();
    });
}

const ENDPOINT: &str = "rona-plugins";

/// Starts listening. Returns false when another Rona window already has the endpoint.
pub fn serve() -> bool {
    serve_on(ENDPOINT)
}

fn serve_on(base: &str) -> bool {
    match platform::Listener::bind(base) {
        Ok(listener) => {
            info!("plugin endpoint is {}", platform::endpoint_name(base));
            std::thread::Builder::new()
                .name("plugin endpoint".into())
                .spawn(move || {
                    loop {
                        match listener.accept() {
                            Ok(stream) => {
                                std::thread::spawn(move || handle(Arc::new(stream)));
                            }
                            Err(err) => {
                                warn!("stopped listening on the plugin endpoint: {err}");
                                return;
                            }
                        }
                    }
                })
                .is_ok()
        }
        Err(err) => {
            warn!("no plugin endpoint for this window: {err}");
            false
        }
    }
}

fn refuse(stream: &Stream, error: &str) {
    debug!("plugin endpoint refused a program: {error}");
    let _ = write_json(stream, &json!({ "ok": false, "error": error }));
    stream.finish();
}

fn handle(stream: Arc<Stream>) {
    let peer = stream.peer();
    if !peer.same_user {
        return refuse(&stream, "Only programs of the user running Rona may connect");
    }
    let hello = match read_frame(&stream) {
        Ok(Some(frame)) => frame,
        Ok(None) => return,
        Err(err) => return refuse(&stream, &err.to_string()),
    };
    let Ok(hello) = serde_json::from_slice::<Value>(&hello) else { return refuse(&stream, "The first message must be JSON") };
    if hello.get("rona").and_then(Value::as_u64) != Some(PROTOCOL_VERSION) {
        return refuse(&stream, "Start with {\"rona\": 1, ...}");
    }
    if let Some(path) = hello.get("install").and_then(Value::as_str) {
        let via = hello.get("via").and_then(Value::as_str).filter(|_| peer.is_rona()).map(str::to_owned);
        let requester = super::Requester { program: via.unwrap_or(peer.program.clone()) };
        let reply = match super::request_install(std::path::PathBuf::from(path), requester) {
            Ok(result) => json!({ "ok": true, "result": result }),
            Err(error) => json!({ "ok": false, "error": error }),
        };
        let _ = write_json(&stream, &reply);
        stream.finish();
        return;
    }
    let Some(plugin) = hello.get("plugin").and_then(Value::as_str).map(str::to_owned) else {
        return refuse(&stream, "Name a plugin, or a file to install");
    };
    let channel: String = hello.get("channel").and_then(Value::as_str).unwrap_or_default().chars().take(200).collect();
    serve_plugin(stream, peer, plugin, channel);
}

fn serve_plugin(stream: Arc<Stream>, peer: Peer, plugin: String, channel: String) {
    let shared = match super::serving_plugin(&plugin) {
        Ok(shared) => shared,
        Err(error) => return refuse(&stream, &error),
    };
    if connection_count(&plugin) >= MAX_CONNECTIONS_PER_PLUGIN {
        return refuse(&stream, "The plugin has too many programs connected");
    }
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let client = json!({ "connection": id, "pid": peer.pid, "program": peer.program, "channel": channel });
    if shared.manifest.has_hook(super::manifest::HOOK_IPC_ACCEPT) {
        let accepted = shared
            .call_and_wait(super::manifest::HOOK_IPC_ACCEPT, json!({ "client": client }), ACCEPT_BUDGET, false)
            .ok()
            .and_then(|output| serde_json::from_slice::<Value>(&output).ok())
            .and_then(|value| value.as_bool())
            .unwrap_or(false);
        if !accepted {
            return refuse(&stream, "The plugin turned the connection down");
        }
    }

    let (writer, outgoing) = sync_channel(OUTGOING_QUEUE);
    with_connections(|connections| connections.insert(id, Connection { plugin: plugin.clone(), writer, stream: stream.clone() }));
    if write_json(&stream, &json!({ "ok": true, "connection": id })).is_err() {
        close(&plugin, id);
        return;
    }
    spawn_writer(stream.clone(), outgoing);
    info!("{} connected to plugin {plugin} as {id}", if peer.program.is_empty() { "a program" } else { &peer.program });
    shared.post_event(json!({ "type": "ipc-connected", "connection": id, "client": client }));

    loop {
        let frame = match read_frame(&stream) {
            Ok(Some(frame)) => frame,
            Ok(None) => break,
            Err(err) => {
                debug!("plugin connection {id} ended: {err}");
                break;
            }
        };
        let Ok(message) = serde_json::from_slice::<Value>(&frame) else {
            let _ = write_json(&stream, &json!({ "ok": false, "error": "Messages must be JSON" }));
            break;
        };
        if !shared.granted(super::permissions::IPC_SERVE) {
            break;
        }
        shared.post_event(json!({ "type": "ipc-message", "connection": id, "message": message }));
    }
    let was_open = with_connections(|connections| connections.remove(&id).is_some());
    stream.shutdown();
    if was_open {
        shared.post_event(json!({ "type": "ipc-closed", "connection": id }));
    }
}

/// Opens one of the pipes the plugin's manifest names, for `ipc.connect`.
pub fn connect_pipe(shared: &Arc<Shared>, name: &str, max_buffer: usize) -> Result<u32, String> {
    let stream = Arc::new(platform::connect_named(name).map_err(|err| format!("Couldn't open {name}: {err}"))?);
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let (writer, outgoing) = std::sync::mpsc::channel::<Vec<u8>>();
    let received = Arc::new(Mutex::new(Vec::new()));
    let told = Arc::new(AtomicBool::new(false));
    lock(&shared.pipes).insert(id, PipeConn { name: name.to_owned(), writer, received: received.clone(), told: told.clone() });

    let write_stream = stream.clone();
    std::thread::spawn(move || {
        while let Ok(bytes) = outgoing.recv() {
            if write_stream.write_all(&bytes).is_err() {
                break;
            }
        }
        write_stream.shutdown();
    });

    let weak = Arc::downgrade(shared);
    std::thread::spawn(move || {
        let mut buffer = vec![0u8; 64 * 1024];
        loop {
            let read = match stream.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(read) => read,
            };
            let Some(shared) = weak.upgrade() else { break };
            {
                let mut received = lock(&received);
                if received.len() + read > max_buffer {
                    shared.log.push("warn", "A pipe sent more than the plugin read, so it was closed");
                    drop(received);
                    lock(&shared.pipes).remove(&id);
                    break;
                }
                received.extend_from_slice(&buffer[..read]);
            }
            if !told.swap(true, Ordering::SeqCst) && !shared.post_event(json!({ "type": "pipe-data", "connection": id })) {
                told.store(false, Ordering::SeqCst);
            }
        }
        stream.shutdown();
        if let Some(shared) = weak.upgrade()
            && lock(&shared.pipes).remove(&id).is_some()
        {
            shared.post_event(json!({ "type": "pipe-closed", "connection": id }));
        }
    });
    Ok(id)
}

fn hello(stream: &Stream, hello: &Value) -> Result<Value, String> {
    write_json(stream, hello).map_err(|err| err.to_string())?;
    let reply = read_frame(stream).map_err(|err| err.to_string())?.ok_or("Rona closed the connection")?;
    let reply: Value = serde_json::from_slice(&reply).map_err(|_| "Rona sent something that isn't JSON".to_string())?;
    if reply.get("ok").and_then(Value::as_bool) == Some(true) {
        Ok(reply)
    } else {
        Err(reply.get("error").and_then(Value::as_str).unwrap_or("Rona refused").to_owned())
    }
}

/// `rona --plugin-send <plugin> <json>`: sends one message and returns the first answer.
pub fn send_once(plugin: &str, message: &str) -> Result<String, String> {
    let message: Value = serde_json::from_str(message).map_err(|_| "The message must be JSON".to_string())?;
    let stream = Arc::new(platform::connect_endpoint(ENDPOINT).map_err(|_| "Rona isn't running".to_string())?);
    hello(&stream, &json!({ "rona": PROTOCOL_VERSION, "plugin": plugin, "channel": "rona --plugin-send" }))?;
    write_json(&stream, &message).map_err(|err| err.to_string())?;
    let (answer, answered) = std::sync::mpsc::channel();
    let reader = stream.clone();
    std::thread::spawn(move || {
        let _ = answer.send(read_frame(&reader));
    });
    let result = match answered.recv_timeout(REPLY_TIMEOUT) {
        Ok(Ok(Some(frame))) => Ok(String::from_utf8_lossy(&frame).into_owned()),
        Ok(Ok(None)) => Ok(String::new()),
        Ok(Err(err)) => Err(err.to_string()),
        Err(_) => Ok(String::new()),
    };
    stream.shutdown();
    result
}

/// `rona --native-messaging <plugin> [origin]`: passes a browser extension's native messages to a plugin and back.
/// Both sides use the same framing, so frames go through unchanged.
pub fn native_messaging(plugin: &str, origin: &str) -> Result<(), String> {
    let stream = Arc::new(platform::connect_endpoint(ENDPOINT).map_err(|_| "Rona isn't running".to_string())?);
    hello(&stream, &json!({ "rona": PROTOCOL_VERSION, "plugin": plugin, "channel": origin }))?;
    let upstream = stream.clone();
    std::thread::spawn(move || {
        let mut stdin = io::stdin().lock();
        loop {
            let mut length = [0u8; 4];
            if stdin.read_exact(&mut length).is_err() {
                break;
            }
            let length = u32::from_le_bytes(length) as usize;
            if length > MAX_FRAME {
                break;
            }
            let mut frame = vec![0; length];
            if stdin.read_exact(&mut frame).is_err() || write_frame(&upstream, &frame).is_err() {
                break;
            }
        }
        upstream.shutdown();
    });
    let mut stdout = io::stdout().lock();
    while let Ok(Some(frame)) = read_frame(&stream) {
        let length = (frame.len() as u32).to_le_bytes();
        if stdout.write_all(&length).and_then(|()| stdout.write_all(&frame)).and_then(|()| stdout.flush()).is_err() {
            break;
        }
    }
    Ok(())
}

/// `rona --install-plugin <path>`: asks the running Rona to install a plugin, starting one when none runs,
/// and waits for the user's answer.
pub fn request_install(path: &str) -> Result<String, String> {
    let path = std::path::absolute(path).map_err(|err| err.to_string())?;
    if !path.is_file() {
        return Err(format!("{} doesn't exist", path.display()));
    }
    let stream = match platform::connect_endpoint(ENDPOINT) {
        Ok(stream) => stream,
        Err(_) => start_rona_and_connect()?,
    };
    let reply = hello(
        &stream,
        &json!({ "rona": PROTOCOL_VERSION, "install": path.to_string_lossy(), "via": platform::parent_program() }),
    )?;
    Ok(reply.get("result").and_then(Value::as_str).unwrap_or("failed").to_owned())
}

fn start_rona_and_connect() -> Result<Stream, String> {
    let exe = std::env::current_exe().map_err(|err| err.to_string())?;
    std::process::Command::new(exe).spawn().map_err(|err| format!("Couldn't start Rona: {err}"))?;
    let started = std::time::Instant::now();
    while started.elapsed() < STARTUP_WAIT {
        std::thread::sleep(Duration::from_millis(250));
        if let Ok(stream) = platform::connect_endpoint(ENDPOINT) {
            return Ok(stream);
        }
    }
    Err("Rona didn't start in time".into())
}

#[cfg(test)]
pub fn tests_serve(base: &str) -> bool {
    serve_on(base)
}

#[cfg(test)]
pub fn tests_connect(base: &str) -> Stream {
    platform::connect_endpoint(base).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint(test: &str) -> String {
        let base = format!("rona-test-{test}-{}", std::process::id());
        assert!(serve_on(&base));
        base
    }

    fn ask(base: &str, frame: &[u8]) -> Value {
        let stream = platform::connect_endpoint(base).unwrap();
        stream.write_all(frame).unwrap();
        let reply = read_frame(&stream).unwrap().expect("an answer");
        serde_json::from_slice(&reply).unwrap()
    }

    fn framed(json: &Value) -> Vec<u8> {
        let text = json.to_string();
        [&(text.len() as u32).to_le_bytes()[..], text.as_bytes()].concat()
    }

    #[test]
    fn a_program_naming_a_plugin_that_doesnt_run_is_refused() {
        let base = endpoint("refuse");
        let reply = ask(&base, &framed(&json!({ "rona": 1, "plugin": "com.example.missing" })));
        assert_eq!(reply["ok"], json!(false));
        assert!(reply["error"].as_str().is_some_and(|error| !error.is_empty()));
    }

    #[test]
    fn a_first_message_that_isnt_a_hello_is_refused() {
        let base = endpoint("hello");
        assert_eq!(ask(&base, &framed(&json!({ "plugin": "a" })))["ok"], json!(false));
        assert_eq!(ask(&base, &[3, 0, 0, 0, b'n', b'o', b'!'])["ok"], json!(false));
    }

    #[test]
    fn a_frame_over_the_limit_is_refused_before_it_is_read() {
        let base = endpoint("limit");
        let length = (MAX_FRAME as u32 + 1).to_le_bytes();
        let reply = ask(&base, &length);
        assert_eq!(reply["ok"], json!(false));
        assert!(reply["error"].as_str().unwrap().contains("1 MiB"));
    }

    // A read waiting on one thread must not hold up a write on another, or a plugin couldn't answer a program
    // that is quiet. On Windows that needs overlapped I/O on the pipe.
    #[test]
    fn a_waiting_read_doesnt_block_a_write_on_the_same_pipe() {
        let base = format!("rona-test-duplex-{}", std::process::id());
        let listener = platform::Listener::bind(&base).unwrap();
        let client = std::thread::spawn({
            let base = base.clone();
            move || {
                let stream = platform::connect_endpoint(&base).unwrap();
                let frame = read_frame(&stream).unwrap().unwrap();
                write_frame(&stream, b"bye").unwrap();
                frame
            }
        });
        let server = Arc::new(listener.accept().unwrap());
        let reader = server.clone();
        let reading = std::thread::spawn(move || read_frame(&reader).unwrap().unwrap());
        std::thread::sleep(Duration::from_millis(100));
        write_frame(&server, b"hello").unwrap();
        assert_eq!(client.join().unwrap(), b"hello");
        assert_eq!(reading.join().unwrap(), b"bye");
    }
}

#[cfg(windows)]
mod platform {
    use std::io;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    use windows::Win32::Foundation::{
        CloseHandle, ERROR_BROKEN_PIPE, ERROR_IO_PENDING, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, GENERIC_READ,
        GENERIC_WRITE, HANDLE, HLOCAL, LocalFree,
    };
    use windows::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows::Win32::Security::{
        GetTokenInformation, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER, TokenUser,
    };
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, FILE_SHARE_NONE, FlushFileBuffers,
        OPEN_EXISTING, PIPE_ACCESS_DUPLEX, ReadFile, WriteFile,
    };
    use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
    use windows::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, GetNamedPipeClientProcessId, PIPE_READMODE_BYTE,
        PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT, WaitNamedPipeW,
    };
    use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
    use windows::Win32::System::Threading::{
        CreateEventW, GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_NAME_WIN32,
        PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
    };
    use windows::core::{PCWSTR, PWSTR};

    const BUFFER_SIZE: u32 = 64 * 1024;

    struct Owned(HANDLE);

    // Overlapped I/O on a handle is safe from several threads at once, each with its own OVERLAPPED.
    unsafe impl Send for Owned {}
    unsafe impl Sync for Owned {}

    impl Drop for Owned {
        fn drop(&mut self) {
            let _ = unsafe { CloseHandle(self.0) };
        }
    }

    pub struct Peer {
        pub pid: u32,
        pub program: String,
        pub same_user: bool,
    }

    impl Peer {
        pub fn is_rona(&self) -> bool {
            let own = std::env::current_exe().map(|path| path.to_string_lossy().to_lowercase()).unwrap_or_default();
            !own.is_empty() && self.program.to_lowercase() == own
        }
    }

    /// One end of a pipe, read and written from different threads at once.
    pub struct Stream {
        handle: Owned,
        server: bool,
        closed: AtomicBool,
    }

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain([0]).collect()
    }

    fn overlapped(handle: HANDLE, start: impl FnOnce(*mut OVERLAPPED) -> windows::core::Result<()>) -> io::Result<u32> {
        let event = unsafe { CreateEventW(None, true, false, PCWSTR::null()) }?;
        let event = Owned(event);
        let mut state = OVERLAPPED { hEvent: event.0, ..Default::default() };
        match start(&mut state) {
            Ok(()) => {}
            Err(err) if err.code() == ERROR_IO_PENDING.to_hresult() => {}
            Err(err) if err.code() == ERROR_BROKEN_PIPE.to_hresult() => return Ok(0),
            Err(err) if err.code() == ERROR_PIPE_CONNECTED.to_hresult() => return Ok(0),
            Err(err) => return Err(err.into()),
        }
        let mut transferred = 0;
        match unsafe { GetOverlappedResult(handle, &state, &mut transferred, true) } {
            Ok(()) => Ok(transferred),
            Err(err) if err.code() == ERROR_BROKEN_PIPE.to_hresult() => Ok(0),
            Err(err) => Err(err.into()),
        }
    }

    impl Stream {
        pub fn read(&self, buffer: &mut [u8]) -> io::Result<usize> {
            if self.closed.load(Ordering::SeqCst) {
                return Ok(0);
            }
            let handle = self.handle.0;
            overlapped(handle, |state| unsafe { ReadFile(handle, Some(buffer), None, Some(state)) }).map(|read| read as usize)
        }

        pub fn write_all(&self, mut bytes: &[u8]) -> io::Result<()> {
            let handle = self.handle.0;
            while !bytes.is_empty() {
                if self.closed.load(Ordering::SeqCst) {
                    return Err(io::Error::from(io::ErrorKind::BrokenPipe));
                }
                let written = overlapped(handle, |state| unsafe { WriteFile(handle, Some(bytes), None, Some(state)) })?;
                if written == 0 {
                    return Err(io::Error::from(io::ErrorKind::BrokenPipe));
                }
                bytes = &bytes[written as usize..];
            }
            Ok(())
        }

        /// Ends both directions. Reads and writes in other threads return, and what the other side hasn't read
        /// yet is lost.
        pub fn shutdown(&self) {
            if self.closed.swap(true, Ordering::SeqCst) {
                return;
            }
            let _ = unsafe { CancelIoEx(self.handle.0, None) };
            if self.server {
                let _ = unsafe { DisconnectNamedPipe(self.handle.0) };
            }
        }

        /// Waits until the other side has read everything written, then ends. Disconnecting a pipe at once throws
        /// away what is still in it, like a refusal's reason. Blocks, so only on the connection's own thread.
        pub fn finish(&self) {
            if !self.closed.load(Ordering::SeqCst) {
                let _ = unsafe { FlushFileBuffers(self.handle.0) };
            }
            self.shutdown();
        }

        pub fn peer(&self) -> Peer {
            let mut pid = 0;
            if unsafe { GetNamedPipeClientProcessId(self.handle.0, &mut pid) }.is_err() {
                return Peer { pid: 0, program: String::new(), same_user: true };
            }
            // The pipe's security descriptor already lets only this user connect.
            Peer { pid, program: program_of(pid), same_user: true }
        }
    }

    pub fn program_of(pid: u32) -> String {
        let Ok(process) = (unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }) else { return String::new() };
        let process = Owned(process);
        let mut buffer = vec![0u16; 1024];
        let mut size = buffer.len() as u32;
        match unsafe { QueryFullProcessImageNameW(process.0, PROCESS_NAME_WIN32, PWSTR(buffer.as_mut_ptr()), &mut size) } {
            Ok(()) => String::from_utf16_lossy(&buffer[..size as usize]),
            Err(_) => String::new(),
        }
    }

    pub fn parent_program() -> String {
        use windows::Win32::System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
        };
        let own = std::process::id();
        let Ok(snapshot) = (unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }) else { return String::new() };
        let snapshot = Owned(snapshot);
        let mut entry = PROCESSENTRY32W { dwSize: size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        let mut found = unsafe { Process32FirstW(snapshot.0, &mut entry) }.is_ok();
        while found {
            if entry.th32ProcessID == own {
                return program_of(entry.th32ParentProcessID);
            }
            found = unsafe { Process32NextW(snapshot.0, &mut entry) }.is_ok();
        }
        String::new()
    }

    pub fn endpoint_name(base: &str) -> String {
        let mut session = 0;
        let _ = unsafe { ProcessIdToSessionId(std::process::id(), &mut session) };
        format!(r"\\.\pipe\{base}-{session}")
    }

    fn user_sid() -> io::Result<String> {
        let mut token = HANDLE::default();
        unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }?;
        let token = Owned(token);
        let mut size = 0;
        let _ = unsafe { GetTokenInformation(token.0, TokenUser, None, 0, &mut size) };
        let mut buffer = vec![0u8; size as usize];
        unsafe { GetTokenInformation(token.0, TokenUser, Some(buffer.as_mut_ptr().cast()), size, &mut size) }?;
        let user = unsafe { &*(buffer.as_ptr() as *const TOKEN_USER) };
        let mut text = PWSTR::null();
        unsafe { ConvertSidToStringSidW(user.User.Sid, &mut text) }?;
        let sid = unsafe { text.to_string() }.map_err(io::Error::other);
        let _ = unsafe { LocalFree(Some(HLOCAL(text.0.cast()))) };
        sid
    }

    struct Descriptor(PSECURITY_DESCRIPTOR);

    impl Drop for Descriptor {
        fn drop(&mut self) {
            let _ = unsafe { LocalFree(Some(HLOCAL(self.0.0))) };
        }
    }

    // Only this user and the system get access, so another user on the same machine can't connect.
    fn descriptor() -> io::Result<Descriptor> {
        let sddl = wide(&format!("D:P(A;;GA;;;{})(A;;GA;;;SY)", user_sid()?));
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(PCWSTR(sddl.as_ptr()), SDDL_REVISION_1, &mut descriptor, None)
        }?;
        Ok(Descriptor(descriptor))
    }

    fn create(name: &[u16], first: bool) -> io::Result<Owned> {
        let descriptor = descriptor()?;
        let attributes = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor.0.0,
            bInheritHandle: false.into(),
        };
        let mut access = PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED;
        if first {
            access |= FILE_FLAG_FIRST_PIPE_INSTANCE;
        }
        let pipe = unsafe {
            CreateNamedPipeW(
                PCWSTR(name.as_ptr()),
                access,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                PIPE_UNLIMITED_INSTANCES,
                BUFFER_SIZE,
                BUFFER_SIZE,
                0,
                Some(&attributes),
            )
        };
        if pipe.is_invalid() {
            return Err(io::Error::last_os_error());
        }
        Ok(Owned(pipe))
    }

    pub struct Listener {
        name: Vec<u16>,
        next: Mutex<Option<Owned>>,
    }

    impl Listener {
        pub fn bind(base: &str) -> io::Result<Self> {
            let name = wide(&endpoint_name(base));
            let first = create(&name, true)?;
            Ok(Self { name, next: Mutex::new(Some(first)) })
        }

        // The next pipe instance exists before a connection is handed over, so a slow program holds up nobody.
        pub fn accept(&self) -> io::Result<Stream> {
            let pipe = match self.next.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take() {
                Some(pipe) => pipe,
                None => create(&self.name, false)?,
            };
            let handle = pipe.0;
            overlapped(handle, |state| unsafe { ConnectNamedPipe(handle, Some(state)) })?;
            *self.next.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = create(&self.name, false).ok();
            Ok(Stream { handle: pipe, server: true, closed: AtomicBool::new(false) })
        }
    }

    fn open(path: &str) -> io::Result<Stream> {
        let wide_path = wide(path);
        for _ in 0..5 {
            let opened = unsafe {
                CreateFileW(
                    PCWSTR(wide_path.as_ptr()),
                    (GENERIC_READ | GENERIC_WRITE).0,
                    FILE_SHARE_NONE,
                    None,
                    OPEN_EXISTING,
                    FILE_FLAG_OVERLAPPED,
                    None,
                )
            };
            match opened {
                Ok(handle) => return Ok(Stream { handle: Owned(handle), server: false, closed: AtomicBool::new(false) }),
                Err(err) if err.code() == ERROR_PIPE_BUSY.to_hresult() => {
                    let _ = unsafe { WaitNamedPipeW(PCWSTR(wide_path.as_ptr()), 1000) };
                }
                Err(err) => return Err(err.into()),
            }
        }
        Err(io::Error::other("the pipe is busy"))
    }

    pub fn connect_endpoint(base: &str) -> io::Result<Stream> {
        open(&endpoint_name(base))
    }

    pub fn connect_named(name: &str) -> io::Result<Stream> {
        open(&format!(r"\\.\pipe\{name}"))
    }
}

#[cfg(unix)]
mod platform {
    use std::fs::{File, OpenOptions, TryLockError};
    use std::io::{self, Read, Write};
    use std::net::Shutdown;
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::PathBuf;

    pub struct Peer {
        pub pid: u32,
        pub program: String,
        pub same_user: bool,
    }

    impl Peer {
        pub fn is_rona(&self) -> bool {
            let own = std::env::current_exe().map(|path| path.to_string_lossy().into_owned()).unwrap_or_default();
            !own.is_empty() && self.program == own
        }
    }

    pub struct Stream(UnixStream);

    impl Stream {
        pub fn read(&self, buffer: &mut [u8]) -> io::Result<usize> {
            (&self.0).read(buffer)
        }

        pub fn write_all(&self, bytes: &[u8]) -> io::Result<()> {
            (&self.0).write_all(bytes)
        }

        pub fn shutdown(&self) {
            let _ = self.0.shutdown(Shutdown::Both);
        }

        // A socket keeps what was written readable after a shutdown, so there is nothing to wait for.
        pub fn finish(&self) {
            self.shutdown();
        }

        #[cfg(target_os = "linux")]
        pub fn peer(&self) -> Peer {
            use std::os::fd::AsRawFd;
            let mut credentials = libc::ucred { pid: 0, uid: u32::MAX, gid: 0 };
            let mut size = size_of::<libc::ucred>() as libc::socklen_t;
            let result = unsafe {
                libc::getsockopt(
                    self.0.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_PEERCRED,
                    (&mut credentials as *mut libc::ucred).cast(),
                    &mut size,
                )
            };
            let pid = if result == 0 { credentials.pid.max(0) as u32 } else { 0 };
            Peer { pid, program: program_of(pid), same_user: result == 0 && credentials.uid == unsafe { libc::getuid() } }
        }

        #[cfg(not(target_os = "linux"))]
        pub fn peer(&self) -> Peer {
            use std::os::fd::AsRawFd;
            let (mut uid, mut gid) = (u32::MAX, 0);
            let result = unsafe { libc::getpeereid(self.0.as_raw_fd(), &mut uid, &mut gid) };
            Peer { pid: 0, program: String::new(), same_user: result == 0 && uid == unsafe { libc::getuid() } }
        }
    }

    fn program_of(pid: u32) -> String {
        if pid == 0 {
            return String::new();
        }
        std::fs::read_link(format!("/proc/{pid}/exe")).map(|path| path.to_string_lossy().into_owned()).unwrap_or_default()
    }

    pub fn parent_program() -> String {
        let stat = std::fs::read_to_string("/proc/self/stat").unwrap_or_default();
        // The process name in brackets can hold spaces, so fields are counted after its closing bracket.
        let parent = stat.rsplit_once(')').and_then(|(_, rest)| rest.split_whitespace().nth(1)).and_then(|pid| pid.parse().ok());
        parent.map(program_of).unwrap_or_default()
    }

    fn runtime_path(name: &str) -> PathBuf {
        match std::env::var_os("XDG_RUNTIME_DIR") {
            Some(dir) => PathBuf::from(dir).join(name),
            None => std::env::temp_dir().join(format!("{name}-{}", unsafe { libc::getuid() })),
        }
    }

    pub fn endpoint_name(base: &str) -> String {
        runtime_path(&format!("{base}.sock")).to_string_lossy().into_owned()
    }

    pub struct Listener {
        listener: UnixListener,
        _lock: File,
    }

    impl Listener {
        pub fn bind(base: &str) -> io::Result<Self> {
            let lock = OpenOptions::new().create(true).write(true).truncate(false).open(runtime_path(&format!("{base}.lock")))?;
            match lock.try_lock() {
                Ok(()) => {}
                Err(TryLockError::WouldBlock) => return Err(io::Error::other("another Rona window has it")),
                Err(TryLockError::Error(err)) => return Err(err),
            }
            // Holding the lock means any socket left there belongs to a window that crashed.
            let socket = runtime_path(&format!("{base}.sock"));
            match std::fs::remove_file(&socket) {
                Err(err) if err.kind() != io::ErrorKind::NotFound => return Err(err),
                _ => {}
            }
            Ok(Self { listener: UnixListener::bind(socket)?, _lock: lock })
        }

        pub fn accept(&self) -> io::Result<Stream> {
            self.listener.accept().map(|(stream, _)| Stream(stream))
        }
    }

    pub fn connect_endpoint(base: &str) -> io::Result<Stream> {
        UnixStream::connect(runtime_path(&format!("{base}.sock"))).map(Stream)
    }

    pub fn connect_named(name: &str) -> io::Result<Stream> {
        UnixStream::connect(runtime_path(name)).map(Stream)
    }
}
