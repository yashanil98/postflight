use postflight::config::Config;
use postflight::diff::{capture_content, capture_snapshot, diff_snapshots, generate_unified_diff_relative};
use postflight::events::{Event, SessionEndEvent};
use postflight::fs_watcher::FsWatcher;
use postflight::network::NetworkObserver;
use postflight::process::ProcessTracker;
use postflight::pty::PtyChild;
use postflight::report;
use postflight::session::{ConnectionSummary, Session, SessionSummary, SubprocessSummary};
use postflight::shutdown::GracefulShutdown;
use anyhow::{Context, Result};
use chrono::Utc;
use clap::{Parser, Subcommand};
use std::collections::HashSet;
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

#[derive(Parser)]
#[command(name = "postflight", version, about = "Flight recorder for AI coding agents")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Run a command and record everything it does
    Run {
        /// The command to execute (wrap in quotes if it has spaces)
        command: String,

        /// Workspace root to observe (defaults to current directory)
        #[arg(short, long)]
        workspace: Option<PathBuf>,

        /// Suppress the report printed after the session ends
        #[arg(short, long)]
        quiet: bool,

        /// Print the session summary as JSON to stdout after the run
        #[arg(long)]
        json: bool,
    },

    /// Show the report for a session
    Report {
        /// Session ID (timestamp). Defaults to latest.
        #[arg(short, long)]
        session: Option<String>,

        /// Output as JSON
        #[arg(long)]
        json: bool,

        /// Include file diffs in output
        #[arg(long)]
        diff: bool,
    },

    /// List stored sessions
    Sessions {
        /// Filter sessions by command substring
        #[arg(short, long)]
        filter: Option<String>,

        /// Show only sessions with non-zero exit codes
        #[arg(long)]
        failed: bool,
    },

    /// Remove old sessions
    Clean {
        /// Number of sessions to keep (default: from config or 20)
        #[arg(short, long)]
        keep: Option<usize>,
    },

    /// Generate a default config file at ~/.postflight/config.toml
    Init,

    /// Replay the terminal output of a recorded session
    Replay {
        /// Session ID (timestamp). Defaults to latest.
        #[arg(short, long)]
        session: Option<String>,
    },

    /// Gracefully stop a running session (sends wrap-up message, then SIGTERM after grace period)
    Stop {
        /// Session ID to stop. Defaults to the most recent active session.
        #[arg(short, long)]
        session: Option<String>,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Run { command, workspace, quiet, json } => cmd_run(&command, workspace, quiet, json),
        Commands::Report { session, json, diff } => cmd_report(session, json, diff),
        Commands::Sessions { filter, failed } => cmd_sessions(filter, failed),
        Commands::Clean { keep } => cmd_clean(keep),
        Commands::Init => cmd_init(),
        Commands::Replay { session } => cmd_replay(session),
        Commands::Stop { session } => cmd_stop(session),
    }
}

fn cmd_run(command: &str, workspace_override: Option<PathBuf>, quiet: bool, json: bool) -> Result<()> {
    let config = Config::load()?;
    let workspace = workspace_override
        .or_else(|| config.workspace_root.clone())
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

    let workspace = workspace.canonicalize().unwrap_or(workspace);

    if !quiet {
        eprintln!(
            "{} recording session for: {}",
            colored::Colorize::dimmed("postflight:"),
            colored::Colorize::bold(command)
        );
        eprintln!(
            "{} workspace: {}",
            colored::Colorize::dimmed("postflight:"),
            workspace.display()
        );
    }

    let mut session = Session::create(command, &workspace)?;

    let exclude_fn = |p: &std::path::Path| config.should_exclude(p);
    let pre_snapshot = capture_snapshot(&workspace, &exclude_fn);
    let pre_content = capture_content(&pre_snapshot);

    let mut fs_watcher = FsWatcher::new(workspace.clone(), config.clone());
    fs_watcher.start()?;

    let child = PtyChild::spawn(command).context("failed to spawn command")?;
    let child_pid = child.pid.as_raw() as u32;

    // Forward SIGINT/SIGTERM to the child process so Ctrl+C kills the child,
    // not postflight. The session still gets saved after the child exits.
    // The child_alive flag prevents signaling a recycled PID after exit.
    let child_alive = Arc::new(AtomicBool::new(true));

    let child_pid_for_signal = child.pid;
    let alive_for_int = Arc::clone(&child_alive);
    unsafe {
        signal_hook::low_level::register(signal_hook::consts::SIGINT, move || {
            if alive_for_int.load(Ordering::Relaxed) {
                let _ = nix::sys::signal::kill(child_pid_for_signal, nix::sys::signal::Signal::SIGINT);
            }
        })
    }
    .ok();
    let child_pid_for_term = child.pid;
    let alive_for_term = Arc::clone(&child_alive);
    unsafe {
        signal_hook::low_level::register(signal_hook::consts::SIGTERM, move || {
            if alive_for_term.load(Ordering::Relaxed) {
                let _ = nix::sys::signal::kill(child_pid_for_term, nix::sys::signal::Signal::SIGTERM);
            }
        })
    }
    .ok();

    let mut process_tracker = ProcessTracker::new(child_pid, config.process_poll_interval_ms);
    let mut network_observer = NetworkObserver::new(child_pid, config.network_poll_interval_ms);

    let running = Arc::new(AtomicBool::new(true));

    let running_proc = Arc::clone(&running);
    let (proc_tx, proc_rx) = std::sync::mpsc::channel();
    let proc_poll_interval = process_tracker.poll_interval();

    let proc_handle = thread::spawn(move || {
        while running_proc.load(Ordering::Relaxed) {
            let (spawns, exits) = process_tracker.poll();
            for event in spawns {
                let _ = proc_tx.send(Event::ProcessSpawned(event));
            }
            for event in exits {
                let _ = proc_tx.send(Event::ProcessExited(event));
            }
            thread::sleep(proc_poll_interval);
        }
        for event in process_tracker.finish_all() {
            let _ = proc_tx.send(Event::ProcessExited(event));
        }
    });

    let running_net = Arc::clone(&running);
    let (net_tx, net_rx) = std::sync::mpsc::channel();
    let net_poll_interval = network_observer.poll_interval();

    let net_handle = thread::spawn(move || {
        while running_net.load(Ordering::Relaxed) {
            let events = network_observer.poll();
            for event in events {
                let _ = net_tx.send(Event::NetworkConnection(event));
            }
            thread::sleep(net_poll_interval);
        }
    });

    let mut shutdown_watchdog = GracefulShutdown::new(
        session.dir.clone(),
        config.max_duration_secs.map(Duration::from_secs),
        Duration::from_secs(config.grace_period_secs),
        config.shutdown_message.clone(),
        child.pid,
        child.primary_fd.as_raw_fd(),
        Arc::clone(&child_alive),
    );
    shutdown_watchdog.start(std::time::Instant::now());

    let pty_result = child.wait_with_output(|data| {
        let _ = session.write_terminal_chunk(data);
        if json && quiet {
            // --json --quiet: suppress child output entirely, only emit JSON
        } else if json {
            let _ = std::io::Write::write_all(&mut std::io::stderr(), data);
        } else {
            let _ = std::io::Write::write_all(&mut std::io::stdout(), data);
            let _ = std::io::Write::flush(&mut std::io::stdout());
        }
    })?;

    child_alive.store(false, Ordering::Relaxed);
    shutdown_watchdog.stop();
    running.store(false, Ordering::Relaxed);

    let _ = proc_handle.join();
    let _ = net_handle.join();

    fs_watcher.stop();

    while let Ok(event) = proc_rx.try_recv() {
        session.write_event(&event)?;
    }
    while let Ok(event) = net_rx.try_recv() {
        session.write_event(&event)?;
    }

    let fs_events = fs_watcher.drain_events();
    for event in &fs_events {
        session.write_event(event)?;
    }

    let post_snapshot = capture_snapshot(&workspace, &exclude_fn);
    let snapshot_diff = diff_snapshots(&pre_snapshot, &post_snapshot);

    let files_created: Vec<PathBuf> = snapshot_diff.created;
    let files_modified: Vec<PathBuf> = snapshot_diff.modified;
    let files_deleted: Vec<PathBuf> = snapshot_diff.deleted;

    // Generate unified diffs for modified text files
    for path in &files_modified {
        if let Some(old_text) = pre_content.get(path) {
            if let Ok(new_text) = std::fs::read_to_string(path) {
                let diff_content =
                    generate_unified_diff_relative(old_text, &new_text, path, Some(&workspace));
                if !diff_content.trim().is_empty() {
                    let filename = path.to_string_lossy();
                    session.save_diff(&filename, &diff_content)?;
                }
            }
        }
    }

    let files_read: Vec<PathBuf> = fs_watcher.read_paths().into_iter().collect();

    let total_bytes_written: u64 = files_created
        .iter()
        .chain(files_modified.iter())
        .filter_map(|p| std::fs::metadata(p).ok())
        .map(|m| m.len())
        .sum();

    let mut network_connections = Vec::new();
    let mut seen_conns: HashSet<(String, u16, String)> = HashSet::new();
    if let Ok(all_events) = Session::load_events(&session.dir) {
        for event in &all_events {
            if let Event::NetworkConnection(e) = event {
                let key = (e.remote_host.clone(), e.remote_port, format!("{:?}", e.protocol));
                if seen_conns.insert(key) {
                    network_connections.push(ConnectionSummary {
                        remote_host: e.remote_host.clone(),
                        remote_port: e.remote_port,
                        protocol: format!("{:?}", e.protocol).to_lowercase(),
                    });
                }
            }
        }
    }

    let mut subprocesses = Vec::new();
    if let Ok(all_events) = Session::load_events(&session.dir) {
        for event in &all_events {
            if let Event::ProcessExited(e) = event {
                let argv = all_events
                    .iter()
                    .find_map(|ev| {
                        if let Event::ProcessSpawned(s) = ev {
                            if s.pid == e.pid {
                                return Some(s.argv.clone());
                            }
                        }
                        None
                    })
                    .unwrap_or_else(|| vec![format!("<pid:{}>", e.pid)]);

                subprocesses.push(SubprocessSummary {
                    argv,
                    exit_code: e.exit_code,
                    duration: e.duration,
                });

            }
        }
    }

    let end_event = Event::SessionEnd(SessionEndEvent {
        exit_code: pty_result.exit_code,
        duration: pty_result.duration,
        timestamp: Utc::now(),
    });
    session.write_event(&end_event)?;

    let summary = SessionSummary {
        id: session.id.clone(),
        command: command.to_string(),
        workspace,
        start_time: session.start_time,
        duration: pty_result.duration,
        exit_code: pty_result.exit_code,
        files_created,
        files_modified,
        files_deleted,
        files_read,
        network_connections,
        subprocesses,
        total_bytes_written,
    };

    session.save_summary(&summary)?;

    if json {
        println!("{}", report::render_json(&summary));
    } else if !quiet {
        eprintln!();
        let report_output = report::render_terminal(&summary, false);
        eprint!("{report_output}");
    }

    if !quiet {
        eprintln!(
            "{} session saved to {}",
            colored::Colorize::dimmed("postflight:"),
            session.dir.display()
        );
    }

    let pruned = Session::prune_sessions(config.session_retention)?;
    if pruned > 0 && !quiet {
        eprintln!(
            "{} pruned {pruned} old session(s)",
            colored::Colorize::dimmed("postflight:"),
        );
    }

    let _ = std::io::Write::flush(&mut std::io::stdout());
    let _ = std::io::Write::flush(&mut std::io::stderr());
    std::process::exit(pty_result.exit_code);
}

fn resolve_session_id(id: &str) -> Result<PathBuf> {
    if id.is_empty() {
        anyhow::bail!("session ID cannot be empty");
    }
    if id.contains('/') || id.contains('\\') || id.starts_with('.') {
        anyhow::bail!("invalid session ID: '{id}'");
    }

    let exact = Config::sessions_dir().join(id);
    if exact.exists() && exact.join("events.jsonl").exists() {
        return Ok(exact);
    }

    let sessions = Session::list_sessions()?;
    let matches: Vec<_> = sessions
        .iter()
        .filter(|(name, _)| name.starts_with(id))
        .collect();

    match matches.len() {
        0 => anyhow::bail!("no session matching '{id}'"),
        1 => Ok(matches[0].1.clone()),
        n => anyhow::bail!("'{id}' is ambiguous ({n} matches), be more specific"),
    }
}

fn cmd_report(session_id: Option<String>, json: bool, show_diff: bool) -> Result<()> {
    let session_dir = if let Some(id) = session_id {
        resolve_session_id(&id)?
    } else {
        Session::latest_session()?.context("no sessions found")?
    };

    if !session_dir.exists() {
        anyhow::bail!("session not found: {}", session_dir.display());
    }

    let summary = Session::load_summary(&session_dir)
        .with_context(|| format!("failed to load session summary (file may be corrupted): {}", session_dir.display()))?;

    if json {
        println!("{}", report::render_json(&summary));
    } else {
        print!("{}", report::render_terminal(&summary, show_diff));

        if show_diff {
            let diffs_dir = session_dir.join("diffs");
            if diffs_dir.exists() {
                let mut diff_entries: Vec<_> = std::fs::read_dir(&diffs_dir)?
                    .filter_map(|e| e.ok())
                    .collect();
                diff_entries.sort_by_key(|e| e.file_name());
                if !diff_entries.is_empty() {
                    println!("{}", colored::Colorize::bold("\u{2501}\u{2501}\u{2501} diffs \u{2501}\u{2501}\u{2501}"));
                    for entry in diff_entries {
                        if let Ok(content) = std::fs::read_to_string(entry.path()) {
                            print!("{}", postflight::diff::format_diff_colored(&content));
                        }
                    }
                }
            }
        }
    }

    Ok(())
}

fn cmd_sessions(filter: Option<String>, failed: bool) -> Result<()> {
    let sessions = Session::list_sessions()?;

    if sessions.is_empty() {
        println!("no sessions recorded yet");
        return Ok(());
    }

    println!(
        "{:<20} {:<6} {:<8} {:<7} {:<30}",
        "SESSION", "EXIT", "DURATION", "FILES", "COMMAND"
    );
    println!("{}", "\u{2500}".repeat(75));

    for (id, path) in &sessions {
        if let Ok(s) = Session::load_summary(path) {
            if failed && s.exit_code == 0 {
                continue;
            }
            if let Some(ref pattern) = filter {
                let pattern_lower = pattern.to_lowercase();
                if !s.command.to_lowercase().contains(&pattern_lower)
                    && !id.contains(&pattern_lower)
                {
                    continue;
                }
            }

            let cmd_display = if s.command.len() > 28 {
                let truncated = truncate_str(&s.command, 25);
                format!("{truncated}...")
            } else {
                s.command.clone()
            };
            let duration = format_duration_short(s.duration);
            let file_count = s.files_created.len()
                + s.files_modified.len()
                + s.files_deleted.len();
            println!(
                "{:<20} {:<6} {:<8} {:<7} {:<30}",
                id, s.exit_code, duration, file_count, cmd_display
            );
        } else {
            if failed {
                continue;
            }
            println!(
                "{:<20} {:<6} {:<8} {:<7} {:<30}",
                id, "?", "-", "-", "(no summary)"
            );
        }
    }

    Ok(())
}

fn format_duration_short(d: Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{}s", secs / 60, secs % 60)
    } else {
        format!("{}h{}m", secs / 3600, (secs % 3600) / 60)
    }
}

fn truncate_str(s: &str, max_chars: usize) -> &str {
    match s.char_indices().nth(max_chars) {
        Some((byte_idx, _)) => &s[..byte_idx],
        None => s,
    }
}

fn cmd_clean(keep: Option<usize>) -> Result<()> {
    let config = Config::load()?;
    let retention = keep.unwrap_or(config.session_retention);
    let pruned = Session::prune_sessions(retention)?;
    println!("removed {pruned} session(s), keeping {retention}");
    Ok(())
}

fn cmd_init() -> Result<()> {
    let config_path = Config::config_path();

    if config_path.exists() {
        println!("config already exists at {}", config_path.display());
        return Ok(());
    }

    std::fs::create_dir_all(Config::config_dir())?;
    std::fs::write(
        &config_path,
        r#"# postflight configuration

# Number of sessions to keep before auto-pruning
session_retention = 20

# Workspace root override (defaults to current directory)
# workspace_root = "/path/to/project"

# Glob patterns for files/directories to ignore during observation
exclude_patterns = [
    ".git/**",
    "target/**",
    "node_modules/**",
    ".postflight/**",
    "*.pyc",
    "__pycache__/**",
]

# How often to poll for network connections (milliseconds)
network_poll_interval_ms = 500

# How often to poll for subprocess changes (milliseconds)
process_poll_interval_ms = 250

# Maximum session duration before triggering graceful shutdown (seconds)
# Uncomment to enforce a timeout on agent sessions:
# max_duration_secs = 3600

# Grace period after sending wrap-up message before SIGTERM (seconds)
grace_period_secs = 60

# Message written to the agent's stdin when graceful shutdown begins
shutdown_message = "You have 60 seconds to finish. Wrap up and produce your final output now.\n"
"#,
    )?;

    println!("created config at {}", config_path.display());
    Ok(())
}

fn cmd_replay(session_id: Option<String>) -> Result<()> {
    let session_dir = if let Some(id) = session_id {
        resolve_session_id(&id)?
    } else {
        Session::latest_session()?.context("no sessions found")?
    };

    let terminal_path = session_dir.join("terminal.raw");
    if !terminal_path.exists() {
        anyhow::bail!("no terminal recording found in {}", session_dir.display());
    }

    let data = std::fs::read(&terminal_path)
        .with_context(|| format!("failed to read {}", terminal_path.display()))?;

    let _ = std::io::Write::write_all(&mut std::io::stdout(), &data);
    let _ = std::io::Write::flush(&mut std::io::stdout());

    Ok(())
}

fn cmd_stop(session_id: Option<String>) -> Result<()> {
    let session_dir = if let Some(id) = session_id {
        resolve_session_id(&id)?
    } else {
        find_active_session()?.context("no active session found")?
    };

    let sentinel = GracefulShutdown::sentinel_path(&session_dir);
    if sentinel.exists() {
        println!("stop already requested for {}", session_dir.file_name().unwrap_or_default().to_string_lossy());
        return Ok(());
    }

    std::fs::write(&sentinel, "")
        .with_context(|| format!("failed to create stop sentinel in {}", session_dir.display()))?;

    println!(
        "graceful stop requested for session {}",
        session_dir.file_name().unwrap_or_default().to_string_lossy()
    );
    println!("the agent will receive a wrap-up message and has a grace period to finish");

    Ok(())
}

fn find_active_session() -> Result<Option<PathBuf>> {
    let sessions = Session::list_sessions()?;
    Ok(sessions
        .into_iter()
        .find(|(_, path)| {
            !path.join("summary.json").exists() && path.join("events.jsonl").exists()
        })
        .map(|(_, path)| path))
}
