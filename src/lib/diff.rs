use similar::TextDiff;
use std::collections::HashMap;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

pub struct SnapshotEntry {
    pub mtime: SystemTime,
    pub size: u64,
    pub content_hash: u64,
}

pub type FileSnapshot = HashMap<PathBuf, SnapshotEntry>;

fn hash_file_content(path: &Path) -> u64 {
    match fs::read(path) {
        Ok(bytes) => {
            let mut hasher = std::hash::DefaultHasher::new();
            bytes.hash(&mut hasher);
            hasher.finish()
        }
        Err(_) => 0,
    }
}

pub fn capture_snapshot(root: &Path, exclude: &dyn Fn(&Path) -> bool) -> FileSnapshot {
    let mut snapshot = HashMap::new();
    walk_dir(root, exclude, &mut snapshot);
    snapshot
}

fn walk_dir(dir: &Path, exclude: &dyn Fn(&Path) -> bool, snapshot: &mut FileSnapshot) {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if exclude(&path) {
            continue;
        }
        let file_type = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };
        if file_type.is_dir() {
            walk_dir(&path, exclude, snapshot);
        } else if file_type.is_file() {
            if let Ok(metadata) = path.metadata() {
                if let Ok(mtime) = metadata.modified() {
                    let content_hash = hash_file_content(&path);
                    snapshot.insert(
                        path,
                        SnapshotEntry {
                            mtime,
                            size: metadata.len(),
                            content_hash,
                        },
                    );
                }
            }
        } else if file_type.is_symlink() {
            // Follow symlinks to files for content tracking, but never
            // recurse into symlinked directories to avoid infinite loops.
            if path.is_file() {
                if let Ok(metadata) = path.metadata() {
                    if let Ok(mtime) = metadata.modified() {
                        let content_hash = hash_file_content(&path);
                        snapshot.insert(
                            path,
                            SnapshotEntry {
                                mtime,
                                size: metadata.len(),
                                content_hash,
                            },
                        );
                    }
                }
            }
        }
    }
}

pub type ContentSnapshot = HashMap<PathBuf, String>;

const MAX_DIFF_FILE_SIZE: u64 = 512 * 1024;

pub fn capture_content(snapshot: &FileSnapshot) -> ContentSnapshot {
    let mut contents = HashMap::new();
    for (path, entry) in snapshot {
        if entry.size > MAX_DIFF_FILE_SIZE {
            continue;
        }
        if let Ok(text) = fs::read_to_string(path) {
            contents.insert(path.clone(), text);
        }
    }
    contents
}

pub struct DiffResult {
    pub created: Vec<PathBuf>,
    pub modified: Vec<PathBuf>,
    pub deleted: Vec<PathBuf>,
}

pub fn diff_snapshots(before: &FileSnapshot, after: &FileSnapshot) -> DiffResult {
    let mut created = Vec::new();
    let mut modified = Vec::new();
    let mut deleted = Vec::new();

    for (path, after_entry) in after {
        match before.get(path) {
            None => created.push(path.clone()),
            Some(before_entry) => {
                if before_entry.content_hash != after_entry.content_hash {
                    modified.push(path.clone());
                }
            }
        }
    }

    for path in before.keys() {
        if !after.contains_key(path) {
            deleted.push(path.clone());
        }
    }

    created.sort();
    modified.sort();
    deleted.sort();

    DiffResult {
        created,
        modified,
        deleted,
    }
}

pub fn generate_unified_diff(old_content: &str, new_content: &str, path: &Path) -> String {
    generate_unified_diff_relative(old_content, new_content, path, None)
}

pub fn generate_unified_diff_relative(
    old_content: &str,
    new_content: &str,
    path: &Path,
    workspace: Option<&Path>,
) -> String {
    let diff = TextDiff::from_lines(old_content, new_content);
    let rel_path = workspace
        .and_then(|ws| path.strip_prefix(ws).ok())
        .unwrap_or(path);
    let path_str = rel_path.to_string_lossy();

    let mut output = String::new();
    output.push_str(&format!("--- a/{path_str}\n"));
    output.push_str(&format!("+++ b/{path_str}\n"));

    for hunk in diff.unified_diff().context_radius(3).iter_hunks() {
        output.push_str(&format!("{hunk}"));
    }

    output
}

pub fn format_diff_colored(content: &str) -> String {
    use colored::Colorize;

    let mut output = String::new();
    for line in content.lines() {
        if line.starts_with("--- ") || line.starts_with("+++ ") {
            output.push_str(&format!("{}\n", line.bold()));
        } else if line.starts_with("@@") {
            output.push_str(&format!("{}\n", line.cyan()));
        } else if line.starts_with('+') {
            output.push_str(&format!("{}\n", line.green()));
        } else if line.starts_with('-') {
            output.push_str(&format!("{}\n", line.red()));
        } else {
            output.push_str(&format!("{line}\n"));
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_unified_diff() {
        let old = "line1\nline2\nline3\n";
        let new = "line1\nmodified\nline3\n";
        let diff = generate_unified_diff(old, new, Path::new("test.txt"));
        assert!(diff.contains("--- a/test.txt"));
        assert!(diff.contains("+++ b/test.txt"));
        assert!(diff.contains("-line2"));
        assert!(diff.contains("+modified"));
    }

    #[test]
    fn test_unified_diff_uses_relative_path() {
        let old = "aaa\n";
        let new = "bbb\n";
        let abs_path = Path::new("/home/user/project/src/main.rs");
        let workspace = Path::new("/home/user/project");
        let diff = generate_unified_diff_relative(old, new, abs_path, Some(workspace));
        assert!(diff.contains("--- a/src/main.rs"));
        assert!(diff.contains("+++ b/src/main.rs"));
        assert!(!diff.contains("/home/user/project"));
    }

    #[test]
    fn test_diff_snapshots_detects_changes() {
        let mut before = FileSnapshot::new();
        let mut after = FileSnapshot::new();

        let time1 = SystemTime::UNIX_EPOCH;
        let time2 = SystemTime::now();

        before.insert(
            PathBuf::from("/a.txt"),
            SnapshotEntry {
                mtime: time1,
                size: 100,
                content_hash: 111,
            },
        );
        before.insert(
            PathBuf::from("/b.txt"),
            SnapshotEntry {
                mtime: time1,
                size: 50,
                content_hash: 222,
            },
        );

        after.insert(
            PathBuf::from("/a.txt"),
            SnapshotEntry {
                mtime: time2,
                size: 120,
                content_hash: 333,
            },
        );
        after.insert(
            PathBuf::from("/c.txt"),
            SnapshotEntry {
                mtime: time2,
                size: 30,
                content_hash: 444,
            },
        );

        let result = diff_snapshots(&before, &after);
        assert_eq!(result.modified, vec![PathBuf::from("/a.txt")]);
        assert_eq!(result.created, vec![PathBuf::from("/c.txt")]);
        assert_eq!(result.deleted, vec![PathBuf::from("/b.txt")]);
    }

    #[test]
    fn test_diff_snapshots_same_mtime_different_content() {
        let mut before = FileSnapshot::new();
        let mut after = FileSnapshot::new();

        let time = SystemTime::UNIX_EPOCH;

        before.insert(
            PathBuf::from("/sneaky.txt"),
            SnapshotEntry {
                mtime: time,
                size: 10,
                content_hash: 100,
            },
        );

        after.insert(
            PathBuf::from("/sneaky.txt"),
            SnapshotEntry {
                mtime: time,
                size: 10,
                content_hash: 200,
            },
        );

        let result = diff_snapshots(&before, &after);
        assert_eq!(result.modified, vec![PathBuf::from("/sneaky.txt")]);
        assert!(result.created.is_empty());
        assert!(result.deleted.is_empty());
    }

    #[test]
    fn test_diff_snapshots_same_content_no_false_positive() {
        let mut before = FileSnapshot::new();
        let mut after = FileSnapshot::new();

        let time1 = SystemTime::UNIX_EPOCH;
        let time2 = SystemTime::now();

        before.insert(
            PathBuf::from("/stable.txt"),
            SnapshotEntry {
                mtime: time1,
                size: 10,
                content_hash: 999,
            },
        );

        after.insert(
            PathBuf::from("/stable.txt"),
            SnapshotEntry {
                mtime: time2,
                size: 10,
                content_hash: 999,
            },
        );

        let result = diff_snapshots(&before, &after);
        assert!(result.modified.is_empty());
    }

    #[test]
    fn test_capture_snapshot_skips_symlink_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let real_file = dir.path().join("real.txt");
        fs::write(&real_file, "content").unwrap();

        let link_dir = dir.path().join("loop_link");
        std::os::unix::fs::symlink(dir.path(), &link_dir).unwrap();

        let snapshot = capture_snapshot(dir.path(), &|_| false);

        assert!(snapshot.contains_key(&real_file));
        let loop_file = link_dir.join("real.txt");
        assert!(
            !snapshot.contains_key(&loop_file),
            "symlinked directory should not be recursed into"
        );
    }
}
