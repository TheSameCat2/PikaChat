//! Frontend-facing state reducer for `pikachat-desktop`.

use std::collections::{HashMap, HashSet};

use backend_core::{
    BackendEvent, BackendLifecycleState, RoomSummary, SendAck, SyncStatus, TimelineItem, TimelineOp,
};
use tracing::{debug, trace, warn};

const DEFAULT_STATUS: &str = "Idle";
const SIDEBAR_MIN_WIDTH_PX: f32 = 220.0;
const SIDEBAR_MAX_FRACTION: f32 = 0.45;

/// Sidebar room row consumed by the Slint UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomView {
    pub room_id: String,
    pub display_name: String,
    pub unread_notifications: u64,
    pub highlight_count: u64,
    pub is_selected: bool,
}

/// Timeline message row consumed by the Slint UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageView {
    pub event_id: Option<String>,
    pub sender: String,
    pub body: String,
    pub is_own: bool,
}

/// Full UI snapshot emitted after state transitions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesktopSnapshot {
    pub rooms: Vec<RoomView>,
    pub messages: Vec<MessageView>,
    pub selected_room_id: Option<String>,
    pub status_text: String,
    pub error_text: Option<String>,
    pub can_send: bool,
}

#[derive(Debug, Default, Clone)]
struct PaginationTracker {
    in_flight: HashSet<String>,
    last_requested_ms: HashMap<String, u64>,
}

impl PaginationTracker {
    fn should_request(
        &self,
        room_id: &str,
        viewport_y: f32,
        now_ms: u64,
        top_threshold_px: f32,
        cooldown_ms: u64,
    ) -> bool {
        if viewport_y < -top_threshold_px {
            return false;
        }
        if self.in_flight.contains(room_id) {
            return false;
        }

        let last = self.last_requested_ms.get(room_id).copied().unwrap_or(0);
        now_ms.saturating_sub(last) >= cooldown_ms
    }

    fn mark_requested(&mut self, room_id: String, now_ms: u64) {
        self.in_flight.insert(room_id.clone());
        self.last_requested_ms.insert(room_id, now_ms);
    }

    fn mark_complete(&mut self, room_id: &str) {
        self.in_flight.remove(room_id);
    }

    fn clear_in_flight(&mut self) {
        self.in_flight.clear();
    }
}

/// Mutable app state that receives backend events and user actions.
#[derive(Debug, Clone)]
pub struct DesktopState {
    own_user_id: String,
    timeline_max_items: usize,
    rooms: Vec<RoomView>,
    selected_room_id: Option<String>,
    timelines: HashMap<String, Vec<TimelineItem>>,
    messages: Vec<MessageView>,
    status_text: String,
    error_text: Option<String>,
    pending_sends: HashSet<String>,
    pagination: PaginationTracker,
}

impl DesktopState {
    /// Create a new reducer state.
    pub fn new(own_user_id: impl Into<String>, timeline_max_items: usize) -> Self {
        Self {
            own_user_id: own_user_id.into(),
            timeline_max_items: timeline_max_items.max(1),
            rooms: Vec::new(),
            selected_room_id: None,
            timelines: HashMap::new(),
            messages: Vec::new(),
            status_text: DEFAULT_STATUS.to_owned(),
            error_text: None,
            pending_sends: HashSet::new(),
            pagination: PaginationTracker::default(),
        }
    }

    /// Current immutable snapshot for UI rendering.
    pub fn snapshot(&self) -> DesktopSnapshot {
        DesktopSnapshot {
            rooms: self.rooms.clone(),
            messages: self.messages.clone(),
            selected_room_id: self.selected_room_id.clone(),
            status_text: self.status_text.clone(),
            error_text: self.error_text.clone(),
            can_send: self.selected_room_id.is_some(),
        }
    }

    /// Set top-level fatal/auth/runtime error.
    pub fn set_error_text(&mut self, text: impl Into<String>) {
        self.error_text = Some(text.into());
    }

    /// Clears the top-level error message.
    pub fn clear_error(&mut self) {
        self.error_text = None;
    }

    /// Return selected room ID, when present.
    pub fn selected_room_id(&self) -> Option<&str> {
        self.selected_room_id.as_deref()
    }

    /// Update room list while preserving backend ordering.
    pub fn replace_rooms(&mut self, rooms: Vec<RoomSummary>) {
        let selected = self.selected_room_id.clone();
        self.rooms = rooms
            .into_iter()
            .map(|room| {
                let display_name = room
                    .name
                    .clone()
                    .filter(|value| !value.trim().is_empty())
                    .unwrap_or_else(|| room.room_id.clone());
                RoomView {
                    room_id: room.room_id.clone(),
                    display_name,
                    unread_notifications: room.unread_notifications,
                    highlight_count: room.highlight_count,
                    is_selected: selected.as_deref() == Some(room.room_id.as_str()),
                }
            })
            .collect();
        debug!(room_count = self.rooms.len(), "room list replaced");

        if let Some(selected_room_id) = &self.selected_room_id
            && !self
                .rooms
                .iter()
                .any(|room| room.room_id == *selected_room_id)
        {
            warn!(room_id = %selected_room_id, "selected room disappeared from room list");
            self.selected_room_id = None;
            self.messages.clear();
        }
    }

    /// Select room by sidebar index and return selected room ID.
    pub fn select_room_by_index(&mut self, index: usize) -> Option<String> {
        let room_id = self.rooms.get(index)?.room_id.clone();
        self.select_room(room_id.clone());
        Some(room_id)
    }

    /// Select room by ID.
    pub fn select_room(&mut self, room_id: String) {
        debug!(%room_id, "state selected room");
        self.selected_room_id = Some(room_id.clone());
        for room in &mut self.rooms {
            room.is_selected = room.room_id == room_id;
        }
        self.rebuild_visible_messages();
    }

    /// Mark a send as pending using its client transaction ID.
    pub fn mark_send_requested(&mut self, client_txn_id: String) {
        self.pending_sends.insert(client_txn_id);
    }

    /// Handle send acknowledgement from backend.
    pub fn handle_send_ack(&mut self, ack: SendAck) {
        self.pending_sends.remove(&ack.client_txn_id);
        if let Some(error_code) = ack.error_code {
            warn!(
                client_txn_id = %ack.client_txn_id,
                error_code = %error_code,
                "send acknowledgement reported failure"
            );
            self.error_text = Some(format!("send failed ({error_code})"));
        } else {
            debug!(client_txn_id = %ack.client_txn_id, "send acknowledgement succeeded");
            self.clear_error();
        }
    }

    /// Feed one backend event into the reducer.
    pub fn handle_backend_event(&mut self, event: BackendEvent) {
        match event {
            BackendEvent::StateChanged { state } => {
                self.status_text = lifecycle_label(state).to_owned();
            }
            BackendEvent::AuthResult {
                success,
                error_code,
            } => {
                if success {
                    self.status_text = "Authenticated".to_owned();
                    self.clear_error();
                } else {
                    let code = error_code.unwrap_or_else(|| "unknown".to_owned());
                    self.status_text = "Authentication failed".to_owned();
                    self.error_text = Some(auth_error_text(&code));
                }
            }
            BackendEvent::SyncStatus(SyncStatus {
                running,
                lag_hint_ms,
            }) => {
                self.status_text = if running {
                    if let Some(hint) = lag_hint_ms {
                        format!("Reconnecting (retry in {} ms)", hint)
                    } else {
                        "Connected".to_owned()
                    }
                } else {
                    "Disconnected".to_owned()
                };
            }
            BackendEvent::RoomListUpdated { rooms } => {
                self.replace_rooms(rooms);
            }
            BackendEvent::RoomTimelineSnapshot { room_id, items } => {
                self.pagination.mark_complete(&room_id);
                trace!(
                    room_id = %room_id,
                    item_count = items.len(),
                    "received room timeline snapshot"
                );
                self.timelines.insert(
                    room_id.clone(),
                    dedupe_and_trim(items, self.timeline_max_items),
                );
                if self.selected_room_id.as_deref() == Some(room_id.as_str()) {
                    self.rebuild_visible_messages();
                }
            }
            BackendEvent::RoomTimelineDelta { room_id, ops } => {
                self.pagination.mark_complete(&room_id);
                trace!(
                    room_id = %room_id,
                    op_count = ops.len(),
                    "received room timeline delta"
                );
                let timeline = self.timelines.entry(room_id.clone()).or_default();
                apply_delta_lenient(timeline, &ops);
                let normalized = dedupe_and_trim(std::mem::take(timeline), self.timeline_max_items);
                *timeline = normalized;
                if self.selected_room_id.as_deref() == Some(room_id.as_str()) {
                    self.rebuild_visible_messages();
                }
            }
            BackendEvent::SendAck(ack) => {
                self.handle_send_ack(ack);
            }
            BackendEvent::FatalError { code, message, .. } => {
                self.pagination.clear_in_flight();
                warn!(%code, %message, "backend fatal error surfaced to state");
                self.status_text = "Backend error".to_owned();
                self.error_text = Some(format!("{code}: {message}"));
            }
            BackendEvent::MediaUploadAck(_)
            | BackendEvent::MediaDownloadAck(_)
            | BackendEvent::CryptoStatus(_) => {}
        }
    }

    /// Returns pagination target room and marks request as in-flight if trigger conditions match.
    pub fn request_pagination_if_needed(
        &mut self,
        viewport_y: f32,
        now_ms: u64,
        top_threshold_px: f32,
        cooldown_ms: u64,
        limit: u16,
    ) -> Option<(String, u16)> {
        let room_id = self.selected_room_id.clone()?;
        if self.pagination.should_request(
            &room_id,
            viewport_y,
            now_ms,
            top_threshold_px,
            cooldown_ms,
        ) {
            self.pagination.mark_requested(room_id.clone(), now_ms);
            Some((room_id, limit.max(1)))
        } else {
            None
        }
    }

    fn rebuild_visible_messages(&mut self) {
        let Some(room_id) = &self.selected_room_id else {
            self.messages.clear();
            return;
        };

        self.messages = self
            .timelines
            .get(room_id)
            .map(|items| {
                items
                    .iter()
                    .map(|item| MessageView {
                        event_id: item.event_id.clone(),
                        sender: item.sender.clone(),
                        body: item.body.clone(),
                        is_own: item.sender == self.own_user_id,
                    })
                    .collect()
            })
            .unwrap_or_default();
    }
}

/// Clamp sidebar width in pixels using window-relative max bounds.
pub fn clamp_sidebar_width(window_width: f32, requested_width: f32) -> f32 {
    let safe_window = window_width.max(1.0);
    let min_width = SIDEBAR_MIN_WIDTH_PX.min(safe_window);
    let max_width = (safe_window * SIDEBAR_MAX_FRACTION).max(min_width);
    requested_width.clamp(min_width, max_width)
}

fn lifecycle_label(state: BackendLifecycleState) -> &'static str {
    match state {
        BackendLifecycleState::Cold => "Cold",
        BackendLifecycleState::Configured => "Configured",
        BackendLifecycleState::Authenticating => "Authenticating",
        BackendLifecycleState::Authenticated => "Authenticated",
        BackendLifecycleState::Syncing => "Syncing",
        BackendLifecycleState::LoggedOut => "Logged out",
        BackendLifecycleState::Fatal => "Fatal",
    }
}

fn auth_error_text(code: &str) -> String {
    if code == "crypto_store_account_mismatch" {
        "authentication failed (crypto_store_account_mismatch): local data dir belongs to a different account; set PIKACHAT_DATA_DIR or remove the existing store".to_owned()
    } else {
        format!("authentication failed ({code})")
    }
}

fn apply_delta_lenient(items: &mut Vec<TimelineItem>, ops: &[TimelineOp]) {
    for op in ops {
        match op {
            TimelineOp::Append(item) => items.push(item.clone()),
            TimelineOp::Prepend(item) => items.insert(0, item.clone()),
            TimelineOp::UpdateBody { event_id, new_body } => {
                if let Some(item) = items
                    .iter_mut()
                    .find(|item| item.event_id.as_deref() == Some(event_id.as_str()))
                {
                    item.body = new_body.clone();
                }
            }
            TimelineOp::Remove { event_id } => {
                if let Some(index) = items
                    .iter()
                    .position(|item| item.event_id.as_deref() == Some(event_id.as_str()))
                {
                    items.remove(index);
                }
            }
            TimelineOp::Clear => items.clear(),
        }
    }
}

fn dedupe_and_trim(items: Vec<TimelineItem>, max_items: usize) -> Vec<TimelineItem> {
    let mut seen_event_ids = HashSet::new();
    let mut reversed = Vec::with_capacity(items.len());

    for item in items.into_iter().rev() {
        let keep = match item.event_id.as_deref() {
            Some(event_id) => seen_event_ids.insert(event_id.to_owned()),
            None => true,
        };
        if keep {
            reversed.push(item);
        }
    }

    reversed.reverse();
    if reversed.len() > max_items {
        let excess = reversed.len() - max_items;
        reversed.drain(0..excess);
    }
    reversed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(event_id: &str, sender: &str, body: &str) -> TimelineItem {
        TimelineItem {
            event_id: Some(event_id.to_owned()),
            sender: sender.to_owned(),
            body: body.to_owned(),
            timestamp_ms: 1_700_000_000,
        }
    }

    fn room(room_id: &str, name: &str, unread: u64, highlight: u64) -> RoomSummary {
        RoomSummary {
            room_id: room_id.to_owned(),
            name: Some(name.to_owned()),
            unread_notifications: unread,
            highlight_count: highlight,
            is_direct: false,
        }
    }

    #[test]
    fn room_list_keeps_backend_order_and_no_auto_selection() {
        let mut state = DesktopState::new("@alice:example.org", 50);
        state.replace_rooms(vec![
            room("!b:example.org", "B", 2, 1),
            room("!a:example.org", "A", 0, 0),
        ]);

        let snapshot = state.snapshot();
        assert_eq!(snapshot.selected_room_id, None);
        assert_eq!(snapshot.rooms[0].room_id, "!b:example.org");
        assert_eq!(snapshot.rooms[0].unread_notifications, 2);
        assert_eq!(snapshot.rooms[0].highlight_count, 1);
        assert_eq!(snapshot.rooms[1].room_id, "!a:example.org");
    }

    #[test]
    fn selecting_room_marks_selected_state() {
        let mut state = DesktopState::new("@alice:example.org", 50);
        state.replace_rooms(vec![room("!r1:example.org", "R1", 0, 0)]);
        let selected = state.select_room_by_index(0);

        assert_eq!(selected.as_deref(), Some("!r1:example.org"));
        let snapshot = state.snapshot();
        assert_eq!(
            snapshot.selected_room_id.as_deref(),
            Some("!r1:example.org")
        );
        assert!(snapshot.rooms[0].is_selected);
    }

    #[test]
    fn snapshot_mapping_sets_own_message_alignment() {
        let mut state = DesktopState::new("@alice:example.org", 50);
        state.replace_rooms(vec![room("!r1:example.org", "R1", 0, 0)]);
        state.select_room("!r1:example.org".to_owned());
        state.handle_backend_event(BackendEvent::RoomTimelineSnapshot {
            room_id: "!r1:example.org".to_owned(),
            items: vec![
                item("$1", "@alice:example.org", "mine"),
                item("$2", "@bob:example.org", "theirs"),
            ],
        });

        let snapshot = state.snapshot();
        assert_eq!(snapshot.messages.len(), 2);
        assert!(snapshot.messages[0].is_own);
        assert!(!snapshot.messages[1].is_own);
    }

    #[test]
    fn send_ack_failure_sets_error_and_clears_pending() {
        let mut state = DesktopState::new("@alice:example.org", 50);
        state.mark_send_requested("txn-1".to_owned());
        state.handle_send_ack(SendAck {
            client_txn_id: "txn-1".to_owned(),
            event_id: None,
            error_code: Some("forbidden".to_owned()),
        });

        let snapshot = state.snapshot();
        assert_eq!(
            snapshot.error_text.as_deref(),
            Some("send failed (forbidden)")
        );
    }

    #[test]
    fn sync_status_labels_use_connected_and_reconnecting() {
        let mut state = DesktopState::new("@alice:example.org", 50);

        state.handle_backend_event(BackendEvent::SyncStatus(SyncStatus {
            running: true,
            lag_hint_ms: None,
        }));
        assert_eq!(state.snapshot().status_text, "Connected");

        state.handle_backend_event(BackendEvent::SyncStatus(SyncStatus {
            running: true,
            lag_hint_ms: Some(1500),
        }));
        assert_eq!(
            state.snapshot().status_text,
            "Reconnecting (retry in 1500 ms)"
        );

        state.handle_backend_event(BackendEvent::SyncStatus(SyncStatus {
            running: false,
            lag_hint_ms: None,
        }));
        assert_eq!(state.snapshot().status_text, "Disconnected");
    }

    #[test]
    fn auth_mismatch_error_is_actionable() {
        let mut state = DesktopState::new("@alice:example.org", 50);
        state.handle_backend_event(BackendEvent::AuthResult {
            success: false,
            error_code: Some("crypto_store_account_mismatch".to_owned()),
        });
        let text = state
            .snapshot()
            .error_text
            .expect("error text should be present");
        assert!(text.contains("PIKACHAT_DATA_DIR"));
        assert!(text.contains("different account"));
    }

    #[test]
    fn pagination_trigger_respects_top_threshold_inflight_and_cooldown() {
        let mut state = DesktopState::new("@alice:example.org", 50);
        state.replace_rooms(vec![room("!r1:example.org", "R1", 0, 0)]);
        state.select_room("!r1:example.org".to_owned());

        assert_eq!(
            state.request_pagination_if_needed(-200.0, 1_000, 80.0, 500, 30),
            None
        );

        let first = state.request_pagination_if_needed(-40.0, 1_000, 80.0, 500, 30);
        assert_eq!(first, Some(("!r1:example.org".to_owned(), 30)));

        let second = state.request_pagination_if_needed(-10.0, 1_200, 80.0, 500, 30);
        assert_eq!(second, None);

        state.handle_backend_event(BackendEvent::RoomTimelineSnapshot {
            room_id: "!r1:example.org".to_owned(),
            items: vec![],
        });

        let third = state.request_pagination_if_needed(-20.0, 1_300, 80.0, 500, 30);
        assert_eq!(third, None);

        let fourth = state.request_pagination_if_needed(-20.0, 1_600, 80.0, 500, 30);
        assert_eq!(fourth, Some(("!r1:example.org".to_owned(), 30)));
    }

    #[test]
    fn dedupe_and_trim_keeps_latest_event_instance() {
        let mut state = DesktopState::new("@alice:example.org", 2);
        state.replace_rooms(vec![room("!r1:example.org", "R1", 0, 0)]);
        state.select_room("!r1:example.org".to_owned());
        state.handle_backend_event(BackendEvent::RoomTimelineSnapshot {
            room_id: "!r1:example.org".to_owned(),
            items: vec![
                item("$1", "@alice:example.org", "v1"),
                item("$2", "@alice:example.org", "v2"),
                item("$1", "@alice:example.org", "v1-new"),
            ],
        });

        let snapshot = state.snapshot();
        assert_eq!(snapshot.messages.len(), 2);
        assert_eq!(snapshot.messages[0].event_id.as_deref(), Some("$2"));
        assert_eq!(snapshot.messages[1].event_id.as_deref(), Some("$1"));
        assert_eq!(snapshot.messages[1].body, "v1-new");
    }

    #[test]
    fn apply_delta_handles_update_and_remove_leniently() {
        let mut state = DesktopState::new("@alice:example.org", 50);
        state.replace_rooms(vec![room("!r1:example.org", "R1", 0, 0)]);
        state.select_room("!r1:example.org".to_owned());
        state.handle_backend_event(BackendEvent::RoomTimelineSnapshot {
            room_id: "!r1:example.org".to_owned(),
            items: vec![item("$1", "@alice:example.org", "one")],
        });
        state.handle_backend_event(BackendEvent::RoomTimelineDelta {
            room_id: "!r1:example.org".to_owned(),
            ops: vec![
                TimelineOp::UpdateBody {
                    event_id: "$1".to_owned(),
                    new_body: "one-updated".to_owned(),
                },
                TimelineOp::Remove {
                    event_id: "$404".to_owned(),
                },
            ],
        });

        let snapshot = state.snapshot();
        assert_eq!(snapshot.messages.len(), 1);
        assert_eq!(snapshot.messages[0].body, "one-updated");
    }

    #[test]
    fn clamps_sidebar_width_within_bounds() {
        let width = clamp_sidebar_width(1_000.0, 500.0);
        assert_eq!(width, 450.0);

        let width = clamp_sidebar_width(1_000.0, 100.0);
        assert_eq!(width, 220.0);
    }
}
