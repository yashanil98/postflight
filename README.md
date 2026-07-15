# postflight

Records everything an AI coding agent does to your system, then renders a structured report.

## Install

```
cargo install --path .
```

## Usage

```bash
# Record an agent session
postflight run "claude code fix the auth bug"
postflight run "aider --model claude-3.5-sonnet fix server.py"

# Quiet mode (record without printing the report)
postflight run "make build" --quiet

# Machine-readable output from a live run (child output goes to stderr)
postflight run "npm test" --json | jq .exit_code

# Show the last session's report
postflight report
postflight report --json
postflight report --diff    # include file diffs

# List all recorded sessions (shows duration + file counts)
postflight sessions

# Clean up old sessions
postflight clean --keep 10

# Generate a documented config file
postflight init
```

## Example Output

```
━━━ postflight session report ━━━
  command: aider --model claude-3.5-sonnet fix server.py
  workspace: /home/dev/myproject
  duration: 34s
  exit code: 0

files changed
  created (1)
    + src/auth/middleware.rs (1.2 KiB)
  modified (2)
    ~ src/server.rs (4.8 KiB)
    ~ src/auth/mod.rs (890 B)

files read
  src/ (12 files)
    server.rs, main.rs, config.rs, lib.rs, ...
  tests/ (4 files)
    test_server.rs, test_auth.rs, ...

network connections
  → api.anthropic.com:443 (tcp)
  → registry.npmjs.org:443 (tcp)

subprocesses
  ▸ cargo check [exit:0] (3s)
  ▸ cargo test [exit:0] (8s)
  ▸ git diff --stat [exit:0] (0s)

━━━ verdict ━━━
  touched 3 files in src/auth/, read 16 files, 2 network connections, 14 subprocesses, ran for 34s
  total disk writes: 2.1 KB
```

## What It Records

- **Files**: every create, modify, and delete in the workspace (with diffs)
- **Reads**: files accessed during the session
- **Network**: outbound connections (host, port, protocol)
- **Processes**: every subprocess spawned (full argv, exit code, duration)
- **Terminal output**: raw PTY capture, replayable with `cat`
- **Structured log**: JSONL event stream for programmatic consumption

## Architecture

```
┌──────────────────────────────────────────────────┐
│              postflight run "..."                  │
├──────────────┬───────────────┬───────────────────┤
│  PTY Wrapper │  FS Observer  │  Network Observer  │
│  (openpty)   │  (poll+snap)  │  (lsof poll)       │
├──────────────┼───────────────┼───────────────────┤
│         Process Tracker (proc_listchildpids)       │
├──────────────────────────────────────────────────┤
│       Session Storage (~/.postflight/sessions/)    │
│       events.jsonl │ terminal.raw │ summary.json   │
├──────────────────────────────────────────────────┤
│         Report Renderer (terminal / JSON)          │
└──────────────────────────────────────────────────┘
```

The child command runs in a PTY. Filesystem changes are detected by pre/post snapshot diffing
(authoritative) supplemented by real-time polling (for event ordering). Network connections
are observed via lsof polling. Process trees are tracked via libproc (macOS) or /proc (Linux).

No root required. No kernel extensions. No SIP bypass. Works unprivileged out of the box.

## How It Compares

| Tool | Purpose | Root | Platform | Approach |
|------|---------|------|----------|----------|
| **postflight** | Observe & report | No | macOS, Linux | PTY + poll |
| strace/dtrace | Syscall tracing | Yes* | Linux/macOS | Kernel attach |
| sandbox-runtime | Enforce policy | No | Linux | Seccomp + namespaces |
| nono | Block file access | No | macOS, Linux | LD_PRELOAD |
| AgentSight | Research tracing | Yes | Linux | eBPF |

postflight is observation, not enforcement. It answers "what happened?" not "should this be
allowed?" It composes with sandboxes rather than competing with them.

## Configuration

Optional. Run `postflight init` to generate a documented config, or create `~/.postflight/config.toml` manually:

```toml
session_retention = 20
exclude_patterns = [".git/**", "target/**", "node_modules/**", "*.pyc"]
network_poll_interval_ms = 500
process_poll_interval_ms = 250
```

## Event Log Format

Each session stores `events.jsonl` with one JSON object per line:

```json
{"type":"session_start","command":"...","workspace":"/...","timestamp":"...","pid":1234}
{"type":"file_created","path":"/project/new.rs","timestamp":"...","size_bytes":456}
{"type":"network_connection","pid":1234,"remote_host":"api.example.com","remote_port":443,"protocol":"tcp","timestamp":"..."}
{"type":"process_spawned","pid":5678,"ppid":1234,"argv":["cargo","test"],"timestamp":"..."}
{"type":"session_end","exit_code":0,"duration":{"secs":34,"nanos":0},"timestamp":"..."}
```

## Design

See [DESIGN.md](DESIGN.md) for architecture decisions, tradeoffs, and the v2 roadmap.

## License

MIT
