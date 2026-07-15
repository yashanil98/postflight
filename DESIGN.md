# postflight Design Document

This document explains the architecture, tradeoffs, and reasoning behind postflight's
implementation. It's written for engineers evaluating the system design, not as user documentation.

## Problem Statement

AI coding agents (Claude Code, Aider, Cursor, etc.) execute arbitrary shell commands on your
machine. After they finish, you have no structured record of what they did — which files they
read, what they modified, what network calls they made, what subprocesses they spawned. You're
left grepping shell history or diffing filesystem snapshots manually.

postflight fills this gap: a lightweight, unprivileged observation layer that records everything
the agent touched and renders a structured post-session report.

## Architecture Overview

```
┌─────────────────────────────────────────────────────┐
│                   postflight CLI                     │
├─────────────────────────────────────────────────────┤
│  PTY Wrapper    │  FS Observer  │  Net Observer     │
│  (libc openpty) │  (polling)    │  (lsof polling)   │
│                 │               │                   │
│  Process        │  Snapshot     │                   │
│  Tracker        │  Diffing      │                   │
│  (proc_listchild│  (pre/post)   │                   │
│   pids)         │               │                   │
├─────────────────┴───────────────┴───────────────────┤
│           Session Storage (JSONL + diffs)            │
├─────────────────────────────────────────────────────┤
│           Report Renderer (terminal + JSON)          │
└─────────────────────────────────────────────────────┘
```

## PTY Wrapping

The child command runs inside a pseudoterminal. This is necessary (not optional) because:

1. Many AI agents detect whether they're in a TTY and change behavior accordingly.
2. We need to capture stdout/stderr interleaved exactly as the user would see it.
3. The child process and its descendants form a session, making process tree tracking easier.

Implementation: raw libc `openpty()` + `fork()` + `execvp()`. We considered the `portable-pty`
crate but it adds a large dependency tree for cross-platform PTY handling we don't need (macOS
and Linux only). The nix crate removed its PTY module in recent versions, so raw libc is the
most stable approach.

The parent reads from the primary fd in a poll loop (100ms timeout), forwarding all output to
the real stdout in real time. Terminal output is also saved to `terminal.raw` for replay.

## Filesystem Observation

### Why Polling (Not FSEvents/kqueue Directly)

macOS FSEvents API provides real-time file system notifications, but its Rust ecosystem support
is limited. The `fsevent-stream` crate works but requires tokio, and its event granularity
(created vs. modified vs. metadata-changed) is unreliable for short-lived files.

Our approach: **periodic filesystem polling + pre/post snapshot diffing**.

- **Pre-run snapshot**: Walk the workspace, record `(path, mtime, size)` for every file.
- **Real-time polling** (200ms interval): Detect creates/modifies/deletes as they happen.
- **Post-run snapshot**: Walk again after the child exits, diff against pre-run state.

The post-run snapshot diff is authoritative — it catches everything the real-time poller might
miss due to timing. The real-time poller provides ordering information (which file was touched
first) for the event log.

### Linux: inotify

On Linux, we use `inotify` for real-time filesystem events instead of polling. This provides
lower latency, lower CPU usage, and the ability to track file ACCESS events (reads) that
polling cannot detect. The inotify watcher monitors CREATE, MODIFY, DELETE, MOVED_FROM,
MOVED_TO, and ACCESS events.

### Why Not kqueue

kqueue requires explicitly watching each file descriptor. For a large workspace (10k+ files),
this means 10k+ kevent registrations. It also doesn't handle recursive directory watching — you
need to manually watch new directories as they're created. FSEvents handles this natively, but
its Rust bindings add async runtime complexity we don't want in v1.

### Why Not dtrace/Instruments

dtrace on modern macOS requires System Integrity Protection disabled or a signed entitled
binary. This violates our "no root/no special privileges" constraint.

## Network Observation

### Why lsof Polling

Options considered:

| Approach | Privileges | Overhead | Coverage |
|----------|-----------|----------|----------|
| **lsof polling** | None | Low (~2ms per call) | Established connections |
| libproc/proc_pidinfo | None | Very low | Established connections |
| Network Extension | Entitlement | High | All traffic |
| BPF/pcap | Root | Medium | All packets |
| /proc/net/tcp (Linux) | None | Very low | TCP only |

lsof is the pragmatic v1 choice: no privileges needed, works on both macOS and Linux, captures
the host/port/protocol tuple we care about. The 500ms polling interval means very short-lived
connections (< 500ms) might be missed — acceptable for an observation tool.

### Deduplication

The network observer maintains a set of `(pid, remote_host, remote_port, protocol)` tuples.
Each unique connection is reported once, regardless of how many poll cycles it persists through.

### Filtering

Loopback connections (127.0.0.1, ::1) are filtered out. They're almost always internal IPC
(e.g., language servers, build daemons) and would overwhelm the report with noise.

## Process Tree Tracking

On macOS: `proc_listchildpids()` from libproc. This returns the immediate children of a PID.
We recursively walk the tree to find all descendants of the shell process we spawned.

On Linux: `/proc/<pid>/task/<pid>/children` provides the same information.

The process tracker polls at 250ms intervals. When a new PID appears, we read its argv from
`KERN_PROCARGS2` (macOS) or `/proc/<pid>/cmdline` (Linux). When a PID disappears, we record
its exit (duration calculated from first-seen time; exit code is best-effort since we only
`waitpid()` the direct child).

### Limitation: Exit Codes of Grandchildren

We can only reliably get the exit code of our direct child (the shell). Grandchildren exit
codes are recorded as 0 unless we can read them before the process is reaped. This is a known
v1 limitation; v2 could use process_connector (Linux) or Endpoint Security (macOS) for exact
exit codes.

## Session Storage

Sessions are stored in `~/.postflight/sessions/<YYYYMMDD_HHMMSS>/`:

```
20260714_143022/
├── events.jsonl    # Structured event log (one JSON object per line)
├── terminal.raw    # Raw PTY output (replayable with cat)
├── diffs/          # Unified diffs of modified files
│   └── src_main.rs.diff
└── summary.json    # Final structured report
```

JSONL format was chosen over a single JSON array because:
1. Events can be appended without reading/parsing the entire file.
2. Partial sessions (killed mid-run) still have valid, parseable data.
3. Streaming consumers can process events as they arrive.

## Report Rendering

The terminal report is designed to be scannable in 3 seconds:
- Header: command, workspace, duration, exit code
- Files: grouped by operation (created/modified/deleted), then reads grouped by directory
- Network: unique connections with host:port
- Subprocesses: argv truncated to 80 chars, with exit code and duration
- Verdict: one-sentence summary of total impact

Color coding: green for creates, yellow for modifies, red for deletes, cyan for network.
Dimmed text for metadata. Bold for section headers. Follows the cargo/ripgrep school of
terminal output design.

## Tradeoffs Accepted in v1

1. **Polling vs. event-driven filesystem watching**: Higher latency for file detection (200ms)
   but simpler, more reliable, no async runtime needed. (Linux uses inotify for real-time events.)

2. **lsof subprocess vs. libproc FFI**: ~2ms overhead per poll but portable and well-understood.
   v2 should use proc_pidinfo directly.

3. **No pre-run file content caching**: We can't generate true unified diffs of modified files
   because we don't cache file contents before the run. The snapshot records size/mtime changes.
   v2 should hash or cache content for files under a size threshold.

4. **Best-effort subprocess exit codes**: Grandchild exit codes are unreliable without kernel
   event sources. Acceptable for an observation tool.

5. **Single-machine, single-run scope**: No daemon mode, no remote collection, no aggregation.
   This is intentionally a single-session tool in v1.

## v2 Roadmap

- **eBPF backend** (Linux): Replace polling with BPF programs for process lifecycle and network
  events. Zero overhead, perfect coverage, kernel-level timestamps.
- **Endpoint Security** (macOS): Apple's modern replacement for kauth — provides file access,
  process exec, and network events through a unified API. Requires entitlement but no root.
- **Content diffing**: Cache file contents pre-run for true unified diffs.
- **libproc integration**: Replace lsof with direct proc_pidinfo calls.
- **Daemon mode**: Long-running observer that auto-attaches to agent processes.
- **Web UI**: Session replay with timeline scrubbing.
- **Composability**: Integration with sandboxes (sandbox-runtime, nono) for
  "observe then enforce" workflows.
