use anyhow::Result;
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::sys::signal::{Signal, kill};
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::Pid;
use std::io::Write as _;
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

        loop {
            let fd = unsafe { BorrowedFd::borrow_raw(self.primary_fd.as_raw_fd()) };
            let mut poll_fds = [PollFd::new(fd, PollFlags::POLLIN)];

            match poll(&mut poll_fds, PollTimeout::from(100u16)) {
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
                    match bytes_read {
                        0 => break,
                        n if n > 0 => {
                            let n = n as usize;
                            on_output(&buf[..n]);
                            let _ = std::io::stdout().write_all(&buf[..n]);
                            let _ = std::io::stdout().flush();
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
                    let _ = std::io::stdout().write_all(&buf[..n]);
                    let _ = std::io::stdout().flush();
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
