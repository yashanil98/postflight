use bytesize::ByteSize;
use crate::session::SessionSummary;
use colored::Colorize;
use std::time::Duration;

pub fn render_terminal(summary: &SessionSummary, _show_diffs: bool) -> String {
    let mut output = String::new();

    output.push_str(&format!(
        "\n{}\n",
        "━━━ postflight session report ━━━".bold()
    ));
    output.push_str(&format!(
        "  {} {}\n",
        "command:".dimmed(),
        summary.command.bold()
    ));
    output.push_str(&format!(
        "  {} {}\n",
        "workspace:".dimmed(),
        summary.workspace.display()
    ));
    output.push_str(&format!(
        "  {} {}\n",
        "duration:".dimmed(),
        format_duration(summary.duration)
    ));
    output.push_str(&format!(
        "  {} {}\n",
        "exit code:".dimmed(),
        format_exit_code(summary.exit_code)
    ));
    output.push('\n');

    let total_file_ops = summary.files_created.len()
        + summary.files_modified.len()
        + summary.files_deleted.len();

    if total_file_ops > 0 {
        output.push_str(&format!("{}\n", "files changed".bold().underline()));

        if !summary.files_created.is_empty() {
            output.push_str(&format!(
                "  {} ({})\n",
                "created".green().bold(),
                summary.files_created.len()
            ));
            for path in &summary.files_created {
                let size = file_size_label(path);
                output.push_str(&format!("    {} {} {}\n", "+".green(), path.display(), size.dimmed()));
            }
        }

        if !summary.files_modified.is_empty() {
            output.push_str(&format!(
                "  {} ({})\n",
                "modified".yellow().bold(),
                summary.files_modified.len()
            ));
            for path in &summary.files_modified {
                let size = file_size_label(path);
                output.push_str(&format!("    {} {} {}\n", "~".yellow(), path.display(), size.dimmed()));
            }
        }

        if !summary.files_deleted.is_empty() {
            output.push_str(&format!(
                "  {} ({})\n",
                "deleted".red().bold(),
                summary.files_deleted.len()
            ));
            for path in &summary.files_deleted {
                output.push_str(&format!("    {} {}\n", "-".red(), path.display()));
            }
        }
        output.push('\n');
    }

    if !summary.files_read.is_empty() {
        output.push_str(&format!("{}\n", "files read".bold().underline()));
        let grouped = group_by_directory(&summary.files_read);
        for (dir, files) in &grouped {
            output.push_str(&format!("  {} ({} files)\n", dir.dimmed(), files.len()));
            for file in files {
                output.push_str(&format!("    {file}\n"));
            }
        }
        output.push('\n');
    }

    if !summary.network_connections.is_empty() {
        output.push_str(&format!("{}\n", "network connections".bold().underline()));
        for conn in &summary.network_connections {
            output.push_str(&format!(
                "  {} {}:{} ({})\n",
                "\u{2192}".cyan(),
                conn.remote_host,
                conn.remote_port,
                conn.protocol
            ));
        }
        output.push('\n');
    }

    if !summary.subprocesses.is_empty() {
        output.push_str(&format!("{}\n", "subprocesses".bold().underline()));
        for proc in &summary.subprocesses {
            let argv_str = proc.argv.join(" ");
            let truncated = if argv_str.len() > 80 {
                format!("{}...", &argv_str[..77])
            } else {
                argv_str
            };
            output.push_str(&format!(
                "  {} {} {} {}\n",
                "\u{25b8}".dimmed(),
                truncated,
                format!("[exit:{}]", proc.exit_code).dimmed(),
                format!("({})", format_duration(proc.duration)).dimmed()
            ));
        }
        output.push('\n');
    }

    output.push_str(&format!("{}\n", "\u{2501}\u{2501}\u{2501} verdict \u{2501}\u{2501}\u{2501}".bold()));
    output.push_str(&format!(
        "  touched {} files in {}, read {} files, {} network connections, {} subprocesses, ran for {}\n",
        total_file_ops,
        summarize_directories(&summary.files_created, &summary.files_modified, &summary.files_deleted),
        summary.files_read.len(),
        summary.network_connections.len(),
        summary.subprocesses.len(),
        format_duration(summary.duration),
    ));

    if summary.total_bytes_written > 0 {
        output.push_str(&format!(
            "  total disk writes: {}\n",
            format_bytes(summary.total_bytes_written)
        ));
    }

    output.push('\n');
    output
}

pub fn render_json(summary: &SessionSummary) -> String {
    serde_json::to_string_pretty(summary).unwrap_or_else(|_| "{}".to_string())
}

fn format_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else {
        format!("{}h {}m {}s", secs / 3600, (secs % 3600) / 60, secs % 60)
    }
}

fn format_exit_code(code: i32) -> String {
    if code == 0 {
        format!("{}", "0".green())
    } else {
        format!("{}", code.to_string().red())
    }
}

fn format_bytes(bytes: u64) -> String {
    ByteSize(bytes).to_string()
}

fn file_size_label(path: &std::path::Path) -> String {
    match std::fs::metadata(path) {
        Ok(meta) => format!("({})", ByteSize(meta.len())),
        Err(_) => String::new(),
    }
}

fn group_by_directory(paths: &[std::path::PathBuf]) -> Vec<(String, Vec<String>)> {
    use std::collections::BTreeMap;

    let mut groups: BTreeMap<String, Vec<String>> = BTreeMap::new();

    for path in paths {
        let dir = path
            .parent()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| ".".to_string());
        let filename = path
            .file_name()
            .map(|f| f.to_string_lossy().to_string())
            .unwrap_or_else(|| path.to_string_lossy().to_string());
        groups.entry(dir).or_default().push(filename);
    }

    groups.into_iter().collect()
}

fn summarize_directories(
    created: &[std::path::PathBuf],
    modified: &[std::path::PathBuf],
    deleted: &[std::path::PathBuf],
) -> String {
    use std::collections::BTreeSet;

    let dirs: BTreeSet<String> = created
        .iter()
        .chain(modified.iter())
        .chain(deleted.iter())
        .filter_map(|p| p.parent())
        .map(|p| {
            let s = p.to_string_lossy().to_string();
            let parts: Vec<&str> = s.split('/').collect();
            if parts.len() > 2 {
                format!(".../{}", parts[parts.len() - 2..].join("/"))
            } else {
                s
            }
        })
        .collect();

    if dirs.is_empty() {
        ".".to_string()
    } else if dirs.len() <= 3 {
        dirs.into_iter().collect::<Vec<_>>().join(", ")
    } else {
        let first_three: Vec<_> = dirs.iter().take(3).cloned().collect();
        format!("{} (+{} more)", first_three.join(", "), dirs.len() - 3)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_duration() {
        assert_eq!(format_duration(Duration::from_secs(5)), "5s");
        assert_eq!(format_duration(Duration::from_secs(65)), "1m 5s");
        assert_eq!(format_duration(Duration::from_secs(3665)), "1h 1m 5s");
    }

    #[test]
    fn test_format_bytes() {
        assert_eq!(format_bytes(500), "500 B");
        assert_eq!(format_bytes(1536), "1.5 KiB");
        assert_eq!(format_bytes(1_572_864), "1.5 MiB");
    }
}
