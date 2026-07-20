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
            #[cfg(target_os = "macos")]
            run_polling_watcher(&workspace, &config, &events, &read_paths, &stop_rx);

            #[cfg(target_os = "linux")]
            run_inotify_watcher(&workspace, &config, &events, &read_paths, &stop_rx);
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

impl Drop for FsWatcher {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(target_os = "macos")]
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
        if stop_rx.try_recv() != Err(std::sync::mpsc::TryRecvError::Empty) {
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
        let file_type = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };
        if file_type.is_dir() {
            scan_directory(&path, config, files);
        } else if file_type.is_file() || (file_type.is_symlink() && path.is_file()) {
            if let Ok(metadata) = path.metadata() {
                if let Ok(mtime) = metadata.modified() {
                    files.insert(path, mtime);
                }
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn run_inotify_watcher(
    workspace: &Path,
    config: &Config,
    events: &Arc<Mutex<Vec<Event>>>,
    read_paths: &Arc<Mutex<HashSet<PathBuf>>>,
    stop_rx: &mpsc::Receiver<()>,
) {
    use inotify::{EventMask, Inotify, WatchDescriptor, WatchMask};
    use std::collections::HashMap;
    use std::os::fd::AsRawFd;

    let mut inotify = match Inotify::init() {
        Ok(i) => i,
        Err(_) => return,
    };

    // Set inotify fd to non-blocking so read_events returns WouldBlock
    // when no events are available, allowing the stop check to run.
    unsafe {
        let fd = inotify.as_raw_fd();
        let flags = libc::fcntl(fd, libc::F_GETFL);
        libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
    }

    let watch_mask = WatchMask::CREATE
        | WatchMask::MODIFY
        | WatchMask::DELETE
        | WatchMask::MOVED_FROM
        | WatchMask::MOVED_TO
        | WatchMask::ACCESS;

    let mut wd_to_path: HashMap<WatchDescriptor, PathBuf> = HashMap::new();

    add_watches_recursive(&mut inotify, workspace, config, watch_mask, &mut wd_to_path);

    let mut buffer = [0u8; 4096];

    loop {
        if stop_rx.try_recv() != Err(std::sync::mpsc::TryRecvError::Empty) {
            break;
        }

        if let Ok(events_iter) = inotify.read_events(&mut buffer) {
            let mut new_events = Vec::new();
            for event in events_iter {
                let parent_dir = match wd_to_path.get(&event.wd) {
                    Some(p) => p.clone(),
                    None => continue,
                };

                if let Some(name) = event.name {
                    let path = parent_dir.join(name.to_string_lossy().as_ref());
                    if config.should_exclude(&path) {
                        continue;
                    }

                    if event.mask.contains(EventMask::ISDIR)
                        && event.mask.contains(EventMask::CREATE)
                    {
                        add_watches_recursive(
                            &mut inotify, &path, config, watch_mask, &mut wd_to_path,
                        );
                    }

                    if event.mask.contains(EventMask::CREATE) {
                        new_events.push(Event::FileCreated(FileEvent {
                            path: path.clone(),
                            timestamp: Utc::now(),
                            size_bytes: std::fs::metadata(&path).ok().map(|m| m.len()),
                        }));
                    } else if event.mask.contains(EventMask::MODIFY) {
                        new_events.push(Event::FileModified(FileEvent {
                            path: path.clone(),
                            timestamp: Utc::now(),
                            size_bytes: std::fs::metadata(&path).ok().map(|m| m.len()),
                        }));
                    } else if event.mask.contains(EventMask::DELETE) {
                        new_events.push(Event::FileDeleted(FileEvent {
                            path,
                            timestamp: Utc::now(),
                            size_bytes: None,
                        }));
                    } else if event.mask.contains(EventMask::ACCESS) {
                        read_paths.lock().unwrap().insert(path);
                    }
                }
            }
            if !new_events.is_empty() {
                events.lock().unwrap().extend(new_events);
            }
        }

        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

#[cfg(target_os = "linux")]
fn add_watches_recursive(
    inotify: &mut inotify::Inotify,
    dir: &Path,
    config: &Config,
    mask: inotify::WatchMask,
    wd_map: &mut std::collections::HashMap<inotify::WatchDescriptor, PathBuf>,
) {
    if config.should_exclude(dir) {
        return;
    }

    if let Ok(wd) = inotify.watches().add(dir, mask) {
        wd_map.insert(wd, dir.to_path_buf());
    }

    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() && !config.should_exclude(&path) {
            add_watches_recursive(inotify, &path, config, mask, wd_map);
        }
    }
}
