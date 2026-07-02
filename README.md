# Codex Rail

Codex Rail (`rail`) is a lightweight session manager for multiple Codex CLI processes. It is designed to feel closer to Claude Code's agent view: one manager screen, background sessions, fast attach and detach, and no split panes.

## Scope

- Manages Codex sessions launched by `rail`.
- Does not modify Codex, inject plugins, parse Codex internals, or use tmux.
- Each session runs in its own background worker process with its own PTY and Unix socket.
- Closing the manager or terminal leaves active sessions running. Shutdown or container stop will still stop processes.

## Controls

Manager screen:

- `w` / `Up`: previous session
- `s` / `Down`: next session
- `d` / `Right` / `Enter`: attach selected session
- mouse hover: highlight row
- mouse click: attach row
- `e`: focus the new-session input
- normal typing: starts a new-session input, except reserved manager keys like `w`, `s`, `d`, `e`
- `Ctrl-R`: rename selected session
- `Ctrl-X`, then `Ctrl-X` again within 2 seconds: stop selected session
- `Esc`, then `Esc` again within 2 seconds: leave manager without stopping sessions

Attached Codex session:

- `Ctrl-Z`: detach back to the manager
- Other keys pass through to Codex.

Input mode:

- `Enter` or `Ctrl-D`: submit
- `Esc`: cancel

## Install

Local build:

```sh
cargo build --release
mkdir -p ~/.local/bin
cp target/release/rail ~/.local/bin/rail
```

One-line install for a published repo:

```sh
curl -fsSL https://raw.githubusercontent.com/<owner>/codex-rail/main/install.sh | bash
```

Until the repo is published, run the local build command above from this directory.

## Data Layout

- State: `$XDG_DATA_HOME/codex-rail/jobs` or `~/.local/share/codex-rail/jobs`
- Sockets: `$XDG_RUNTIME_DIR/codex-rail` or `/tmp/codex-rail-$UID`
- Per-session log tail: `output.log` inside the job directory

Set `CODEX_RAIL_CODEX=/path/to/codex` if `codex` is not on `PATH`.

## Current MVP Limits

- Unix-like systems only.
- Only sessions launched by `rail` are manageable.
- Reattach sends recent output log tail and resizes the PTY. It does not inspect Codex's internal screen state.
- One active attachment per session. A second attach is refused until the current attach detaches.
