use std::io::{self, Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

const NAME: &str = "rona";
// The running window can be closing when another launch reaches it, so the claim is tried again.
const ATTEMPTS: usize = 3;
const RETRY_DELAY: Duration = Duration::from_millis(200);
const MAX_MESSAGE: u32 = 16 * 1024 * 1024;
const ACCEPTED: u8 = 1;

pub enum Claim {
    Forwarded,
    Primary(Server),
    Alone,
}

pub struct Launch {
    pub sources: Vec<String>,
    // Wayland only lets a window come to the front with the token its launcher gave the new process.
    pub activation_token: Option<String>,
}

pub use platform::Server;

type OnMessage = Arc<dyn Fn(Vec<u8>) -> bool + Send + Sync>;

/// Hands the sources to the window already running, or makes this process the one later launches hand theirs to.
pub fn claim(sources: &[String]) -> Claim {
    let launch =
        Launch { sources: sources.to_vec(), activation_token: std::env::var("XDG_ACTIVATION_TOKEN").ok() };
    claim_named(NAME, &launch)
}

fn claim_named(name: &str, launch: &Launch) -> Claim {
    let message = encode(launch);
    for attempt in 0..ATTEMPTS {
        if attempt > 0 {
            std::thread::sleep(RETRY_DELAY);
        }
        match platform::try_serve(name) {
            Ok(Some(server)) => return Claim::Primary(server),
            Ok(None) => {}
            Err(err) => return alone(err),
        }
        let connection = match platform::connect(name) {
            Ok(Some(connection)) => connection,
            Ok(None) => continue,
            Err(err) => return alone(err),
        };
        match hand_over(connection, &message) {
            Ok(true) => return Claim::Forwarded,
            Ok(false) => {}
            Err(err) => return alone(err),
        }
    }
    alone(io::Error::other("the running window didn't take the files"))
}

fn alone(err: io::Error) -> Claim {
    eprintln!("running as a separate window: {err}");
    Claim::Alone
}

// The sender only exits once the window confirms, so files sent while it closes aren't lost.
fn hand_over(mut connection: impl Read + Write, message: &[u8]) -> io::Result<bool> {
    let length = u32::try_from(message.len()).map_err(|_| io::Error::other("too many files"))?;
    connection.write_all(&length.to_le_bytes())?;
    connection.write_all(message)?;
    let mut reply = [0];
    match connection.read_exact(&mut reply) {
        Ok(()) => Ok(reply[0] == ACCEPTED),
        Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => Ok(false),
        Err(err) => Err(err),
    }
}

fn answer(mut connection: impl Read + Write, on_message: &OnMessage) {
    let mut length = [0; 4];
    let message = connection.read_exact(&mut length).and_then(|()| {
        let length = u32::from_le_bytes(length);
        if length > MAX_MESSAGE {
            return Err(io::Error::other(format!("message of {length} bytes")));
        }
        let mut message = vec![0; length as usize];
        connection.read_exact(&mut message)?;
        Ok(message)
    });
    match message {
        Ok(message) => {
            let reply = if on_message(message) { ACCEPTED } else { 0 };
            if let Err(err) = connection.write_all(&[reply]) {
                eprintln!("could not answer another launch: {err}");
            }
        }
        Err(err) => eprintln!("could not read the files from another launch: {err}"),
    }
}

/// Once stopped, later launches get no confirmation and open their own window.
pub struct Listener(Arc<AtomicBool>);

impl Listener {
    pub fn stop(&self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// `on_launch` returns whether the window took the launch.
pub fn listen(server: Server, on_launch: impl Fn(Launch) -> bool + Send + Sync + 'static) -> Listener {
    let stopped = Arc::new(AtomicBool::new(false));
    let listening = stopped.clone();
    let on_message: OnMessage =
        Arc::new(move |message| !listening.load(Ordering::Relaxed) && on_launch(decode(&message)));
    std::thread::spawn(move || server.serve(on_message));
    Listener(stopped)
}

// The token comes first, then the sources, all NUL separated since no path or URL contains NUL.
fn encode(launch: &Launch) -> Vec<u8> {
    let mut fields = vec![launch.activation_token.clone().unwrap_or_default()];
    fields.extend(launch.sources.iter().cloned());
    fields.join("\0").into_bytes()
}

fn decode(message: &[u8]) -> Launch {
    let text = String::from_utf8_lossy(message);
    let mut fields = text.split('\0').map(str::to_string);
    let activation_token = fields.next().filter(|token| !token.is_empty());
    Launch { sources: fields.collect(), activation_token }
}

#[cfg(windows)]
mod platform {
    use std::fs::{File, OpenOptions};
    use std::io;
    use std::os::windows::io::{AsRawHandle, FromRawHandle};

    use windows::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, HANDLE, INVALID_HANDLE_VALUE};
    use windows::Win32::Storage::FileSystem::{FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX};
    use windows::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, GetNamedPipeServerProcessId, PIPE_READMODE_BYTE,
        PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT, WaitNamedPipeW,
    };
    use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
    use windows::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow;
    use windows::core::PCWSTR;

    use super::OnMessage;

    const BUFFER_SIZE: u32 = 64 * 1024;
    const BUSY_ATTEMPTS: usize = 5;
    const BUSY_WAIT_MS: u32 = 1000;

    pub struct Server {
        name: Vec<u16>,
        pipe: File,
    }

    // Pipe names are machine wide, so each session gets its own window.
    fn pipe_name(name: &str) -> io::Result<String> {
        let mut session = 0;
        unsafe { ProcessIdToSessionId(std::process::id(), &mut session) }?;
        Ok(format!(r"\\.\pipe\{name}-{session}"))
    }

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain([0]).collect()
    }

    fn handle(file: &File) -> HANDLE {
        HANDLE(file.as_raw_handle())
    }

    fn create(name: &[u16], first: bool) -> io::Result<File> {
        let access = if first { PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE } else { PIPE_ACCESS_DUPLEX };
        let pipe = unsafe {
            CreateNamedPipeW(
                PCWSTR(name.as_ptr()),
                access,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                PIPE_UNLIMITED_INSTANCES,
                BUFFER_SIZE,
                BUFFER_SIZE,
                0,
                None,
            )
        };
        if pipe == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        Ok(unsafe { File::from_raw_handle(pipe.0) })
    }

    pub fn try_serve(name: &str) -> io::Result<Option<Server>> {
        let name = wide(&pipe_name(name)?);
        match create(&name, true) {
            Ok(pipe) => Ok(Some(Server { name, pipe })),
            // What FILE_FLAG_FIRST_PIPE_INSTANCE gives when another process already made the pipe.
            Err(err) if err.raw_os_error() == Some(ERROR_ACCESS_DENIED.0 as i32) => Ok(None),
            Err(err) => Err(err),
        }
    }

    pub fn connect(name: &str) -> io::Result<Option<File>> {
        let name = pipe_name(name)?;
        let wide_name = wide(&name);
        for _ in 0..BUSY_ATTEMPTS {
            match OpenOptions::new().read(true).write(true).open(&name) {
                Ok(pipe) => {
                    let mut server = 0;
                    // Only the process the user just started may take the foreground, so it hands that on.
                    if unsafe { GetNamedPipeServerProcessId(handle(&pipe), &mut server) }.is_ok() {
                        let _ = unsafe { AllowSetForegroundWindow(server) };
                    }
                    return Ok(Some(pipe));
                }
                Err(err) if err.raw_os_error() == Some(ERROR_PIPE_BUSY.0 as i32) => {
                    let _ = unsafe { WaitNamedPipeW(PCWSTR(wide_name.as_ptr()), BUSY_WAIT_MS) };
                }
                Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
                Err(err) => return Err(err),
            }
        }
        Err(io::Error::other("the running window is busy"))
    }

    impl Server {
        // The next pipe exists before a connection is read, so a launch that never finishes sending only holds up its own.
        pub fn serve(self, on_message: OnMessage) {
            let Server { name, mut pipe } = self;
            loop {
                if let Err(err) = unsafe { ConnectNamedPipe(handle(&pipe), None) }
                    && err.code() != ERROR_PIPE_CONNECTED.to_hresult()
                {
                    eprintln!("stopped listening for other launches: {err}");
                    return;
                }
                let next = create(&name, false);
                let on_message = on_message.clone();
                std::thread::spawn(move || super::answer(pipe, &on_message));
                match next {
                    Ok(next) => pipe = next,
                    Err(err) => {
                        eprintln!("stopped listening for other launches: {err}");
                        return;
                    }
                }
            }
        }
    }
}

#[cfg(unix)]
mod platform {
    use std::fs::{File, OpenOptions, TryLockError};
    use std::io;
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::PathBuf;
    use std::time::Duration;

    use super::OnMessage;

    const CONNECT_ATTEMPTS: usize = 20;
    const CONNECT_RETRY: Duration = Duration::from_millis(100);

    pub struct Server {
        listener: UnixListener,
        lock: File,
    }

    // XDG_RUNTIME_DIR belongs to the user. The shared temp folder needs the user id in the name.
    fn path(name: &str, extension: &str) -> PathBuf {
        match std::env::var_os("XDG_RUNTIME_DIR") {
            Some(dir) => PathBuf::from(dir).join(format!("{name}.{extension}")),
            None => std::env::temp_dir().join(format!("{name}-{}.{extension}", unsafe { libc::getuid() })),
        }
    }

    pub fn try_serve(name: &str) -> io::Result<Option<Server>> {
        let lock = OpenOptions::new().create(true).write(true).truncate(false).open(path(name, "lock"))?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Ok(None),
            Err(TryLockError::Error(err)) => return Err(err),
        }
        // Holding the lock means any socket left there belongs to a window that crashed.
        let socket = path(name, "sock");
        match std::fs::remove_file(&socket) {
            Err(err) if err.kind() != io::ErrorKind::NotFound => return Err(err),
            _ => {}
        }
        Ok(Some(Server { listener: UnixListener::bind(socket)?, lock }))
    }

    // The running window binds its socket just after taking the lock, so it can be a moment late.
    pub fn connect(name: &str) -> io::Result<Option<UnixStream>> {
        let socket = path(name, "sock");
        for _ in 0..CONNECT_ATTEMPTS {
            match UnixStream::connect(&socket) {
                Ok(stream) => return Ok(Some(stream)),
                Err(err) if matches!(err.kind(), io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused) => {
                    std::thread::sleep(CONNECT_RETRY);
                }
                Err(err) => return Err(err),
            }
        }
        Ok(None)
    }

    impl Server {
        // Each connection is read on its own thread, so a launch that never finishes sending holds up nothing else.
        pub fn serve(self, on_message: OnMessage) {
            let Server { listener, lock: _lock } = self;
            for stream in listener.incoming() {
                match stream {
                    Ok(stream) => {
                        let on_message = on_message.clone();
                        std::thread::spawn(move || super::answer(stream, &on_message));
                    }
                    Err(err) => eprintln!("could not accept another launch: {err}"),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use super::*;

    fn launch(sources: &[&str], activation_token: Option<&str>) -> Launch {
        Launch {
            sources: sources.iter().map(|source| source.to_string()).collect(),
            activation_token: activation_token.map(str::to_string),
        }
    }

    fn first_launch(name: &str) -> (Listener, mpsc::Receiver<(Vec<String>, Option<String>)>) {
        let Claim::Primary(server) = claim_named(name, &launch(&[], None)) else {
            panic!("the first launch should become the window")
        };
        let (sender, received) = mpsc::channel();
        let listener = listen(server, move |launch| sender.send((launch.sources, launch.activation_token)).is_ok());
        (listener, received)
    }

    #[test]
    fn later_launches_hand_their_files_to_the_first() {
        let name = format!("rona-test-forward-{}", std::process::id());
        let (_listener, received) = first_launch(&name);
        let next = || received.recv_timeout(Duration::from_secs(5)).unwrap();

        let files = [
            r"C:\Users\someone\Videos\Season 1\Épisode 01 [1080p].mkv",
            r"D:\映画\movie.mp4",
            "https://example.com/stream.m3u8?token=a b",
        ];
        assert!(matches!(claim_named(&name, &launch(&files, Some("kwin-1234_TIME5678"))), Claim::Forwarded));
        let (sources, token) = next();
        assert_eq!(sources, files);
        assert_eq!(token.as_deref(), Some("kwin-1234_TIME5678"));

        assert!(matches!(claim_named(&name, &launch(&[], None)), Claim::Forwarded));
        assert_eq!(next(), (Vec::new(), None));
    }

    #[test]
    fn a_launch_while_the_window_closes_opens_its_own() {
        let name = format!("rona-test-closing-{}", std::process::id());
        let (listener, received) = first_launch(&name);
        listener.stop();

        assert!(matches!(claim_named(&name, &launch(&["movie.mkv"], None)), Claim::Alone));
        assert!(received.try_recv().is_err());
    }
}
