# PikaChat Agent Notes

## Project Goal
Build a Rust-first backend for a desktop Matrix client (UI layer later), with reliable sync, messaging, E2EE support, and solid test coverage.
- UI direction: Slint for desktop UI.
- Architecture direction: keep backend crates UI-agnostic so they can be reused by other clients/platforms (including mobile).

## Available Tools and Access
- Local shell access is available.
- Rust toolchain is installed and available:
  - `cargo`
  - `rustc`
- Common CLI utilities are available and preferred for fast repo work:
  - `rg` / `rg --files` for search
  - `cargo fmt --all`
  - `cargo test --workspace`
- Git is available for local version control.
- GitHub CLI is installed and authenticated:
  - `gh` commands can be used for repo management, PRs, and remote operations.
  - Current repo remote uses SSH.
- Network/web access is available for research and documentation lookups.

## Repo-Specific Rules
- `PLAN.md` is local-only planning scratch and must never be committed.
- Keep credentials/secrets out of committed files and commit history.
- Prefer tests with `cargo test --workspace` before pushing substantial changes.
- If unexpected tracked edits appear that were not intentionally made, pause and confirm with the user before proceeding.
- Keep commit scope focused; do not include `PLAN.md` in any commit.

## Live Validation Notes
- Live homeserver testing is allowed when needed.
- Prefer passing live credentials via environment variables instead of storing them in tracked files.
- Logging is available via `tracing` with severity filters:
  - global: `RUST_LOG=...`
  - project fallback: `PIKACHAT_LOG=...`
  - desktop-specific: `PIKACHAT_DESKTOP_LOG=...`
  - backend-smoke-specific: `PIKACHAT_SMOKE_LOG=...`

## User Collaboration Preferences
- Execute plans to completion autonomously.
- Only stop when:
  - required information is missing,
  - a true architecture-impacting decision is needed,
  - or the plan is fully complete.
- Keep status concise and action-oriented; continue implementation by default.
