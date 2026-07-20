use crate::events::{ProcessEvent, ProcessExitEvent};
use chrono::Utc;
use std::collections::HashMap;
use std::time::{Duration, Instant};

pub struct ProcessTracker {
    root_pid: u32,
    known_pids: HashMap<u32, TrackedProcess>,
    poll_interval: Duration,
}

struct TrackedProcess {
    #[allow(dead_code)]
    argv: Vec<String>,
    start_time: Instant,
    reported_spawn: bool,
}

impl ProcessTracker {
    pub fn new(root_pid: u32, poll_interval_ms: u64) -> Self {
        Self {
            root_pid,
            known_pids: HashMap::new(),
            poll_interval: Duration::from_millis(poll_interval_ms),
        }
    }

    pub fn poll_interval(&self) -> Duration {
        self.poll_interval
    }

    pub fn poll(&mut self) -> (Vec<ProcessEvent>, Vec<ProcessExitEvent>) {
        let mut spawn_events = Vec::new();
        let mut exit_events = Vec::new();

        let current_children = get_descendant_pids(self.root_pid);

        // Detect new processes
        for &pid in &current_children {
            use std::collections::hash_map::Entry;
            if let Entry::Vacant(entry) = self.known_pids.entry(pid) {
                let argv = get_process_argv(pid);
                let tracked = TrackedProcess {
                    argv: argv.clone(),
                    start_time: Instant::now(),
                    reported_spawn: true,
                };
                entry.insert(tracked);

                spawn_events.push(ProcessEvent {
                    pid,
                    ppid: self.root_pid,
                    argv,
                    timestamp: Utc::now(),
                });
            }
        }

        // Detect exited processes
        let exited: Vec<u32> = self
            .known_pids
            .keys()
            .filter(|pid| !current_children.contains(pid))
            .copied()
            .collect();

        for pid in exited {
            if let Some(tracked) = self.known_pids.remove(&pid) {
                if tracked.reported_spawn {
                    exit_events.push(ProcessExitEvent {
                        pid,
                        exit_code: None,
                        duration: tracked.start_time.elapsed(),
                        timestamp: Utc::now(),
                    });
                }
            }
        }

        (spawn_events, exit_events)
    }

    pub fn finish_all(&mut self) -> Vec<ProcessExitEvent> {
        let mut events = Vec::new();
        for (pid, tracked) in self.known_pids.drain() {
            if tracked.reported_spawn {
                events.push(ProcessExitEvent {
                    pid,
                    exit_code: None,
                    duration: tracked.start_time.elapsed(),
                    timestamp: Utc::now(),
                });
            }
        }
        events
    }
}

#[cfg(target_os = "macos")]
fn get_descendant_pids(root_pid: u32) -> Vec<u32> {
    let mut pids = Vec::new();
    let mut to_check = vec![root_pid];

    while let Some(parent) = to_check.pop() {
        let mut buf = [0i32; 1024];
        let count = unsafe {
            libc::proc_listchildpids(parent as i32, buf.as_mut_ptr().cast(), std::mem::size_of_val(&buf) as i32)
        };
        if count > 0 {
            let n = count as usize / std::mem::size_of::<i32>();
            for &pid in &buf[..n] {
                if pid > 0 {
                    let upid = pid as u32;
                    pids.push(upid);
                    to_check.push(upid);
                }
            }
        }
    }

    pids
}

#[cfg(target_os = "linux")]
fn get_descendant_pids(root_pid: u32) -> Vec<u32> {
    let mut pids = Vec::new();
    let mut to_check = vec![root_pid];

    while let Some(parent) = to_check.pop() {
        let children_path = format!("/proc/{parent}/task/{parent}/children");
        if let Ok(content) = std::fs::read_to_string(&children_path) {
            for part in content.split_whitespace() {
                if let Ok(pid) = part.parse::<u32>() {
                    pids.push(pid);
                    to_check.push(pid);
                }
            }
        }
    }

    pids
}

#[cfg(target_os = "macos")]
fn get_process_argv(pid: u32) -> Vec<String> {
    let mut mib = [
        libc::CTL_KERN,
        libc::KERN_PROCARGS2,
        pid as i32,
    ];

    let mut size: usize = 0;
    let ret = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            std::ptr::null_mut(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };

    if ret != 0 || size == 0 {
        return vec![format!("<pid:{pid}>")];
    }

    let mut buf = vec![0u8; size];
    let ret = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            buf.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };

    if ret != 0 {
        return vec![format!("<pid:{pid}>")];
    }

    if size < std::mem::size_of::<i32>() {
        return vec![format!("<pid:{pid}>")];
    }

    let argc = i32::from_ne_bytes(buf[..4].try_into().unwrap_or([0; 4]));
    let mut pos = 4;

    // Skip exec_path (null-terminated)
    while pos < size && buf[pos] != 0 {
        pos += 1;
    }
    // Skip trailing nulls
    while pos < size && buf[pos] == 0 {
        pos += 1;
    }

    let mut argv = Vec::new();
    let argc_usize = argc.max(0) as usize;
    for _ in 0..argc_usize {
        if pos >= size {
            break;
        }
        let start = pos;
        while pos < size && buf[pos] != 0 {
            pos += 1;
        }
        if let Ok(s) = std::str::from_utf8(&buf[start..pos]) {
            argv.push(s.to_string());
        }
        pos += 1;
    }

    if argv.is_empty() {
        vec![format!("<pid:{pid}>")]
    } else {
        argv
    }
}

#[cfg(target_os = "linux")]
fn get_process_argv(pid: u32) -> Vec<String> {
    let path = format!("/proc/{pid}/cmdline");
    match std::fs::read(&path) {
        Ok(data) => {
            let argv: Vec<String> = data
                .split(|&b| b == 0)
                .filter(|s| !s.is_empty())
                .filter_map(|s| std::str::from_utf8(s).ok())
                .map(String::from)
                .collect();
            if argv.is_empty() {
                vec![format!("<pid:{pid}>")]
            } else {
                argv
            }
        }
        Err(_) => vec![format!("<pid:{pid}>")],
    }
}
