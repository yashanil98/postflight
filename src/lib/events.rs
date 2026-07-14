use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    FileCreated(FileEvent),
    FileModified(FileEvent),
    FileDeleted(FileEvent),
    FileRenamed(FileRenameEvent),
    FileRead(FileEvent),
    NetworkConnection(NetworkEvent),
    ProcessSpawned(ProcessEvent),
    ProcessExited(ProcessExitEvent),
    SessionStart(SessionStartEvent),
    SessionEnd(SessionEndEvent),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEvent {
    pub path: PathBuf,
    pub timestamp: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileRenameEvent {
    pub from: PathBuf,
    pub to: PathBuf,
    pub timestamp: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkEvent {
    pub pid: u32,
    pub remote_host: String,
    pub remote_port: u16,
    pub protocol: Protocol,
    pub timestamp: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    Tcp,
    Udp,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessEvent {
    pub pid: u32,
    pub ppid: u32,
    pub argv: Vec<String>,
    pub timestamp: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessExitEvent {
    pub pid: u32,
    pub exit_code: i32,
    pub duration: Duration,
    pub timestamp: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionStartEvent {
    pub command: String,
    pub workspace: PathBuf,
    pub timestamp: DateTime<Utc>,
    pub pid: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionEndEvent {
    pub exit_code: i32,
    pub duration: Duration,
    pub timestamp: DateTime<Utc>,
}

impl Event {
    pub fn timestamp(&self) -> DateTime<Utc> {
        match self {
            Self::FileCreated(e)
            | Self::FileModified(e)
            | Self::FileDeleted(e)
            | Self::FileRead(e) => e.timestamp,
            Self::FileRenamed(e) => e.timestamp,
            Self::NetworkConnection(e) => e.timestamp,
            Self::ProcessSpawned(e) => e.timestamp,
            Self::ProcessExited(e) => e.timestamp,
            Self::SessionStart(e) => e.timestamp,
            Self::SessionEnd(e) => e.timestamp,
        }
    }
}
