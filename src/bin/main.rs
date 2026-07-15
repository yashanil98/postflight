use postflight::config::Config;
use postflight::diff::{capture_snapshot, diff_snapshots};
use postflight::events::{Event, SessionEndEvent};
use postflight::fs_watcher::FsWatcher;
use postflight::network::NetworkObserver;
use postflight::process::ProcessTracker;
use postflight::pty::PtyChild;
use postflight::report;
use postflight::session::{ConnectionSummary, Session, SessionSummary, SubprocessSummary};
use anyhow::{Context, Result};
use chrono::Utc;
use clap::{Parser, Subcommand};
use std::collections::HashSet;
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
    Sessions,

    /// Remove old sessions
    Clean {
        /// Number of sessions to keep (default: from config or 20)
        #[arg(short, long)]
        keep: Option<usize>,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Run { command, workspace, quiet, json } => cmd_run(&command, workspace, quiet, json),
        Commands::Report { session, json, diff } => cmd_report(session, json, diff),
        Commands::Sessions => cmd_sessions(),
        Commands::Clean { keep } => cmd_clean(keep),
    }
}

fn cmd_run(command: &str, workspace_override: Option<PathBuf>, quiet: bool, json: bool) -> Result<()> {
    let config = Config::load()?;
    let workspace = workspace_override
        .or_else(|| config.workspace_root.clone())
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

    let workspace = workspace.canonicalize().unwrap_or(workspace);

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

    let mut session = Session::create(command)?;

    let exclude_fn = |p: &std::path::Path| config.should_exclude(p);
    let pre_snapshot = capture_snapshot(&workspace, &exclude_fn);

    let mut fs_watcher = FsWatcher::new(workspace.clone(), config.clone());
    fs_watcher.start()?;

    let child = PtyChild::spawn(command).context("failed to spawn command")?;
    let child_pid = child.pid.as_raw() as u32;

    // Forward SIGINT/SIGTERM to the child process so Ctrl+C kills the child,
    // not postflight. The session still gets saved after the child exits.
    let child_pid_for_signal = child.pid;
    let interrupted = Arc::new(AtomicBool::new(false));
    let interrupted_handler = Arc::clone(&interrupted);
    unsafe {
        signal_hook::low_level::register(signal_hook::consts::SIGINT, move || {
            interrupted_handler.store(true, Ordering::Relaxed);
            let _ = nix::sys::signal::kill(child_pid_for_signal, nix::sys::signal::Signal::SIGINT);
        })
    }
    .ok();
    let child_pid_for_term = child.pid;
    unsafe {
        signal_hook::low_level::register(signal_hook::consts::SIGTERM, move || {
            let _ = nix::sys::signal::kill(child_pid_for_term, nix::sys::signal::Signal::SIGTERM);
        })
    }
    .ok();

    let mut process_tracker = ProcessTracker::new(child_pid, config.process_poll_interval_ms);
    let mut network_observer = NetworkObserver::new(child_pid, config.network_poll_interval_ms);

    let running = Arc::new(AtomicBool::new(true));

    let running_proc = Arc::clone(&running);
    let (proc_tx, proc_rx) = std::sync::mpsc::channel();
    let proc_poll_interval = process_tracker.poll_interval();

    thread::spawn(move || {
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

    thread::spawn(move || {
        while running_net.load(Ordering::Relaxed) {
            let events = network_observer.poll();
            for event in events {
                let _ = net_tx.send(Event::NetworkConnection(event));
            }
            thread::sleep(net_poll_interval);
        }
    });

    let pty_result = child.wait_with_output(|data| {
        let _ = session.write_terminal_chunk(data);
        if json {
            let _ = std::io::Write::write_all(&mut std::io::stderr(), data);
        } else {
            let _ = std::io::Write::write_all(&mut std::io::stdout(), data);
            let _ = std::io::Write::flush(&mut std::io::stdout());
        }
    })?;

    running.store(false, Ordering::Relaxed);
    thread::sleep(Duration::from_millis(100));

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

    // Generate diffs for modified files
    for path in &files_modified {
        if let (Some(pre_entry), true) = (pre_snapshot.get(path), path.exists()) {
            if let Ok(new_content) = std::fs::read_to_string(path) {
                let diff_content =
                    format!("--- a/{}\n+++ b/{}\n(file modified, {} bytes -> {} bytes)\n",
                        path.display(),
                        path.display(),
                        pre_entry.size,
                        new_content.len()
                    );
                let filename = path.to_string_lossy();
                session.save_diff(&filename, &diff_content)?;
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
        start_time: Utc::now() - chrono::Duration::from_std(pty_result.duration).unwrap_or_default(),
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

    eprintln!(
        "{} session saved to {}",
        colored::Colorize::dimmed("postflight:"),
        session.dir.display()
    );

    let pruned = Session::prune_sessions(config.session_retention)?;
    if pruned > 0 {
        eprintln!(
            "{} pruned {pruned} old session(s)",
            colored::Colorize::dimmed("postflight:"),
        );
    }

    std::process::exit(pty_result.exit_code);
}

fn cmd_report(session_id: Option<String>, json: bool, show_diff: bool) -> Result<()> {
    let session_dir = if let Some(id) = session_id {
        Config::sessions_dir().join(&id)
    } else {
        Session::latest_session()?.context("no sessions found")?
    };

    if !session_dir.exists() {
        anyhow::bail!("session not found: {}", session_dir.display());
    }

    let summary = Session::load_summary(&session_dir)?;

    if json {
        println!("{}", report::render_json(&summary));
    } else {
        print!("{}", report::render_terminal(&summary, show_diff));

        if show_diff {
            let diffs_dir = session_dir.join("diffs");
            if diffs_dir.exists() {
                println!("{}", colored::Colorize::bold("\u{2501}\u{2501}\u{2501} diffs \u{2501}\u{2501}\u{2501}"));
                for entry in std::fs::read_dir(&diffs_dir)? {
                    let entry = entry?;
                    if let Ok(content) = std::fs::read_to_string(entry.path()) {
                        println!("{content}");
                    }
                }
            }
        }
    }

    Ok(())
}

fn cmd_sessions() -> Result<()> {
    let sessions = Session::list_sessions()?;

    if sessions.is_empty() {
        println!("no sessions recorded yet");
        return Ok(());
    }

    println!("{:<20} {:<10} {:<40}", "SESSION", "EXIT", "COMMAND");
    println!("{}", "\u{2500}".repeat(70));

    for (id, path) in &sessions {
        let summary = Session::load_summary(path);
        match summary {
            Ok(s) => {
                let cmd_display = if s.command.len() > 38 {
                    format!("{}...", &s.command[..35])
                } else {
                    s.command.clone()
                };
                println!("{:<20} {:<10} {:<40}", id, s.exit_code, cmd_display);
            }
            Err(_) => {
                println!("{:<20} {:<10} {:<40}", id, "?", "(no summary)");
            }
        }
    }

    Ok(())
}

fn cmd_clean(keep: Option<usize>) -> Result<()> {
    let config = Config::load()?;
    let retention = keep.unwrap_or(config.session_retention);
    let pruned = Session::prune_sessions(retention)?;
    println!("removed {pruned} session(s), keeping {retention}");
    Ok(())
}
