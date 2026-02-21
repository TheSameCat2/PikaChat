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

PikaChat is now in an early but functional desktop MVP phase:

- Rust Matrix backend runtime is live and integrated into the desktop app.
- Desktop app now has a login pane, remember-session restore, logout with destructive local wipe, room list/timeline rendering, and plain-text message send.
- Timeline updates live from sync events.
- Sidebar is resizable and room selection is wired to backend room open/pagination commands.
- Security menu supports:
  - `Check Backup Status`
  - `Back Up Identity...`
  - `Reset Identity Backup...` (creates a fresh recovery key)
  - `Restore Identity...` (recovery key/passphrase restore flow)
- Recovery dialogs include copy-key UX and restore in-flight protections.

## Roadmap

### Phase 1
- [x] Matrix Backend Runtime
- [x] Basic Sign In/Out
- [x] Room List/Timeline Rendering
  - [ ] Room Names/Group Names
- [x] Timeline Pagination
- [x] Timeline Event Rendering
- [x] Plain-Text Message Send
- [x] Back Up/Restore Identity
- [x] E2EE

### Phase 2

- [ ] Media Upload/Download (Images)
- [ ] Media Upload/Download (Video)
- [ ] Reactions
- [ ] Voice Channels
- [ ] Video Channels
- [ ] Room Members/Invites
- [ ] Room Settings

### Phase 3

- [ ] Theming (Customizable Pallet/Corner Images)
- [ ] Sound Board (Voice/Video Channels)
- [ ] Multi-Account Management
 
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
- `apps/pikachat-desktop`:
  - Slint desktop Matrix client shell

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

### Run the Desktop App

Run the desktop app:

```bash
cargo run -p pikachat-desktop
```

Optional env prefill (does not auto-login):

```bash
PIKACHAT_HOMESERVER='https://matrix.example.org' \
PIKACHAT_USER='@your-user:example.org' \
PIKACHAT_PASSWORD='your-password' \
cargo run -p pikachat-desktop
```

Expected behavior:

- Window opens to the login pane unless a remembered session is restored.
- Login homeserver input is host-only (`matrix.example.org`); app enforces secure `https://`.
- Joined rooms appear in the left sidebar.
- Selecting a room loads timeline items in chat bubbles.
- Sending a message posts it to the selected room.
- `Remember password` controls whether session restore is persisted across relaunches.
- `File -> Logout` asks for confirmation, then clears local chat/cache/keys and returns to login.
- `File -> Quit` exits cleanly.
- `Help -> About Slint...` opens the About view containing Slint's built-in `AboutSlint` widget.
- `Security` menu exposes backup/reset/restore flows.

### Optional Backend Smoke Run

If you want to exercise backend flows directly:

```bash
cargo run -p backend-smoke
```

You can pass env vars for live auth/media/recovery smoke (`PIKACHAT_HOMESERVER`, `PIKACHAT_USER`, `PIKACHAT_PASSWORD`, etc.).

Useful flags include:

- `PIKACHAT_START_SYNC=1`
- `PIKACHAT_DM_TARGET='@user:example.org'`
- `PIKACHAT_GET_RECOVERY_STATUS=1`
- `PIKACHAT_ENABLE_RECOVERY=1`
- `PIKACHAT_RESET_RECOVERY=1`
- `PIKACHAT_RECOVERY_KEY='...'`

## Why “Opinionated Matrix Client for Gamers”

Most Matrix clients are general-purpose. PikaChat is intentionally not trying to be everything for everyone. It is trying to be excellent for game-centric groups that need:

- quick context switching,
- sane defaults,
- durable messaging behavior,
- and a clear path from shell to full-featured client.

## Next Phase

Current priorities after this MVP slice:

- improve encrypted-history recovery UX (clearer guidance and key lifecycle handling),
- continue timeline decryption quality for older encrypted events,
- add media send/download UI,
- refine login/session management UX and account-switch ergonomics,
- keep backend APIs UI-agnostic for future mobile/client reuse.

## Security Notes

- `PLAN.md` is local-only planning scratch and is intentionally not committed.
- Environment credentials are optional prefill only; no password is persisted by the desktop app.
- Do not commit secrets.
