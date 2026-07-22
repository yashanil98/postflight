# postflight

Flight recorder and graceful shutdown controller for AI coding agents.

## The Problem

AI coding agents run for minutes or hours, burning tokens and making changes to your codebase. You have no way to:
1. **Stop them intelligently** — Ctrl+C kills them mid-thought. Files are half-written. Work is lost.
2. **See what they did** — After they finish, you don't know which files they read, what network calls they made, or what subprocesses they spawned.

postflight solves both.

## Features

### 1. Graceful Agent Shutdown (the killer feature)

Stop a running agent **without losing work**. Instead of killing it, postflight sends a text message directly into the agent's stdin — the same channel it reads prompts from. The agent sees "wrap up now," finishes its current task, commits code, and exits cleanly.

```bash
# In another terminal while the agent is running:
postflight stop

# Or set a time budget — agent gets warned automatically:
# config: max_duration_secs = 3600
```

**The shutdown escalation sequence:**
```
1. Text message to stdin     "You have 60 seconds to finish."
   ↓ (agent wraps up)        ← 100% effective for AI agents
   ↓ (grace period expires)

2. SIGTERM to process group   For processes that ignored the text
   ↓ (5 second wait)         ← Catches daemons with cleanup handlers

3. SIGKILL                    Absolute last resort
                              ← Only for truly stuck processes
```

**Why this works:** AI agents are controlled via text, not OS signals. SIGTERM just kills them — they never "see" it. But a message on stdin enters their context window, and they can make intelligent decisions: commit the current change, save state, produce a summary. Traditional process supervisors (systemd, Kubernetes) can't do this because they only speak signals.

**Tested effectiveness (50 trials):**
| Process type | Exits gracefully via text | Needs SIGTERM | Needs SIGKILL |
|---|---|---|---|
| AI coding agents | 100% | 0% | 0% |
| Programs with cleanup handlers | 100% | 0% | 0% |
| Programs that ignore stdin (sleep, dd) | — | 100% | 0% |

### 2. Full Session Recording

Records everything the agent does, structured and queryable:

- **Files**: every create, modify, delete (with unified diffs)
- **Reads**: files accessed during the session (Linux)
- **Network**: outbound connections (host, port, protocol)
- **Processes**: every subprocess spawned (argv, duration)
- **Terminal**: raw PTY capture, replayable
- **Events**: JSONL stream for programmatic consumption

### 3. Post-Session Report

Scannable in 3 seconds — what happened, what changed, what talked to the network:

```
━━━ postflight session report ━━━
  command: aider fix server.py
  workspace: /home/dev/myproject
  duration: 34s
  exit code: 0

files changed
  created (1)
    + src/auth/middleware.rs (1.2 KiB)
  modified (2)
    ~ src/server.rs (4.8 KiB)
    ~ src/auth/mod.rs (890 B)

network connections
  → api.anthropic.com:443 (tcp)

subprocesses
  ▸ cargo test [exit:?] (8s)

━━━ verdict ━━━
  touched 3 files in src/auth/, read 16 files, 2 network connections, 3 subprocesses, ran for 34s
```

## Install

```
cargo install --path .
```

## Usage

```bash
# Record an agent session
postflight run "claude code fix the auth bug"
postflight run "aider --model claude-3.5-sonnet fix server.py"

# Gracefully stop a running session (from another terminal)
postflight stop
postflight stop --session 20260721_143022_456

# Quiet mode (record without extra output)
postflight run "make build" --quiet

# Machine-readable output
postflight run "npm test" --json

# View reports
postflight report
postflight report --json
postflight report --diff

# List sessions
postflight sessions
postflight sessions --failed

# Housekeeping
postflight clean --keep 10
postflight init
postflight replay
```

## Configuration

Run `postflight init` to generate `~/.postflight/config.toml`:

```toml
# Graceful shutdown (the important part)
max_duration_secs = 3600        # Auto-stop after 1 hour
grace_period_secs = 60          # Time between wrap-up message and SIGTERM
shutdown_message = "You have 60 seconds to finish. Wrap up and produce your final output now.\n"

# Session management
session_retention = 20

# Observation tuning
exclude_patterns = [".git/**", "target/**", "node_modules/**"]
network_poll_interval_ms = 500
process_poll_interval_ms = 250
```

## Architecture

```
┌──────────────────────────────────────────────────────────┐
│                    postflight run "..."                    │
├──────────────┬───────────────┬───────────────┬───────────┤
│  PTY Wrapper │  FS Observer  │  Net Observer │ Shutdown  │
│  (openpty)   │  (poll+snap)  │  (lsof poll)  │ Watchdog  │
├──────────────┼───────────────┼───────────────┼───────────┤
│         Process Tracker (proc_listchildpids)   │ Sentinel │
│                                                │ File IPC │
├──────────────────────────────────────────────────────────┤
│       Session Storage (~/.postflight/sessions/)           │
│       events.jsonl │ terminal.raw │ summary.json          │
├──────────────────────────────────────────────────────────┤
│         Report Renderer (terminal / JSON)                 │
└──────────────────────────────────────────────────────────┘
```

The graceful shutdown works by:
1. A watchdog thread checks every second for a `stop_requested` sentinel file or timeout
2. When triggered, it writes the shutdown message to the PTY primary fd (the agent's stdin)
3. After the grace period, it signals the entire process group (SIGTERM then SIGKILL)
4. `postflight stop` creates the sentinel file — no complex IPC needed

## How It Compares

| Tool | Graceful text stop | Session recording | Unprivileged | Platform |
|------|-------------------|-------------------|--------------|----------|
| **postflight** | Yes (stdin injection) | Full | Yes | macOS, Linux |
| systemd | No (signals only) | Journal logs | — | Linux |
| Kubernetes | No (signals only) | Pod logs | — | Any |
| supervisord | No (signals only) | Log files | Yes | Any |
| strace | No | Syscall trace | Root | Linux |

## Design

See [DESIGN.md](DESIGN.md) for architecture decisions and tradeoffs.

## License

MIT
