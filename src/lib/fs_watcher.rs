use crate::config::Config;
use crate::events::{Event, FileEvent};
use chrono::Utc;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;

pub struct FsWatcher {
    workspace: PathBuf,
    config: Config,
    events: Arc<Mutex<Vec<Event>>>,
    read_paths: Arc<Mutex<HashSet<PathBuf>>>,
    stop_tx: Option<mpsc::Sender<()>>,
    handle: Option<thread::JoinHandle<()>>,
}

impl FsWatcher {
    pub fn new(workspace: PathBuf, config: Config) -> Self {
        Self {
            workspace,
            config,
            events: Arc::new(Mutex::new(Vec::new())),
            read_paths: Arc::new(Mutex::new(HashSet::new())),
            stop_tx: None,
            handle: None,
        }
    }

    pub fn start(&mut self) -> anyhow::Result<()> {
        let (stop_tx, stop_rx) = mpsc::channel();
        self.stop_tx = Some(stop_tx);

        let workspace = self.workspace.clone();
        let config = self.config.clone();
        let events = Arc::clone(&self.events);
        let read_paths = Arc::clone(&self.read_paths);

        let handle = thread::spawn(move || {
            run_polling_watcher(&workspace, &config, &events, &read_paths, &stop_rx);
        });

        self.handle = Some(handle);
        Ok(())
    }

    pub fn stop(&mut self) {
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send(());
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }

    pub fn drain_events(&self) -> Vec<Event> {
        let mut events = self.events.lock().unwrap();
        std::mem::take(&mut *events)
    }

    pub fn read_paths(&self) -> HashSet<PathBuf> {
        self.read_paths.lock().unwrap().clone()
    }
}

fn run_polling_watcher(
    workspace: &Path,
    config: &Config,
    events: &Arc<Mutex<Vec<Event>>>,
    _read_paths: &Arc<Mutex<HashSet<PathBuf>>>,
    stop_rx: &mpsc::Receiver<()>,
) {
    use std::collections::HashMap;
    use std::time::SystemTime;

    let poll_interval = std::time::Duration::from_millis(200);
    let mut known_files: HashMap<PathBuf, SystemTime> = HashMap::new();

    scan_directory(workspace, config, &mut known_files);

    loop {
        if stop_rx.try_recv().is_ok() {
            break;
        }

        std::thread::sleep(poll_interval);

        let mut current_files: HashMap<PathBuf, SystemTime> = HashMap::new();
        scan_directory(workspace, config, &mut current_files);

        let mut new_events = Vec::new();

        for (path, mtime) in &current_files {
            match known_files.get(path) {
                None => {
                    new_events.push(Event::FileCreated(FileEvent {
                        path: path.clone(),
                        timestamp: Utc::now(),
                        size_bytes: std::fs::metadata(path).ok().map(|m| m.len()),
                    }));
                }
                Some(old_mtime) => {
                    if mtime != old_mtime {
                        new_events.push(Event::FileModified(FileEvent {
                            path: path.clone(),
                            timestamp: Utc::now(),
                            size_bytes: std::fs::metadata(path).ok().map(|m| m.len()),
                        }));
                    }
                }
            }
        }

        for path in known_files.keys() {
            if !current_files.contains_key(path) {
                new_events.push(Event::FileDeleted(FileEvent {
                    path: path.clone(),
                    timestamp: Utc::now(),
                    size_bytes: None,
                }));
            }
        }

        if !new_events.is_empty() {
            let mut locked = events.lock().unwrap();
            locked.extend(new_events);
        }

        known_files = current_files;
    }
}

fn scan_directory(
    dir: &Path,
    config: &Config,
    files: &mut std::collections::HashMap<PathBuf, std::time::SystemTime>,
) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if config.should_exclude(&path) {
            continue;
        }
        if path.is_dir() {
            scan_directory(&path, config, files);
        } else if path.is_file() {
            if let Ok(metadata) = path.metadata() {
                if let Ok(mtime) = metadata.modified() {
                    files.insert(path, mtime);
                }
            }
        }
    }
}
