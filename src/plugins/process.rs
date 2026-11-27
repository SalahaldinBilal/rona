//! Runs programs for `system.run`: found on PATH, no shell, no console window. Whatever a program starts ends
//! with it: when it exits, is killed or is closed, its whole job (Windows) or process group (Unix) goes.

use std::io::{Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

// How long output may keep coming after the program ended, from something that escaped its job or group.
const OUTPUT_GRACE: Duration = Duration::from_secs(1);

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

pub struct Output {
    pub status: Option<i32>,
    pub timed_out: bool,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

#[cfg(windows)]
mod platform {
    use std::process::Child;

    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JobObjectExtendedLimitInformation, SetInformationJobObject, TerminateJobObject,
    };
    use windows::Win32::System::Threading::{INFINITE, WaitForSingleObject};
    use windows::core::PCWSTR;

    /// A job holding the program and everything it starts. Closing it kills them all.
    pub struct Group(isize);

    impl Group {
        pub fn new(child: &Child) -> std::io::Result<Group> {
            use std::os::windows::io::AsRawHandle;
            let job = unsafe { CreateJobObjectW(None, PCWSTR::null()) }.map_err(std::io::Error::other)?;
            let group = Group(job.0 as isize);
            let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            unsafe {
                SetInformationJobObject(
                    job,
                    JobObjectExtendedLimitInformation,
                    (&raw const limits).cast(),
                    size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )
                .map_err(std::io::Error::other)?;
                AssignProcessToJobObject(job, HANDLE(child.as_raw_handle())).map_err(std::io::Error::other)?;
            }
            resume(child.id())?;
            Ok(group)
        }

        pub fn kill(&self) {
            let _ = unsafe { TerminateJobObject(HANDLE(self.0 as *mut _), 1) };
        }
    }

    impl Drop for Group {
        fn drop(&mut self) {
            let _ = unsafe { CloseHandle(HANDLE(self.0 as *mut _)) };
        }
    }

    pub struct Exit(isize);

    impl Exit {
        pub fn of(child: &Child) -> Exit {
            use std::os::windows::io::AsRawHandle;
            Exit(child.as_raw_handle() as isize)
        }

        /// Blocks until the program ended. The handle stays open, since the `Child` it came from outlives this.
        pub fn wait(&self) {
            let _ = unsafe { WaitForSingleObject(HANDLE(self.0 as *mut _), INFINITE) };
        }
    }

    // Started suspended, so nothing it starts can run before it's in its job.
    pub fn prepare(command: &mut std::process::Command) {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const CREATE_SUSPENDED: u32 = 0x0000_0004;
        command.creation_flags(CREATE_NO_WINDOW | CREATE_SUSPENDED);
    }

    // std doesn't hand out the main thread's handle, so it's found among the process's threads.
    fn resume(pid: u32) -> std::io::Result<()> {
        use windows::Win32::System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
        };
        use windows::Win32::System::Threading::{OpenThread, ResumeThread, THREAD_SUSPEND_RESUME};

        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) }.map_err(std::io::Error::other)?;
        let mut entry = THREADENTRY32 { dwSize: size_of::<THREADENTRY32>() as u32, ..Default::default() };
        let mut resumed = false;
        let mut more = unsafe { Thread32First(snapshot, &mut entry) }.is_ok();
        while more {
            if entry.th32OwnerProcessID == pid
                && let Ok(thread) = unsafe { OpenThread(THREAD_SUSPEND_RESUME, false, entry.th32ThreadID) }
            {
                resumed |= unsafe { ResumeThread(thread) } != u32::MAX;
                let _ = unsafe { CloseHandle(thread) };
            }
            more = unsafe { Thread32Next(snapshot, &mut entry) }.is_ok();
        }
        let _ = unsafe { CloseHandle(snapshot) };
        if resumed { Ok(()) } else { Err(std::io::Error::other("couldn't start the program's main thread")) }
    }
}

#[cfg(unix)]
mod platform {
    use std::process::Child;

    /// The program's own process group, which everything it starts joins.
    pub struct Group(libc::pid_t);

    impl Group {
        pub fn new(child: &Child) -> std::io::Result<Group> {
            Ok(Group(child.id() as libc::pid_t))
        }

        /// Only called before the program is reaped, so the group id can't belong to anyone else yet.
        pub fn kill(&self) {
            unsafe { libc::kill(-self.0, libc::SIGKILL) };
        }
    }

    pub struct Exit(libc::pid_t);

    impl Exit {
        pub fn of(child: &Child) -> Exit {
            Exit(child.id() as libc::pid_t)
        }

        /// Blocks until the program ended without reaping it, so its pid and group stay reserved.
        pub fn wait(&self) {
            loop {
                let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
                let result = unsafe { libc::waitid(libc::P_PID, self.0 as libc::id_t, &mut info, libc::WEXITED | libc::WNOWAIT) };
                if result == 0 || std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
                    return;
                }
            }
        }
    }

    pub fn prepare(command: &mut std::process::Command) {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
}

struct Tree {
    child: Child,
    group: platform::Group,
    ended: bool,
}

impl Tree {
    fn kill(&mut self) {
        if !self.ended {
            self.group.kill();
            let _ = self.child.kill();
        }
    }
}

/// Waits for the program to end, ends what it left running and returns its exit code.
fn wait_for(tree: &Mutex<Tree>, exit: &platform::Exit) -> Option<i32> {
    exit.wait();
    let mut tree = lock(tree);
    tree.group.kill();
    tree.ended = true;
    tree.child.wait().ok().and_then(|status| status.code())
}

struct Spawned {
    tree: Arc<Mutex<Tree>>,
    exit: platform::Exit,
    stdin: ChildStdin,
    stdout: std::process::ChildStdout,
    stderr: std::process::ChildStderr,
}

fn spawn(program: &str, args: &[String]) -> std::io::Result<Spawned> {
    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(std::env::temp_dir())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    platform::prepare(&mut command);
    let mut child = command.spawn()?;
    let group = match platform::Group::new(&child) {
        Ok(group) => group,
        Err(err) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(err);
        }
    };
    let exit = platform::Exit::of(&child);
    let stdin = child.stdin.take().expect("stdin is piped");
    let stdout = child.stdout.take().expect("stdout is piped");
    let stderr = child.stderr.take().expect("stderr is piped");
    Ok(Spawned { tree: Arc::new(Mutex::new(Tree { child, group, ended: false })), exit, stdin, stdout, stderr })
}

// Hands each read to `push` until the stream ends or `push` says stop, then reports on `done`.
fn read_stream(mut stream: impl Read + Send + 'static, mut push: impl FnMut(&[u8]) -> bool + Send + 'static, done: Sender<()>) {
    std::thread::spawn(move || {
        let mut buffer = vec![0u8; 64 * 1024];
        while let Ok(read) = stream.read(&mut buffer) {
            if read == 0 || !push(&buffer[..read]) {
                break;
            }
        }
        let _ = done.send(());
    });
}

fn wait_output(done: &Receiver<()>, streams: usize) {
    for _ in 0..streams {
        if done.recv_timeout(OUTPUT_GRACE).is_err() {
            return;
        }
    }
}

/// Runs a program to its end and returns its output. Output past `limit` is read and dropped, so a chatty program
/// can't fill memory or block on a full pipe.
pub fn run(program: &str, args: &[String], stdin: Vec<u8>, timeout: Duration, limit: usize) -> std::io::Result<Output> {
    let Spawned { tree, exit, stdin: mut input, stdout, stderr } = spawn(program, args)?;
    std::thread::spawn(move || {
        let _ = input.write_all(&stdin);
    });
    let (done, finished) = channel();
    let kept = [Arc::new(Mutex::new(Vec::new())), Arc::new(Mutex::new(Vec::new()))];
    for (stream, kept) in [Box::new(stdout) as Box<dyn Read + Send>, Box::new(stderr)].into_iter().zip(kept.iter().cloned()) {
        read_stream(
            stream,
            move |bytes| {
                let mut kept = lock(&kept);
                let room = limit.saturating_sub(kept.len());
                kept.extend_from_slice(&bytes[..bytes.len().min(room)]);
                true
            },
            done.clone(),
        );
    }

    let (exited, status) = channel();
    let waiting = tree.clone();
    std::thread::spawn(move || {
        let _ = exited.send(wait_for(&waiting, &exit));
    });
    let (status, timed_out) = match status.recv_timeout(timeout) {
        Ok(status) => (status, false),
        Err(_) => {
            lock(&tree).kill();
            let _ = status.recv();
            (None, true)
        }
    };
    wait_output(&finished, 2);
    let [stdout, stderr] = kept;
    Ok(Output { status, timed_out, stdout: std::mem::take(&mut lock(&stdout)), stderr: std::mem::take(&mut lock(&stderr)) })
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Stream {
    Stdout,
    Stderr,
}

pub enum Event {
    /// Output arrived. No other is sent until the stream is read, unless this one wasn't queued.
    Output,
    /// The program ended, and its output so far is in the buffers. `None` when it was killed or overflowed.
    Exited(Option<i32>),
    /// It printed more than `max_buffer` that wasn't read yet, so it was killed.
    Overflowed,
}

/// A program that keeps running while the plugin talks to it. Dropping it kills it and what it started.
pub struct Running {
    pub program: String,
    tree: Arc<Mutex<Tree>>,
    input: Mutex<Option<Sender<Vec<u8>>>>,
    output: [Arc<Mutex<Vec<u8>>>; 2],
    told: [Arc<AtomicBool>; 2],
    status: Arc<Mutex<Option<Option<i32>>>>,
}

impl Running {
    /// `notify` returns whether the event reached the plugin.
    pub fn start(
        program: &str,
        args: &[String],
        max_buffer: usize,
        notify: impl Fn(Event) -> bool + Send + Sync + 'static,
    ) -> std::io::Result<Running> {
        let Spawned { tree, exit, stdin, stdout, stderr } = spawn(program, args)?;
        let notify = Arc::new(notify);

        let (input, incoming) = channel::<Vec<u8>>();
        std::thread::spawn(move || {
            let mut stdin = stdin;
            while let Ok(bytes) = incoming.recv() {
                if stdin.write_all(&bytes).and_then(|()| stdin.flush()).is_err() {
                    break;
                }
            }
        });

        let output = [Arc::new(Mutex::new(Vec::new())), Arc::new(Mutex::new(Vec::new()))];
        let told = [Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false))];
        let (done, finished) = channel();
        let streams = [Box::new(stdout) as Box<dyn Read + Send>, Box::new(stderr)];
        for ((stream, buffer), told) in streams.into_iter().zip(output.iter().cloned()).zip(told.iter().cloned()) {
            let (notify, tree) = (notify.clone(), tree.clone());
            read_stream(
                stream,
                move |bytes| {
                    let mut buffer = lock(&buffer);
                    if buffer.len() + bytes.len() > max_buffer {
                        drop(buffer);
                        lock(&tree).kill();
                        notify(Event::Overflowed);
                        return false;
                    }
                    buffer.extend_from_slice(bytes);
                    drop(buffer);
                    if !told.swap(true, Ordering::SeqCst) && !notify(Event::Output) {
                        told.store(false, Ordering::SeqCst);
                    }
                    true
                },
                done.clone(),
            );
        }

        let status = Arc::new(Mutex::new(None));
        let (waiting, ended) = (tree.clone(), status.clone());
        std::thread::spawn(move || {
            let code = wait_for(&waiting, &exit);
            wait_output(&finished, 2);
            *lock(&ended) = Some(code);
            notify(Event::Exited(code));
        });

        Ok(Running { program: program.to_owned(), tree, input: Mutex::new(Some(input)), output, told, status })
    }

    /// Fails once the input was ended or the program stopped reading.
    pub fn write(&self, bytes: Vec<u8>) -> Result<(), String> {
        let input = lock(&self.input);
        let sender = input.as_ref().ok_or("The program's input was already ended")?;
        sender.send(bytes).map_err(|_| "The program stopped taking input".to_string())
    }

    /// Closes the program's input, which tells many programs to finish.
    pub fn end_input(&self) {
        lock(&self.input).take();
    }

    pub fn take(&self, stream: Stream) -> Vec<u8> {
        self.told[stream as usize].store(false, Ordering::SeqCst);
        std::mem::take(&mut lock(&self.output[stream as usize]))
    }

    /// `Some` once it ended, with its exit code if it had one.
    pub fn status(&self) -> Option<Option<i32>> {
        *lock(&self.status)
    }

    pub fn kill(&self) {
        lock(&self.tree).kill();
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.kill();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[cfg(windows)]
    fn echo_input() -> (&'static str, Vec<String>) {
        ("findstr", vec!["x".into()])
    }

    #[cfg(unix)]
    fn echo_input() -> (&'static str, Vec<String>) {
        ("grep", vec!["x".into()])
    }

    type Talker = (&'static str, Vec<String>, fn(&str) -> Vec<u8>);

    // A program that prints each line it's given right away, and the input that makes it print `text`.
    #[cfg(windows)]
    fn talker() -> Talker {
        ("cmd", vec!["/d".into(), "/q".into(), "/k".into(), "prompt $s".into()], |text| format!("echo {text}\r\n").into_bytes())
    }

    #[cfg(unix)]
    fn talker() -> Talker {
        ("grep", vec!["--line-buffered".into(), "".into()], |text| format!("{text}\n").into_bytes())
    }

    #[cfg(windows)]
    fn hang() -> (&'static str, Vec<String>) {
        ("ping", vec!["-n".into(), "30".into(), "127.0.0.1".into()])
    }

    #[cfg(unix)]
    fn hang() -> (&'static str, Vec<String>) {
        ("sleep", vec!["30".into()])
    }

    // Starts a child that keeps running and holds the output open, then exits at once.
    #[cfg(windows)]
    fn leaves_a_child_behind() -> (&'static str, Vec<String>) {
        ("cmd", vec!["/c".into(), "start /b ping -n 30 127.0.0.1 & echo started".into()])
    }

    #[cfg(unix)]
    fn leaves_a_child_behind() -> (&'static str, Vec<String>) {
        ("sh", vec!["-c".into(), "sleep 30 & echo started".into()])
    }

    #[test]
    fn the_input_reaches_the_program_and_its_output_comes_back() {
        let (program, args) = echo_input();
        let output = run(program, &args, b"line x one\nno match\nline x two\n".to_vec(), Duration::from_secs(10), 1024).unwrap();
        assert_eq!(output.status, Some(0));
        let text = String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n");
        assert_eq!(text, "line x one\nline x two\n");
    }

    #[test]
    fn a_program_that_runs_too_long_is_killed() {
        let (program, args) = hang();
        let started = Instant::now();
        let output = run(program, &args, Vec::new(), Duration::from_millis(300), 1024).unwrap();
        assert!(output.timed_out);
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn output_past_the_limit_is_dropped_without_blocking_the_program() {
        let (program, args) = echo_input();
        let input = "x".repeat(200_000) + "\n";
        let output = run(program, &args, input.into_bytes(), Duration::from_secs(10), 1000).unwrap();
        assert_eq!(output.status, Some(0));
        assert_eq!(output.stdout.len(), 1000);
    }

    #[test]
    fn a_missing_program_is_an_error_not_a_hang() {
        assert!(run("rona-no-such-program", &[], Vec::new(), Duration::from_secs(1), 1024).is_err());
    }

    #[test]
    fn what_a_program_leaves_running_does_not_hold_up_the_call() {
        let (program, args) = leaves_a_child_behind();
        let started = Instant::now();
        let output = run(program, &args, Vec::new(), Duration::from_secs(20), 1024).unwrap();
        assert!(!output.timed_out);
        assert!(started.elapsed() < Duration::from_secs(5), "took {:?}", started.elapsed());
        assert!(String::from_utf8_lossy(&output.stdout).contains("started"));
    }

    fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
        let started = Instant::now();
        while !done() {
            assert!(started.elapsed() < Duration::from_secs(10), "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn a_running_program_answers_each_line_and_ends_when_its_input_does() {
        let (program, args, say) = talker();
        let (events, received) = channel();
        let running = Running::start(program, &args, 1024 * 1024, move |event| {
            events
                .send(match event {
                    Event::Output => "output".to_string(),
                    Event::Exited(code) => format!("exited {code:?}"),
                    Event::Overflowed => "overflowed".to_string(),
                })
                .is_ok()
        })
        .unwrap();

        let mut seen = String::new();
        for line in ["first answer", "second answer"] {
            running.write(say(line)).unwrap();
            wait_until("an answer", || {
                seen.push_str(&String::from_utf8_lossy(&running.take(Stream::Stdout)));
                seen.contains(line)
            });
        }
        assert_eq!(running.status(), None);

        running.end_input();
        wait_until("the exit", || running.status().is_some());
        let events: Vec<String> = received.try_iter().collect();
        assert!(events.contains(&"output".to_string()));
        assert!(events.last().is_some_and(|event| event.starts_with("exited")), "{events:?}");
    }

    #[test]
    fn output_is_told_again_when_the_first_event_was_dropped() {
        let (program, args, say) = talker();
        let (events, received) = channel();
        let dropped = Arc::new(AtomicBool::new(false));
        let running = Running::start(program, &args, 1024 * 1024, move |event| {
            if !matches!(event, Event::Output) {
                return true;
            }
            if !dropped.swap(true, Ordering::SeqCst) {
                return false;
            }
            events.send(()).is_ok()
        })
        .unwrap();
        running.write(say("first")).unwrap();
        std::thread::sleep(Duration::from_millis(500));
        running.write(say("second")).unwrap();
        assert!(received.recv_timeout(Duration::from_secs(5)).is_ok(), "no output event after the dropped one");
    }

    #[test]
    fn dropping_a_running_program_kills_it() {
        let (program, args) = hang();
        let (events, received) = channel();
        let running = Running::start(program, &args, 1024 * 1024, move |event| {
            if let Event::Exited(_) = event {
                let _ = events.send(());
            }
            true
        })
        .unwrap();
        drop(running);
        assert!(received.recv_timeout(Duration::from_secs(5)).is_ok());
    }

    #[test]
    fn a_program_that_prints_more_than_is_read_is_killed() {
        let (program, args, say) = talker();
        let (events, received) = channel();
        let running = Running::start(program, &args, 1000, move |event| {
            if let Event::Overflowed = event {
                let _ = events.send(());
            }
            true
        })
        .unwrap();
        running.write(say(&"x".repeat(5000))).unwrap();
        assert!(received.recv_timeout(Duration::from_secs(5)).is_ok());
        wait_until("the exit", || running.status().is_some());
    }
}
