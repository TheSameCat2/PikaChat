# PikaChat Agent Notes

## Project Goal
Build a Rust-first backend for a desktop Matrix client (UI layer later), with reliable sync, messaging, E2EE support, and solid test coverage.

## Available Tools and Access
- Local shell access is available.
- Rust toolchain is installed and available:
  - `cargo`
  - `rustc`
- Git is available for local version control.
- GitHub CLI is installed and authenticated:
  - `gh` commands can be used for repo management, PRs, and remote operations.
  - Current repo remote uses SSH.
- Network/web access is available for research and documentation lookups.

## Repo-Specific Rules
- `PLAN.md` is local-only planning scratch and must never be committed.
- Keep credentials/secrets out of committed files and commit history.
- Prefer tests with `cargo test --workspace` before pushing substantial changes.

## Live Validation Notes
- Live homeserver testing is allowed when needed.
- Prefer passing live credentials via environment variables instead of storing them in tracked files.
