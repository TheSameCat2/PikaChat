## Rust Backend Plan for a Desktop Matrix Client (PikaChat)

### Summary
Build a production-oriented Rust backend as an **in-process library** for a future desktop UI (e.g. imgui), using `matrix-sdk`, `matrix-sdk-ui`, and `matrix-sdk-sqlite`.
Milestone 1 scope: **single-account (migration-ready), password login, core chat + E2EE, Linux/macOS/Windows, OS keyring-protected local secrets, async event stream API**.

### Architecture
1. **Crate layout**
- `crates/backend-core`: public backend API (commands/events/DTOs/errors).
- `crates/backend-matrix`: Matrix SDK integration and state orchestration.
- `crates/backend-platform`: OS keyring + path/platform adapters.
- `apps/backend-smoke` (optional): local non-UI smoke harness.

2. **Runtime**
- Tokio runtime with supervisor tasks:
- auth/session state machine
- sync worker
- per-room timeline workers
- outbound send queue with retry/backoff

3. **Storage/Security**
- SDK SQLite state store in app-data path.
- Encryption secret/passphrase sourced from OS keyring only in M1.
- Explicit failure on keyring-unavailable (no insecure fallback).

### Public Interfaces / Types
1. **Commands**
- `Init { homeserver, data_dir }`
- `LoginPassword { user_id_or_localpart, password }`
- `RestoreSession`
- `StartSync`, `StopSync`
- `ListRooms`
- `OpenRoom { room_id }`
- `PaginateBack { room_id, limit }`
- `SendMessage { room_id, client_txn_id, body, msgtype }`
- `EditMessage { room_id, target_event_id, new_body, client_txn_id }`
- `RedactMessage { room_id, target_event_id, reason }`
- `Logout`

2. **Events**
- `StateChanged`
- `AuthResult`
- `SyncStatus`
- `RoomListUpdated`
- `RoomTimelineDelta`
- `SendAck`
- `CryptoStatus`
- `FatalError`

3. **Errors**
- Stable categories: `Config`, `Auth`, `Network`, `RateLimited`, `Crypto`, `Storage`, `Serialization`, `Internal`
- Include machine-readable `code` + optional `retry_after`.

### Testing Plan (explicitly includes unit tests)
1. **Unit tests (required)**
- Command/state transitions.
- Event normalization and DTO mapping.
- Retry/backoff behavior.
- Error classification and code stability.
- Keyring adapter behavior with mocks/fakes.
- Timeline diff/merge logic and pagination boundaries.

2. **Integration tests**
- Local/ephemeral homeserver for login/sync/send/edit/redact.
- E2EE flows between test users.
- Session restore after restart.

3. **Live homeserver validation (using your account when available)**
- Dedicated test phase after local integration green.
- Non-destructive test checklist:
- login + initial sync latency
- room list/timeline correctness
- send/edit/redact roundtrip
- encrypted room send/receive
- restart/session restore
- Capture logs/metrics for regressions.

4. **Acceptance criteria**
- Unit test suite passes in CI on all targets.
- Integration suite passes on Linux CI (plus platform smoke where feasible).
- Manual live-homeserver validation checklist passes with your account.
- No secrets in logs; error codes stable and actionable.

### Edge Cases & Failure Modes
- Invalid credentials/homeserver.
- Session invalidation mid-run.
- Offline/reconnect behavior.
- E2EE key query delays/device trust ambiguity.
- Duplicate send retries (`client_txn_id` idempotency).
- SQLite/keyring failures.
- Large timeline memory pressure and pagination throttling.
- Graceful shutdown during in-flight operations.

### Rollout / Compatibility
- Pin tested SDK version and gate upgrades with CI.
- Add structured logging (`tracing`) with redaction policy.
- Keep schema/API migration notes for future multi-account and OIDC.

### Assumptions / Defaults
- In-process Rust backend (no sidecar in M1).
- Password auth only in M1; OIDC deferred.
- Single account in M1, with migration-ready design.
- No VoIP/spaces/threads/search indexing in M1.
