use anyhow::Result;
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::sys::signal::{Signal, kill};
use nix::sys::termios::{SetArg, Termios, cfmakeraw, tcgetattr, tcsetattr};
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::Pid;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::time::{Duration, Instant};

pub struct PtyChild {
    pub pid: Pid,
    pub primary_fd: OwnedFd,
}

pub struct PtyResult {
    pub exit_code: i32,
    pub duration: Duration,
}

/// Puts the controlling terminal into raw mode so keystrokes pass through
/// to the child PTY unbuffered, and restores the original settings on drop
/// (including on panic or error return).
struct RawModeGuard {
    fd: std::os::fd::RawFd,
    original: Termios,
}

impl RawModeGuard {
    fn new(fd: std::os::fd::RawFd) -> Option<Self> {
        let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
        let original = tcgetattr(borrowed).ok()?;
        let mut raw = original.clone();
        cfmakeraw(&mut raw);
        tcsetattr(borrowed, SetArg::TCSANOW, &raw).ok()?;
        Some(Self { fd, original })
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let borrowed = unsafe { BorrowedFd::borrow_raw(self.fd) };
        let _ = tcsetattr(borrowed, SetArg::TCSANOW, &self.original);
    }
}

/// Copies the window size from `src_fd` (the real terminal) to `dst_fd`
/// (the PTY primary) so TUIs render at the correct dimensions.
fn sync_winsize(src_fd: std::os::fd::RawFd, dst_fd: std::os::fd::RawFd) {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let ok = unsafe { libc::ioctl(src_fd, libc::TIOCGWINSZ, std::ptr::addr_of_mut!(ws)) };
    if ok == 0 && ws.ws_col > 0 {
        unsafe { libc::ioctl(dst_fd, libc::TIOCSWINSZ, std::ptr::addr_of!(ws)) };
    }
}

impl PtyChild {
    pub fn spawn(command: &str) -> Result<Self> {
        let mut primary: libc::c_int = 0;
        let mut secondary: libc::c_int = 0;

        let ret = unsafe { libc::openpty(std::ptr::addr_of_mut!(primary), std::ptr::addr_of_mut!(secondary), std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut()) };
        if ret != 0 {
            anyhow::bail!("openpty failed: {}", std::io::Error::last_os_error());
        }

        let primary_fd = unsafe { OwnedFd::from_raw_fd(primary) };

        let pid = unsafe { libc::fork() };
        match pid {
            -1 => {
                unsafe { libc::close(secondary) };
                anyhow::bail!("fork failed: {}", std::io::Error::last_os_error());
            }
            0 => {
                // Child process
                drop(primary_fd);
                unsafe {
                    libc::setsid();
                    libc::ioctl(secondary, u64::from(libc::TIOCSCTTY), 0);
                    libc::dup2(secondary, 0);
                    libc::dup2(secondary, 1);
                    libc::dup2(secondary, 2);
                    if secondary > 2 {
                        libc::close(secondary);
                    }

                    let shell = c"/bin/sh".as_ptr();
                    let flag = c"-c".as_ptr();
                    let cmd = match std::ffi::CString::new(command) {
                        Ok(c) => c,
                        Err(_) => libc::_exit(127),
                    };
                    let args = [shell, flag, cmd.as_ptr(), std::ptr::null()];
                    libc::execvp(shell, args.as_ptr());
                    libc::_exit(127);
                }
            }
            child_pid => {
                // Parent process
                unsafe { libc::close(secondary) };
                Ok(Self {
                    pid: Pid::from_raw(child_pid),
                    primary_fd,
                })
            }
        }
    }

    pub fn wait_with_output<F>(&self, mut on_output: F) -> Result<PtyResult>
    where
        F: FnMut(&[u8]),
    {
        let start = Instant::now();
        let mut buf = [0u8; 4096];

        let stdin_fd = std::io::stdin().as_raw_fd();
        let stdin_is_tty = unsafe { libc::isatty(stdin_fd) } == 1;
        let mut stdin_open = true;

        // Interactive children (TUIs, prompts) need keystrokes forwarded.
        // Raw mode stops the outer terminal from line-buffering and echoing;
        // the guard restores the original settings when this function exits.
        let _raw_guard = if stdin_is_tty {
            sync_winsize(stdin_fd, self.primary_fd.as_raw_fd());
            RawModeGuard::new(stdin_fd)
        } else {
            None
        };

        // Propagate terminal resizes to the child PTY via SIGWINCH.
        let winch_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        if stdin_is_tty {
            let flag = std::sync::Arc::clone(&winch_flag);
            unsafe {
                signal_hook::low_level::register(signal_hook::consts::SIGWINCH, move || {
                    flag.store(true, std::sync::atomic::Ordering::Relaxed);
                })
            }
            .ok();
        }

        loop {
            if winch_flag.swap(false, std::sync::atomic::Ordering::Relaxed) {
                sync_winsize(stdin_fd, self.primary_fd.as_raw_fd());
            }

            let fd = unsafe { BorrowedFd::borrow_raw(self.primary_fd.as_raw_fd()) };
            let stdin_borrowed = unsafe { BorrowedFd::borrow_raw(stdin_fd) };
            let mut poll_fds = if stdin_open {
                vec![
                    PollFd::new(fd, PollFlags::POLLIN),
                    PollFd::new(stdin_borrowed, PollFlags::POLLIN),
                ]
            } else {
                vec![PollFd::new(fd, PollFlags::POLLIN)]
            };

            match poll(&mut poll_fds, PollTimeout::from(100u16)) {
                Ok(n) if n > 0 => {
                    let child_revents = poll_fds[0].revents().unwrap_or(PollFlags::empty());
                    let stdin_ready = poll_fds
                        .get(1)
                        .and_then(nix::poll::PollFd::revents)
                        .is_some_and(|r| r.contains(PollFlags::POLLIN));

                    if stdin_ready {
                        let n_in = unsafe {
                            libc::read(stdin_fd, buf.as_mut_ptr().cast(), buf.len())
                        };
                        if n_in > 0 {
                            let mut written = 0;
                            while written < n_in as usize {
                                let n_out = unsafe {
                                    libc::write(
                                        self.primary_fd.as_raw_fd(),
                                        buf.as_ptr().add(written).cast(),
                                        n_in as usize - written,
                                    )
                                };
                                match n_out.cmp(&0) {
                                    std::cmp::Ordering::Greater => {
                                        written += n_out as usize;
                                    }
                                    std::cmp::Ordering::Less => {
                                        let err = std::io::Error::last_os_error();
                                        if err.raw_os_error() == Some(libc::EINTR) {
                                            continue;
                                        }
                                        break;
                                    }
                                    std::cmp::Ordering::Equal => break,
                                }
                            }
                        } else if n_in == 0 {
                            stdin_open = false;
                        }
                        // n_in < 0: transient error (e.g. EINTR), ignore and retry next poll
                    }

                    if child_revents.contains(PollFlags::POLLHUP)
                        && !child_revents.contains(PollFlags::POLLIN)
                    {
                        break;
                    }

                    if child_revents.contains(PollFlags::POLLIN) {
                        let bytes_read = unsafe {
                            libc::read(
                                self.primary_fd.as_raw_fd(),
                                buf.as_mut_ptr().cast(),
                                buf.len(),
                            )
                        };
                        match bytes_read {
                            0 => break,
                            n if n > 0 => {
                                let n = n as usize;
                                on_output(&buf[..n]);
                            }
                            _ => {
                                let errno = std::io::Error::last_os_error();
                                match errno.raw_os_error() {
                                    Some(libc::EIO | libc::EAGAIN) => break,
                                    _ => return Err(errno.into()),
                                }
                            }
                        }
                    }
                }
                Ok(_) => {
                    // Poll timeout — check if child is still alive
                    match waitpid(self.pid, Some(WaitPidFlag::WNOHANG)) {
                        Ok(WaitStatus::StillAlive) => continue,
                        Ok(status) => {
                            self.drain_remaining(&mut on_output);
                            let code = extract_exit_code(status);
                            return Ok(PtyResult {
                                exit_code: code,
                                duration: start.elapsed(),
                            });
                        }
                        Err(_) => break,
                    }
                }
                Err(nix::errno::Errno::EINTR) => continue,
                Err(e) => return Err(e.into()),
            }
        }

        let status = waitpid(self.pid, None)?;
        let code = extract_exit_code(status);
        Ok(PtyResult {
            exit_code: code,
            duration: start.elapsed(),
        })
    }

    fn drain_remaining<F>(&self, on_output: &mut F)
    where
        F: FnMut(&[u8]),
    {
        let mut buf = [0u8; 4096];
        loop {
            let fd = unsafe { BorrowedFd::borrow_raw(self.primary_fd.as_raw_fd()) };
            let mut poll_fds = [PollFd::new(fd, PollFlags::POLLIN)];
            match poll(&mut poll_fds, PollTimeout::from(50u16)) {
                Ok(n) if n > 0 => {
                    let revents = poll_fds[0].revents().unwrap_or(PollFlags::empty());
                    if revents.contains(PollFlags::POLLHUP) && !revents.contains(PollFlags::POLLIN) {
                        break;
                    }

                    let bytes_read = unsafe {
                        libc::read(
                            self.primary_fd.as_raw_fd(),
                            buf.as_mut_ptr().cast(),
                            buf.len(),
                        )
                    };
                    if bytes_read <= 0 {
                        break;
                    }
                    let n = bytes_read as usize;
                    on_output(&buf[..n]);
                }
                _ => break,
            }
        }
    }

    pub fn signal(&self, sig: Signal) -> Result<()> {
        kill(self.pid, sig)?;
        Ok(())
    }
}

fn extract_exit_code(status: WaitStatus) -> i32 {
    match status {
        WaitStatus::Exited(_, code) => code,
        WaitStatus::Signaled(_, sig, _) => 128 + sig as i32,
        _ => -1,
    }
}
