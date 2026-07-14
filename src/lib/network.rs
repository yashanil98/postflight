use crate::events::{NetworkEvent, Protocol};
use chrono::Utc;
use std::collections::HashSet;
use std::process::Command;
use std::time::Duration;

#[cfg(target_os = "macos")]
extern crate libc;

pub struct NetworkObserver {
    root_pid: u32,
    seen_connections: HashSet<ConnectionKey>,
    poll_interval: Duration,
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
struct ConnectionKey {
    pid: u32,
    remote_host: String,
    remote_port: u16,
    protocol: Protocol,
}

impl NetworkObserver {
    pub fn new(root_pid: u32, poll_interval_ms: u64) -> Self {
        Self {
            root_pid,
            seen_connections: HashSet::new(),
            poll_interval: Duration::from_millis(poll_interval_ms),
        }
    }

    pub fn poll_interval(&self) -> Duration {
        self.poll_interval
    }

    pub fn poll(&mut self) -> Vec<NetworkEvent> {
        let connections = self.get_connections();
        let mut new_events = Vec::new();

        for conn in connections {
            let key = ConnectionKey {
                pid: conn.pid,
                remote_host: conn.remote_host.clone(),
                remote_port: conn.remote_port,
                protocol: conn.protocol.clone(),
            };

            if !self.seen_connections.contains(&key) {
                self.seen_connections.insert(key);
                new_events.push(conn);
            }
        }

        new_events
    }

    #[cfg(target_os = "macos")]
    fn get_connections(&self) -> Vec<NetworkEvent> {
        let pids = get_process_tree_pids(self.root_pid);
        if pids.is_empty() {
            return Vec::new();
        }

        let pid_list = pids.iter().map(ToString::to_string).collect::<Vec<_>>().join(",");

        let output = Command::new("lsof")
            .args(["-i", "-n", "-P", "-a", "-p", &pid_list])
            .output();

        match output {
            Ok(o) => self.parse_lsof_output(&String::from_utf8_lossy(&o.stdout)),
            Err(_) => Vec::new(),
        }
    }

    #[cfg(target_os = "linux")]
    fn get_connections(&self) -> Vec<NetworkEvent> {
        let output = Command::new("lsof")
            .args(["-i", "-n", "-P", "-p", &self.root_pid.to_string()])
            .output();

        match output {
            Ok(o) => self.parse_lsof_output(&String::from_utf8_lossy(&o.stdout)),
            Err(_) => Vec::new(),
        }
    }

    #[allow(clippy::unused_self)]
    fn parse_lsof_output(&self, output: &str) -> Vec<NetworkEvent> {
        let mut events = Vec::new();

        for line in output.lines().skip(1) {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() < 9 {
                continue;
            }

            let pid: u32 = match parts[1].parse() {
                Ok(p) => p,
                Err(_) => continue,
            };

            let node = parts[parts.len() - 2];
            let name = parts[parts.len() - 1];

            let protocol = match node.to_uppercase().as_str() {
                "TCP" => Protocol::Tcp,
                "UDP" => Protocol::Udp,
                _ => continue,
            };

            if let Some((remote_host, remote_port)) = parse_remote_address(name) {
                events.push(NetworkEvent {
                    pid,
                    remote_host,
                    remote_port,
                    protocol,
                    timestamp: Utc::now(),
                });
            }
        }

        events
    }
}

#[cfg(target_os = "macos")]
fn get_process_tree_pids(root_pid: u32) -> Vec<u32> {
    let mut pids = vec![root_pid];
    let mut to_check = vec![root_pid];

    while let Some(parent) = to_check.pop() {
        let mut buf = [0i32; 512];
        let count = unsafe {
            libc::proc_listchildpids(
                parent as i32,
                buf.as_mut_ptr().cast(),
                std::mem::size_of_val(&buf) as i32,
            )
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

fn parse_remote_address(name: &str) -> Option<(String, u16)> {
    let remote_part = if let Some(arrow_pos) = name.find("->") {
        let after_arrow = &name[arrow_pos + 2..];
        after_arrow.split_whitespace().next().unwrap_or(after_arrow)
            .trim_end_matches(')')
            .split('(')
            .next()
            .unwrap_or(after_arrow)
    } else {
        return None;
    };

    if let Some(bracket_end) = remote_part.find(']') {
        let host = &remote_part[1..bracket_end];
        let port_str = &remote_part[bracket_end + 2..];
        let port: u16 = port_str.parse().ok()?;
        Some((host.to_string(), port))
    } else {
        let last_colon = remote_part.rfind(':')?;
        let host = &remote_part[..last_colon];
        let port: u16 = remote_part[last_colon + 1..].parse().ok()?;
        if host == "127.0.0.1" || host == "::1" || host == "localhost" {
            return None;
        }
        Some((host.to_string(), port))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_remote_address() {
        let result = parse_remote_address("192.168.1.1:8080->93.184.216.34:443");
        assert_eq!(result, Some(("93.184.216.34".to_string(), 443)));

        let result = parse_remote_address("localhost:5000->127.0.0.1:3000");
        assert_eq!(result, None);

        let result = parse_remote_address("*:8080");
        assert_eq!(result, None);
    }

    #[test]
    fn test_parse_ipv6() {
        let result = parse_remote_address("*:443->[2001:db8::1]:8080");
        assert_eq!(result, Some(("2001:db8::1".to_string(), 8080)));
    }
}
