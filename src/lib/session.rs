use crate::config::Config;
use crate::events::Event;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSummary {
    pub id: String,
    pub command: String,
    pub workspace: PathBuf,
    pub start_time: DateTime<Utc>,
    pub duration: Duration,
    pub exit_code: i32,
    pub files_created: Vec<PathBuf>,
    pub files_modified: Vec<PathBuf>,
    pub files_deleted: Vec<PathBuf>,
    pub files_read: Vec<PathBuf>,
    pub network_connections: Vec<ConnectionSummary>,
    pub subprocesses: Vec<SubprocessSummary>,
    pub total_bytes_written: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectionSummary {
    pub remote_host: String,
    pub remote_port: u16,
    pub protocol: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubprocessSummary {
    pub argv: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    pub duration: Duration,
}

pub struct Session {
    pub id: String,
    pub dir: PathBuf,
    pub start_time: DateTime<Utc>,
    events_file: std::fs::File,
}

impl Session {
    pub fn create(command: &str, workspace: &Path) -> Result<Self> {
        let now = Utc::now();
        let base_id = now.format("%Y%m%d_%H%M%S").to_string();
        let millis = now.format("%3f").to_string();
        let id = format!("{base_id}_{millis}");
        let dir = Config::sessions_dir().join(&id);
        fs::create_dir_all(&dir).context("failed to create session directory")?;
        fs::create_dir_all(dir.join("diffs")).context("failed to create diffs directory")?;

        let events_file = fs::File::create(dir.join("events.jsonl"))
            .context("failed to create events.jsonl")?;

        let session = Self {
            id,
            dir,
            start_time: now,
            events_file,
        };

        let start_event = Event::SessionStart(crate::events::SessionStartEvent {
            command: command.to_string(),
            workspace: workspace.to_path_buf(),
            timestamp: now,
            pid: std::process::id(),
        });
        session.write_event_to_file(&start_event)?;

        Ok(session)
    }

    pub fn write_event(&mut self, event: &Event) -> Result<()> {
        self.write_event_to_file(event)
    }

    fn write_event_to_file(&self, event: &Event) -> Result<()> {
        let mut file = &self.events_file;
        let json = serde_json::to_string(event)?;
        writeln!(file, "{json}")?;
        Ok(())
    }

    pub fn write_terminal_chunk(&self, data: &[u8]) -> Result<()> {
        const MAX_TERMINAL_RAW_BYTES: u64 = 50 * 1024 * 1024;

        let path = self.dir.join("terminal.raw");
        if let Ok(meta) = fs::metadata(&path) {
            if meta.len() >= MAX_TERMINAL_RAW_BYTES {
                return Ok(());
            }
        }
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        file.write_all(data)?;
        Ok(())
    }

    pub fn save_diff(&self, filename: &str, diff_content: &str) -> Result<()> {
        let safe_name = filename.replace('/', "_");
        let path = self.dir.join("diffs").join(format!("{safe_name}.diff"));
        fs::write(path, diff_content)?;
        Ok(())
    }

    pub fn save_summary(&self, summary: &SessionSummary) -> Result<()> {
        let path = self.dir.join("summary.json");
        let json = serde_json::to_string_pretty(summary)?;
        fs::write(path, json)?;
        Ok(())
    }

    pub fn load_events(session_dir: &Path) -> Result<Vec<Event>> {
        let events_path = session_dir.join("events.jsonl");
        let file = fs::File::open(&events_path)
            .with_context(|| format!("failed to open {}", events_path.display()))?;
        let reader = BufReader::new(file);
        let mut events = Vec::new();
        for line in reader.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let event: Event = serde_json::from_str(&line)
                .with_context(|| format!("failed to parse event: {line}"))?;
            events.push(event);
        }
        Ok(events)
    }

    pub fn load_summary(session_dir: &Path) -> Result<SessionSummary> {
        let path = session_dir.join("summary.json");
        let content = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let summary: SessionSummary = serde_json::from_str(&content)?;
        Ok(summary)
    }

    pub fn list_sessions() -> Result<Vec<(String, PathBuf)>> {
        let sessions_dir = Config::sessions_dir();
        if !sessions_dir.exists() {
            return Ok(Vec::new());
        }

        let mut sessions: Vec<(String, PathBuf)> = Vec::new();
        for entry in fs::read_dir(&sessions_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    sessions.push((name.to_string(), path));
                }
            }
        }
        sessions.sort_by(|a, b| b.0.cmp(&a.0));
        Ok(sessions)
    }

    pub fn latest_session() -> Result<Option<PathBuf>> {
        let sessions = Self::list_sessions()?;
        Ok(sessions
            .into_iter()
            .find(|(_, path)| path.join("summary.json").exists())
            .map(|(_, path)| path))
    }

    pub fn prune_sessions(keep: usize) -> Result<usize> {
        let sessions = Self::list_sessions()?;
        let mut removed = 0;
        if sessions.len() > keep {
            for (_, path) in sessions.into_iter().skip(keep) {
                let has_summary = path.join("summary.json").exists();
                let has_events = path.join("events.jsonl").exists();

                if !has_summary && has_events {
                    let stale = is_stale_session(&path);
                    if !stale {
                        continue;
                    }
                }

                let _ = fs::remove_dir_all(&path);
                removed += 1;
            }
        }
        Ok(removed)
    }
}

fn is_stale_session(session_dir: &Path) -> bool {
    let events_path = session_dir.join("events.jsonl");
    let mtime = match fs::metadata(&events_path).and_then(|m| m.modified()) {
        Ok(t) => t,
        Err(_) => return true,
    };
    match mtime.elapsed() {
        Ok(age) => age > Duration::from_secs(60),
        Err(_) => false,
    }
}
