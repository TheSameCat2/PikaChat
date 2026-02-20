<p align="center">
  <img src="resources/pikachat-logo.png" alt="PikaChat logo" width="220" />
</p>

<h1 align="center">PikaChat</h1>
<p align="center"><strong>An opinionated Matrix client for gamers.</strong></p>

PikaChat is a Rust-first Matrix client project built for people who treat chat like part of gameplay, not just background noise. The goal is fast, clear communication for squads, guilds, and friend groups, with practical defaults that favor coordination over clutter.

## What This App Is Trying To Become

PikaChat is aiming to be the Matrix client you open before a match, not after one.

### Design Direction

- **Opinionated by default**: fewer decisions to make before you can play.
- **Gamer-first UX**: fast room switching, readable timelines, and low-friction message actions.
- **Desktop-first foundation**: robust Linux desktop behavior now, portable backend for future mobile/other clients.
- **Rust all the way down**: predictable performance, safer concurrency, and maintainable architecture.

### Product Intent

- **Reliable sync under real use**: reconnects, retries, and timeline integrity matter.
- **Messaging tools that match game flow**: fast send/edit/redact and DM support.
- **Media support in the core**: upload/download already available through backend APIs.
- **Future E2EE and trust ergonomics**: security that does not feel like a side quest.

## Current Project State

We have begun a robust backend that implements test-critical Matrix features.

## Architecture (Current)

- `crates/backend-core`:
  - frontend-facing protocol (`BackendCommand`, `BackendEvent`, timeline types, errors)
  - state machine and retry/timeline helpers
- `crates/backend-matrix`:
  - Matrix SDK-backed runtime and adapter (`spawn_runtime`, `MatrixFrontendAdapter`)
- `crates/backend-platform`:
  - platform integrations (for example keyring abstraction)
- `apps/backend-smoke`:
  - smoke/integration command-line harness for live testing
- `apps/desktop-shell`:
  - initial Slint desktop UI shell

## Clone Instructions

```bash
git clone git@github.com:TheSameCat2/PikaChat.git
cd PikaChat
```

## Build Instructions

### Prerequisites

- Rust toolchain (stable) with `cargo` and `rustc`
- Linux desktop session (Wayland or X11) for the Slint desktop shell

### Build Everything

```bash
cargo check --workspace
```

### Run Tests

```bash
cargo test --workspace
```

### Run the Desktop Shell

```bash
cargo run -p desktop-shell
```

Expected behavior:

- Window opens with a minimal shell UI.
- `File -> Quit` exits cleanly.
- `Help -> About Slint...` opens the About view containing Slint's built-in `AboutSlint` widget.

### Optional Backend Smoke Run

If you want to exercise backend flows directly:

```bash
cargo run -p backend-smoke
```

You can pass env vars for live auth/media smoke (`PIKACHAT_HOMESERVER`, `PIKACHAT_USER`, `PIKACHAT_PASSWORD`, etc.).

## Why “Opinionated Matrix Client for Gamers”

Most Matrix clients are general-purpose. PikaChat is intentionally not trying to be everything for everyone. It is trying to be excellent for game-centric groups that need:

- quick context switching,
- sane defaults,
- durable messaging behavior,
- and a clear path from shell to full-featured client.

## Next Phase

The next major step is turning the desktop shell into a real client surface:

- connect Slint models to the backend event stream,
- render room lists and timelines,
- send/edit/redact from UI,
- expose media workflows in the interface.

## Security Notes

- `PLAN.md` is local-only planning scratch and is intentionally not committed.
- Credentials should be passed via environment variables for live testing.
- Do not commit secrets.
