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
            .stderr(std::process::Stdio::null())
            .output();

        match output {
            Ok(o) => self.parse_lsof_output(&String::from_utf8_lossy(&o.stdout)),
            Err(_) => Vec::new(),
        }
    }

    #[cfg(target_os = "linux")]
    fn get_connections(&self) -> Vec<NetworkEvent> {
        let pids = get_process_tree_pids(self.root_pid);
        if pids.is_empty() {
            return Vec::new();
        }

        let pid_list = pids
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(",");

        let output = Command::new("lsof")
            .args(["-i", "-n", "-P", "-a", "-p", &pid_list])
            .stderr(std::process::Stdio::null())
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

            // NODE is always at index 7 in lsof -i output. The NAME field
            // follows at index 8, but may be trailed by a state like
            // "(ESTABLISHED)" at index 9+.
            let node = parts[7];
            let name = parts[8];

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
        let buf_size = std::mem::size_of_val(&buf) as i32;
        let count = unsafe {
            libc::proc_listchildpids(
                parent as i32,
                buf.as_mut_ptr().cast(),
                buf_size,
            )
        };
        if count > 0 {
            let n = (count as usize / std::mem::size_of::<i32>()).min(buf.len());
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
fn get_process_tree_pids(root_pid: u32) -> Vec<u32> {
    let mut pids = vec![root_pid];
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

    let (host, port) = if let Some(bracket_end) = remote_part.find(']') {
        let h = &remote_part[1..bracket_end];
        let port_str = remote_part.get(bracket_end + 2..)?;
        let p: u16 = port_str.parse().ok()?;
        (h, p)
    } else {
        let last_colon = remote_part.rfind(':')?;
        let h = &remote_part[..last_colon];
        let p: u16 = remote_part[last_colon + 1..].parse().ok()?;
        (h, p)
    };

    if is_loopback(host) {
        return None;
    }

    Some((host.to_string(), port))
}

fn is_loopback(host: &str) -> bool {
    host == "127.0.0.1"
        || host == "::1"
        || host == "localhost"
        || host.starts_with("127.")
        || host == "::ffff:127.0.0.1"
        || host.starts_with("::ffff:127.")
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

    #[test]
    fn test_parse_ipv6_loopback_filtered() {
        let result = parse_remote_address("*:5000->[::1]:3000");
        assert_eq!(result, None);
    }

    #[test]
    fn test_parse_ipv4_mapped_loopback_filtered() {
        let result = parse_remote_address("*:5000->::ffff:127.0.0.1:8080");
        assert_eq!(result, None);

        let result = parse_remote_address("*:5000->127.0.0.2:8080");
        assert_eq!(result, None);

        let result = parse_remote_address("*:5000->10.0.0.1:8080");
        assert_eq!(result, Some(("10.0.0.1".to_string(), 8080)));
    }

    #[test]
    fn test_deduplication_suppresses_repeated_connections() {
        let mut observer = NetworkObserver::new(1, 100);

        let event_a = NetworkEvent {
            pid: 100,
            remote_host: "example.com".to_string(),
            remote_port: 443,
            protocol: Protocol::Tcp,
            timestamp: Utc::now(),
        };
        let event_b = NetworkEvent {
            pid: 100,
            remote_host: "other.io".to_string(),
            remote_port: 80,
            protocol: Protocol::Tcp,
            timestamp: Utc::now(),
        };

        // First poll: both connections are new
        let key_a = ConnectionKey {
            pid: event_a.pid,
            remote_host: event_a.remote_host.clone(),
            remote_port: event_a.remote_port,
            protocol: event_a.protocol.clone(),
        };
        let key_b = ConnectionKey {
            pid: event_b.pid,
            remote_host: event_b.remote_host.clone(),
            remote_port: event_b.remote_port,
            protocol: event_b.protocol.clone(),
        };

        assert!(observer.seen_connections.insert(key_a.clone()));
        assert!(observer.seen_connections.insert(key_b.clone()));

        // Second insert of the same keys returns false (already present)
        assert!(!observer.seen_connections.insert(key_a));
        assert!(!observer.seen_connections.insert(key_b));
    }

    #[test]
    fn test_deduplication_via_poll_with_lsof_output() {
        let mut observer = NetworkObserver::new(1, 100);

        let lsof_output = "\
COMMAND   PID USER   FD   TYPE  DEVICE SIZE/OFF NODE NAME
curl      100 user   3u   IPv4  12345      0t0  TCP  192.168.1.1:55000->93.184.216.34:443
curl      100 user   4u   IPv4  12346      0t0  TCP  192.168.1.1:55001->10.0.0.1:8080";

        // First parse: both connections are new
        let connections = observer.parse_lsof_output(lsof_output);
        assert_eq!(connections.len(), 2);

        // Feed them through the dedup logic
        let mut new_events = Vec::new();
        for conn in connections {
            let key = ConnectionKey {
                pid: conn.pid,
                remote_host: conn.remote_host.clone(),
                remote_port: conn.remote_port,
                protocol: conn.protocol.clone(),
            };
            if !observer.seen_connections.contains(&key) {
                observer.seen_connections.insert(key);
                new_events.push(conn);
            }
        }
        assert_eq!(new_events.len(), 2);
        assert_eq!(new_events[0].remote_host, "93.184.216.34");
        assert_eq!(new_events[1].remote_host, "10.0.0.1");

        // Second parse of the same output: all are duplicates
        let connections = observer.parse_lsof_output(lsof_output);
        assert_eq!(connections.len(), 2);

        let mut new_events = Vec::new();
        for conn in connections {
            let key = ConnectionKey {
                pid: conn.pid,
                remote_host: conn.remote_host.clone(),
                remote_port: conn.remote_port,
                protocol: conn.protocol.clone(),
            };
            if !observer.seen_connections.contains(&key) {
                observer.seen_connections.insert(key);
                new_events.push(conn);
            }
        }
        assert_eq!(new_events.len(), 0);

        // Third parse with one new connection: only the new one passes
        let lsof_with_new = "\
COMMAND   PID USER   FD   TYPE  DEVICE SIZE/OFF NODE NAME
curl      100 user   3u   IPv4  12345      0t0  TCP  192.168.1.1:55000->93.184.216.34:443
wget      101 user   5u   IPv4  12347      0t0  UDP  192.168.1.1:55002->8.8.8.8:53";

        let connections = observer.parse_lsof_output(lsof_with_new);
        assert_eq!(connections.len(), 2);

        let mut new_events = Vec::new();
        for conn in connections {
            let key = ConnectionKey {
                pid: conn.pid,
                remote_host: conn.remote_host.clone(),
                remote_port: conn.remote_port,
                protocol: conn.protocol.clone(),
            };
            if !observer.seen_connections.contains(&key) {
                observer.seen_connections.insert(key);
                new_events.push(conn);
            }
        }
        assert_eq!(new_events.len(), 1);
        assert_eq!(new_events[0].remote_host, "8.8.8.8");
        assert_eq!(new_events[0].remote_port, 53);
        assert_eq!(new_events[0].protocol, Protocol::Udp);
    }

    #[test]
    fn test_parse_lsof_handles_established_state_suffix() {
        let observer = NetworkObserver::new(1, 100);

        let lsof_output = "\
COMMAND   PID USER   FD   TYPE  DEVICE SIZE/OFF NODE NAME
ssh       200 user   3u   IPv4  99999      0t0  TCP  10.0.0.5:22->54.231.10.1:443 (ESTABLISHED)
curl      201 user   4u   IPv4  99998      0t0  TCP  10.0.0.5:33->1.2.3.4:8080 (CLOSE_WAIT)";

        let events = observer.parse_lsof_output(lsof_output);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].remote_host, "54.231.10.1");
        assert_eq!(events[0].remote_port, 443);
        assert_eq!(events[1].remote_host, "1.2.3.4");
        assert_eq!(events[1].remote_port, 8080);
    }
}
