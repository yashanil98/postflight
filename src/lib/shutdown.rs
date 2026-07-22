use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;

pub struct GracefulShutdown {
    session_dir: PathBuf,
    max_duration: Option<Duration>,
    grace_period: Duration,
    shutdown_message: String,
    child_pid: Pid,
    primary_fd: std::os::fd::RawFd,
    child_alive: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl GracefulShutdown {
    pub fn new(
        session_dir: PathBuf,
        max_duration: Option<Duration>,
        grace_period: Duration,
        shutdown_message: String,
        child_pid: Pid,
        primary_fd: std::os::fd::RawFd,
        child_alive: Arc<AtomicBool>,
    ) -> Self {
        Self {
            session_dir,
            max_duration,
            grace_period,
            shutdown_message,
            child_pid,
            primary_fd,
            child_alive,
            handle: None,
        }
    }

    pub fn start(&mut self, start_time: Instant) {
        let session_dir = self.session_dir.clone();
        let max_duration = self.max_duration;
        let grace_period = self.grace_period;
        let shutdown_message = self.shutdown_message.clone();
        let child_pid = self.child_pid;
        let primary_fd = self.primary_fd;
        let child_alive = Arc::clone(&self.child_alive);

        let handle = thread::spawn(move || {
            let sentinel_path = session_dir.join("stop_requested");

            loop {
                thread::sleep(Duration::from_secs(1));

                if !child_alive.load(Ordering::Relaxed) {
                    return;
                }

                let should_stop = sentinel_path.exists()
                    || max_duration.is_some_and(|d| start_time.elapsed() >= d);

                if should_stop {
                    initiate_shutdown(
                        &shutdown_message,
                        grace_period,
                        child_pid,
                        primary_fd,
                        &child_alive,
                    );
                    let _ = fs::remove_file(&sentinel_path);
                    return;
                }
            }
        });

        self.handle = Some(handle);
    }

    pub fn stop(mut self) {
        self.join_thread();
    }

    fn join_thread(&mut self) {
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }

    pub fn sentinel_path(session_dir: &Path) -> PathBuf {
        session_dir.join("stop_requested")
    }
}

impl Drop for GracefulShutdown {
    fn drop(&mut self) {
        self.child_alive.store(false, Ordering::Relaxed);
        self.join_thread();
    }
}

fn initiate_shutdown(
    message: &str,
    grace_period: Duration,
    child_pid: Pid,
    primary_fd: std::os::fd::RawFd,
    child_alive: &AtomicBool,
) {
    if !child_alive.load(Ordering::Relaxed) {
        return;
    }

    // Phase 1: Send text message to stdin via PTY. AI agents that read
    // stdin will see this and begin wrapping up intelligently — committing
    // code, saving state, producing final output.
    let msg_bytes = message.as_bytes();
    unsafe {
        libc::write(primary_fd, msg_bytes.as_ptr().cast(), msg_bytes.len());
    }

    // Phase 2: Wait the full grace period. Text-responsive agents (AI
    // coding agents, REPLs, interactive programs) use this time to wrap up.
    // If the process exits during this window, no escalation is needed.
    let deadline = Instant::now() + grace_period;
    while Instant::now() < deadline {
        if !child_alive.load(Ordering::Relaxed) {
            return;
        }
        thread::sleep(Duration::from_millis(500));
    }

    // Phase 3: SIGTERM the process group for processes that didn't respond
    // to text. Sending to the negative PID targets the entire process group
    // (the child shell + all its descendants), ensuring sleep/subprocesses
    // also receive the signal.
    if !child_alive.load(Ordering::Relaxed) {
        return;
    }
    let _ = kill(Pid::from_raw(-child_pid.as_raw()), Signal::SIGTERM);

    // Phase 4: Wait for SIGTERM to take effect (5 seconds).
    let term_deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < term_deadline {
        if !child_alive.load(Ordering::Relaxed) {
            return;
        }
        thread::sleep(Duration::from_millis(250));
    }

    // Phase 5: SIGKILL the process group as absolute last resort.
    // Only reaches here for processes that deliberately ignore SIGTERM.
    if child_alive.load(Ordering::Relaxed) {
        let _ = kill(Pid::from_raw(-child_pid.as_raw()), Signal::SIGKILL);
    }
}
