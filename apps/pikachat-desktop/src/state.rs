//! Frontend-facing state reducer for `pikachat-desktop`.

use std::collections::{HashMap, HashSet};

use backend_core::types::{InviteAction, InviteActionAck, RoomMembership};
use backend_core::{
    BackendEvent, BackendLifecycleState, KeyBackupState, RecoveryState, RecoveryStatus,
    RoomSummary, SendAck, SyncStatus, TimelineItem, TimelineOp,
};
use tracing::{debug, trace, warn};

const DEFAULT_STATUS: &str = "Idle";
const SIDEBAR_MIN_WIDTH_PX: f32 = 220.0;
const SIDEBAR_MAX_FRACTION: f32 = 0.45;
const SECURITY_TITLE_STATUS: &str = "Identity Backup Status";
const SECURITY_TITLE_CREATING: &str = "Creating Identity Backup";
const SECURITY_TITLE_CREATED: &str = "Identity Backup Created";
const SECURITY_TITLE_RESTORE: &str = "Identity Restore";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SecurityDialogMode {
    None,
    Status,
    Creating,
    RecoveryKey,
    Info,
}

/// Sidebar room row consumed by the Slint UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomView {
    pub room_id: String,
    pub display_name: String,
    pub unread_notifications: u64,
    pub highlight_count: u64,
    pub is_selected: bool,
    pub membership: RoomMembership,
    pub invite_pending: bool,
    pub invite_pending_text: String,
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
    pub selected_room_is_invite: bool,
    pub selected_room_invite_pending_text: Option<String>,
    pub status_text: String,
    pub error_text: Option<String>,
    pub can_send: bool,
    pub show_security_dialog: bool,
    pub security_dialog_title: String,
    pub security_dialog_body: String,
    pub security_show_copy_button: bool,
    pub security_copy_button_text: String,
    pub security_show_restore_input: bool,
    pub security_restore_button_text: String,
    pub security_restore_in_flight: bool,
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
    pending_invite_actions: HashMap<String, InviteAction>,
    pending_invite_actions_by_txn: HashMap<String, String>,
    pagination: PaginationTracker,
    show_security_dialog: bool,
    security_dialog_title: String,
    security_dialog_body: String,
    security_show_copy_button: bool,
    security_copy_button_text: String,
    security_show_restore_input: bool,
    security_restore_button_text: String,
    security_restore_in_flight: bool,
    security_dialog_mode: SecurityDialogMode,
    security_recovery_key: Option<String>,
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
            pending_invite_actions: HashMap::new(),
            pending_invite_actions_by_txn: HashMap::new(),
            pagination: PaginationTracker::default(),
            show_security_dialog: false,
            security_dialog_title: String::new(),
            security_dialog_body: String::new(),
            security_show_copy_button: false,
            security_copy_button_text: "Copy Key".to_owned(),
            security_show_restore_input: false,
            security_restore_button_text: "Restore".to_owned(),
            security_restore_in_flight: false,
            security_dialog_mode: SecurityDialogMode::None,
            security_recovery_key: None,
        }
    }

    /// Current immutable snapshot for UI rendering.
    pub fn snapshot(&self) -> DesktopSnapshot {
        let selected_room = self.selected_room();
        let selected_room_is_invite = selected_room
            .map(|room| room.membership == RoomMembership::Invited)
            .unwrap_or(false);
        let selected_room_invite_pending_text = selected_room
            .filter(|room| room.invite_pending)
            .map(|room| room.invite_pending_text.clone());
        DesktopSnapshot {
            rooms: self.rooms.clone(),
            messages: self.messages.clone(),
            selected_room_id: self.selected_room_id.clone(),
            selected_room_is_invite,
            selected_room_invite_pending_text,
            status_text: self.status_text.clone(),
            error_text: self.error_text.clone(),
            can_send: self.can_send_message(),
            show_security_dialog: self.show_security_dialog,
            security_dialog_title: self.security_dialog_title.clone(),
            security_dialog_body: self.security_dialog_body.clone(),
            security_show_copy_button: self.security_show_copy_button,
            security_copy_button_text: self.security_copy_button_text.clone(),
            security_show_restore_input: self.security_show_restore_input,
            security_restore_button_text: self.security_restore_button_text.clone(),
            security_restore_in_flight: self.security_restore_in_flight,
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

    /// Show a modal-style security dialog in the UI.
    pub fn show_security_dialog(&mut self, title: impl Into<String>, body: impl Into<String>) {
        self.show_security_dialog = true;
        self.security_dialog_title = title.into();
        self.security_dialog_body = body.into();
        self.security_dialog_mode = SecurityDialogMode::Info;
        self.clear_recovery_key_copy_state();
        self.clear_restore_input_state();
    }

    /// Show a status-check dialog.
    pub fn show_security_status_dialog(&mut self, body: impl Into<String>) {
        self.show_security_dialog = true;
        self.security_dialog_title = SECURITY_TITLE_STATUS.to_owned();
        self.security_dialog_body = body.into();
        self.security_dialog_mode = SecurityDialogMode::Status;
        self.clear_recovery_key_copy_state();
        self.clear_restore_input_state();
    }

    /// Show a backup creation progress dialog.
    pub fn show_security_creating_dialog(&mut self, body: impl Into<String>) {
        self.show_security_dialog = true;
        self.security_dialog_title = SECURITY_TITLE_CREATING.to_owned();
        self.security_dialog_body = body.into();
        self.security_dialog_mode = SecurityDialogMode::Creating;
        self.clear_recovery_key_copy_state();
        self.clear_restore_input_state();
    }

    /// Show restore prompt dialog with input controls visible.
    pub fn show_security_restore_prompt_dialog(
        &mut self,
        title: impl Into<String>,
        body: impl Into<String>,
    ) {
        self.show_security_dialog = true;
        self.security_dialog_title = title.into();
        self.security_dialog_body = body.into();
        self.security_dialog_mode = SecurityDialogMode::Info;
        self.clear_recovery_key_copy_state();
        self.security_show_restore_input = true;
        self.security_restore_button_text = "Restore".to_owned();
        self.security_restore_in_flight = false;
    }

    /// Mark restore request as in flight in the dialog.
    ///
    /// Returns `true` if restore was transitioned into in-flight mode,
    /// or `false` if a restore request is already running.
    pub fn begin_restore_request(&mut self) -> bool {
        if self.security_restore_in_flight {
            return false;
        }
        self.security_restore_in_flight = true;
        self.security_restore_button_text = "Restoring...".to_owned();
        true
    }

    fn show_security_recovery_key_dialog(&mut self, recovery_key: String) {
        self.show_security_dialog = true;
        self.security_dialog_title = SECURITY_TITLE_CREATED.to_owned();
        self.security_dialog_body = format!(
            "Store this recovery key safely. It can decrypt your encrypted history on new devices.\n\nRecovery key:\n{}",
            recovery_key
        );
        self.security_recovery_key = Some(recovery_key);
        self.security_show_copy_button = true;
        self.security_copy_button_text = "Copy Key".to_owned();
        self.security_dialog_mode = SecurityDialogMode::RecoveryKey;
        self.clear_restore_input_state();
    }

    /// Hide the security dialog in the UI.
    pub fn dismiss_security_dialog(&mut self) {
        self.show_security_dialog = false;
        self.security_dialog_mode = SecurityDialogMode::None;
        self.clear_recovery_key_copy_state();
        self.clear_restore_input_state();
    }

    /// Returns the currently visible recovery key (if one is shown in the dialog).
    pub fn recovery_key_for_copy(&self) -> Option<String> {
        self.security_recovery_key.clone()
    }

    /// Mark the recovery key copy action as successful.
    pub fn mark_recovery_key_copied(&mut self) {
        if self.security_recovery_key.is_some() && self.security_show_copy_button {
            self.security_copy_button_text = "Copied".to_owned();
        }
    }

    fn clear_recovery_key_copy_state(&mut self) {
        self.security_show_copy_button = false;
        self.security_copy_button_text = "Copy Key".to_owned();
        self.security_recovery_key = None;
    }

    fn clear_restore_input_state(&mut self) {
        self.security_show_restore_input = false;
        self.security_restore_button_text = "Restore".to_owned();
        self.security_restore_in_flight = false;
    }

    /// Return selected room ID, when present.
    pub fn selected_room_id(&self) -> Option<&str> {
        self.selected_room_id.as_deref()
    }

    /// Returns `true` when the selected room can accept outgoing messages.
    pub fn can_send_message(&self) -> bool {
        self.selected_room()
            .map(|room| room.membership == RoomMembership::Joined)
            .unwrap_or(false)
    }

    /// Returns `true` when the room exists and is currently an invite.
    pub fn is_invite_room(&self, room_id: &str) -> bool {
        self.rooms
            .iter()
            .find(|room| room.room_id == room_id)
            .map(|room| room.membership == RoomMembership::Invited)
            .unwrap_or(false)
    }

    /// Returns `true` when an invite action is currently pending for this room.
    pub fn has_pending_invite_action(&self, room_id: &str) -> bool {
        self.pending_invite_actions.contains_key(room_id)
    }

    /// Update room list while preserving backend ordering.
    pub fn replace_rooms(&mut self, rooms: Vec<RoomSummary>) {
        let selected = self.selected_room_id.clone();
        let mut invited = Vec::new();
        let mut non_invited = Vec::new();
        for room in rooms {
            let room_id = room.room_id.clone();
            let membership = room.membership;
            let pending_action = self.pending_invite_actions.get(&room_id).copied();
            let mapped = RoomView {
                room_id: room_id.clone(),
                display_name: room
                    .name
                    .clone()
                    .filter(|value| !value.trim().is_empty())
                    .unwrap_or_else(|| room_id.clone()),
                unread_notifications: room.unread_notifications,
                highlight_count: room.highlight_count,
                is_selected: selected.as_deref() == Some(room_id.as_str()),
                membership,
                invite_pending: pending_action.is_some(),
                invite_pending_text: pending_action
                    .map(invite_pending_text)
                    .unwrap_or_default()
                    .to_owned(),
            };
            if membership == RoomMembership::Invited {
                invited.push(mapped);
            } else {
                non_invited.push(mapped);
            }
        }
        invited.append(&mut non_invited);
        self.rooms = invited;
        self.pending_invite_actions
            .retain(|room_id, _| self.rooms.iter().any(|room| room.room_id == *room_id));
        self.pending_invite_actions_by_txn
            .retain(|_, room_id| self.pending_invite_actions.contains_key(room_id));
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

    /// Mark invite action request as pending for room + transaction id.
    pub fn mark_invite_action_requested(
        &mut self,
        room_id: String,
        client_txn_id: String,
        action: InviteAction,
    ) -> bool {
        if !self.is_invite_room(&room_id) || self.has_pending_invite_action(&room_id) {
            return false;
        }
        self.pending_invite_actions.insert(room_id.clone(), action);
        self.pending_invite_actions_by_txn
            .insert(client_txn_id, room_id.clone());
        if let Some(room) = self.rooms.iter_mut().find(|room| room.room_id == room_id) {
            room.invite_pending = true;
            room.invite_pending_text = invite_pending_text(action).to_owned();
        }
        true
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

    fn selected_room(&self) -> Option<&RoomView> {
        let selected_room_id = self.selected_room_id.as_deref()?;
        self.rooms
            .iter()
            .find(|room| room.room_id == selected_room_id)
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

    /// Handle invite action acknowledgement from backend.
    pub fn handle_invite_action_ack(&mut self, ack: InviteActionAck) {
        let room_id = self
            .pending_invite_actions_by_txn
            .remove(&ack.client_txn_id)
            .unwrap_or_else(|| ack.room_id.clone());
        self.pending_invite_actions.remove(&room_id);
        if let Some(room) = self.rooms.iter_mut().find(|room| room.room_id == room_id) {
            room.invite_pending = false;
            room.invite_pending_text.clear();
        }

        if let Some(error_code) = ack.error_code {
            let action = match ack.action {
                InviteAction::Accept => "accept",
                InviteAction::Reject => "reject",
            };
            self.error_text = Some(format!("invite {action} failed ({error_code})"));
        } else {
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
            BackendEvent::InviteActionAck(ack) => {
                self.handle_invite_action_ack(ack);
            }
            BackendEvent::RecoveryStatus(status) => {
                // Avoid replacing freshly shown recovery keys with trailing status events.
                if matches!(self.security_dialog_mode, SecurityDialogMode::Status) {
                    self.show_security_status_dialog(format_recovery_status(&status));
                }
            }
            BackendEvent::RecoveryEnableAck(ack) => {
                if let Some(recovery_key) = ack.recovery_key {
                    self.show_security_recovery_key_dialog(recovery_key);
                    self.clear_error();
                } else if let Some(error_code) = ack.error_code {
                    if error_code == "recovery_already_enabled" {
                        self.show_security_dialog(
                            SECURITY_TITLE_STATUS,
                            "Identity backup is already enabled on this client. Use \"Check Backup Status\" to inspect current state and existing recovery setup.",
                        );
                        self.clear_error();
                    } else if error_code == "recovery_backup_exists" {
                        self.show_security_dialog(
                            SECURITY_TITLE_STATUS,
                            "A server-side key backup already exists for this account. Use \"Check Backup Status\" to inspect state, then restore secrets with your existing recovery key/passphrase instead of creating a new backup.",
                        );
                        self.clear_error();
                    } else {
                        self.error_text = Some(format!("identity backup failed ({error_code})"));
                    }
                } else {
                    self.error_text = Some("identity backup failed (unknown)".to_owned());
                }
            }
            BackendEvent::RecoveryRestoreAck(ack) => {
                if let Some(error_code) = ack.error_code {
                    self.show_security_dialog(
                        SECURITY_TITLE_RESTORE,
                        format!(
                            "Identity restore failed. Verify your recovery key or passphrase and try again.\n\nError code: {error_code}"
                        ),
                    );
                    self.clear_error();
                } else {
                    self.show_security_dialog(
                        SECURITY_TITLE_RESTORE,
                        "Recovery secrets were restored successfully. Encrypted history can now be decrypted on this device.".to_owned(),
                    );
                    self.clear_error();
                }
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

fn invite_pending_text(action: InviteAction) -> &'static str {
    match action {
        InviteAction::Accept => "Accepting invite...",
        InviteAction::Reject => "Rejecting invite...",
    }
}

fn format_recovery_status(status: &RecoveryStatus) -> String {
    format!(
        "Backup state: {}\nBackup enabled locally: {}\nBackup exists on server: {}\nRecovery state: {}",
        format_key_backup_state(status.backup_state),
        if status.backup_enabled { "yes" } else { "no" },
        if status.backup_exists_on_server {
            "yes"
        } else {
            "no"
        },
        format_recovery_state(status.recovery_state),
    )
}

fn format_key_backup_state(state: KeyBackupState) -> &'static str {
    match state {
        KeyBackupState::Unknown => "Unknown",
        KeyBackupState::Creating => "Creating",
        KeyBackupState::Enabling => "Enabling",
        KeyBackupState::Resuming => "Resuming",
        KeyBackupState::Enabled => "Enabled",
        KeyBackupState::Downloading => "Downloading",
        KeyBackupState::Disabling => "Disabling",
    }
}

fn format_recovery_state(state: RecoveryState) -> &'static str {
    match state {
        RecoveryState::Unknown => "Unknown",
        RecoveryState::Enabled => "Enabled",
        RecoveryState::Disabled => "Disabled",
        RecoveryState::Incomplete => "Incomplete",
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

    fn room(
        room_id: &str,
        name: &str,
        unread: u64,
        highlight: u64,
        membership: RoomMembership,
    ) -> RoomSummary {
        RoomSummary {
            room_id: room_id.to_owned(),
            name: Some(name.to_owned()),
            unread_notifications: unread,
            highlight_count: highlight,
            is_direct: false,
            membership,
        }
    }

    #[test]
    fn room_list_places_invites_first_with_stable_backend_order_and_no_auto_selection() {
        let mut state = DesktopState::new("@alice:example.org", 50);
        state.replace_rooms(vec![
            room(
                "!joined-2:example.org",
                "Joined2",
                2,
                1,
                RoomMembership::Joined,
            ),
            room(
                "!invite-1:example.org",
                "Invite1",
                0,
                0,
                RoomMembership::Invited,
            ),
            room(
                "!joined-1:example.org",
                "Joined1",
                0,
                0,
                RoomMembership::Joined,
            ),
            room(
                "!invite-2:example.org",
                "Invite2",
                0,
                0,
                RoomMembership::Invited,
            ),
        ]);

        let snapshot = state.snapshot();
        assert_eq!(snapshot.selected_room_id, None);
        assert_eq!(snapshot.rooms[0].room_id, "!invite-1:example.org");
        assert_eq!(snapshot.rooms[1].room_id, "!invite-2:example.org");
        assert_eq!(snapshot.rooms[2].room_id, "!joined-2:example.org");
        assert_eq!(snapshot.rooms[2].unread_notifications, 2);
        assert_eq!(snapshot.rooms[2].highlight_count, 1);
        assert_eq!(snapshot.rooms[3].room_id, "!joined-1:example.org");
    }

    #[test]
    fn selecting_room_marks_selected_state() {
        let mut state = DesktopState::new("@alice:example.org", 50);
        state.replace_rooms(vec![room(
            "!r1:example.org",
            "R1",
            0,
            0,
            RoomMembership::Joined,
        )]);
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
        state.replace_rooms(vec![room(
            "!r1:example.org",
            "R1",
            0,
            0,
            RoomMembership::Joined,
        )]);
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
    fn invite_selection_disables_sending_and_exposes_snapshot_flags() {
        let mut state = DesktopState::new("@alice:example.org", 50);
        state.replace_rooms(vec![room(
            "!invite:example.org",
            "Invite",
            0,
            0,
            RoomMembership::Invited,
        )]);
        state.select_room("!invite:example.org".to_owned());

        let snapshot = state.snapshot();
        assert!(!snapshot.can_send);
        assert!(snapshot.selected_room_is_invite);
        assert_eq!(snapshot.selected_room_invite_pending_text, None);
    }

    #[test]
    fn invite_ack_clears_pending_and_sets_error_on_failure() {
        let mut state = DesktopState::new("@alice:example.org", 50);
        state.replace_rooms(vec![room(
            "!invite:example.org",
            "Invite",
            0,
            0,
            RoomMembership::Invited,
        )]);
        assert!(state.mark_invite_action_requested(
            "!invite:example.org".to_owned(),
            "txn-invite-1".to_owned(),
            InviteAction::Accept,
        ));
        state.handle_backend_event(BackendEvent::InviteActionAck(InviteActionAck {
            client_txn_id: "txn-invite-1".to_owned(),
            room_id: "!invite:example.org".to_owned(),
            action: InviteAction::Accept,
            error_code: Some("not_invited".to_owned()),
        }));

        let snapshot = state.snapshot();
        assert_eq!(
            snapshot.error_text.as_deref(),
            Some("invite accept failed (not_invited)")
        );
        assert!(!snapshot.rooms[0].invite_pending);
        assert!(snapshot.rooms[0].invite_pending_text.is_empty());
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
        state.replace_rooms(vec![room(
            "!r1:example.org",
            "R1",
            0,
            0,
            RoomMembership::Joined,
        )]);
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
        state.replace_rooms(vec![room(
            "!r1:example.org",
            "R1",
            0,
            0,
            RoomMembership::Joined,
        )]);
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
        state.replace_rooms(vec![room(
            "!r1:example.org",
            "R1",
            0,
            0,
            RoomMembership::Joined,
        )]);
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

    #[test]
    fn recovery_status_event_opens_security_dialog() {
        let mut state = DesktopState::new("@alice:example.org", 50);
        state.show_security_status_dialog("Checking backup status...");
        state.handle_backend_event(BackendEvent::RecoveryStatus(RecoveryStatus {
            backup_state: KeyBackupState::Enabled,
            backup_enabled: true,
            backup_exists_on_server: true,
            recovery_state: RecoveryState::Enabled,
        }));

        let snapshot = state.snapshot();
        assert!(snapshot.show_security_dialog);
        assert_eq!(snapshot.security_dialog_title, "Identity Backup Status");
        assert!(
            snapshot
                .security_dialog_body
                .contains("Backup enabled locally: yes")
        );
        assert!(
            snapshot
                .security_dialog_body
                .contains("Recovery state: Enabled")
        );
    }

    #[test]
    fn recovery_enable_success_shows_recovery_key_dialog() {
        let mut state = DesktopState::new("@alice:example.org", 50);
        state.handle_backend_event(BackendEvent::RecoveryEnableAck(
            backend_core::RecoveryEnableAck {
                client_txn_id: "txn-r1".to_owned(),
                recovery_key: Some("word1 word2 word3".to_owned()),
                error_code: None,
            },
        ));

        let snapshot = state.snapshot();
        assert!(snapshot.show_security_dialog);
        assert_eq!(snapshot.security_dialog_title, "Identity Backup Created");
        assert!(snapshot.security_dialog_body.contains("Recovery key:"));
        assert!(snapshot.security_dialog_body.contains("word1 word2 word3"));
        assert!(snapshot.security_show_copy_button);
        assert_eq!(snapshot.security_copy_button_text, "Copy Key");
        assert_eq!(snapshot.error_text, None);
    }

    #[test]
    fn mark_recovery_key_copied_updates_button_label() {
        let mut state = DesktopState::new("@alice:example.org", 50);
        state.handle_backend_event(BackendEvent::RecoveryEnableAck(
            backend_core::RecoveryEnableAck {
                client_txn_id: "txn-r1".to_owned(),
                recovery_key: Some("word1 word2 word3".to_owned()),
                error_code: None,
            },
        ));

        assert_eq!(
            state.recovery_key_for_copy().as_deref(),
            Some("word1 word2 word3")
        );
        state.mark_recovery_key_copied();
        let snapshot = state.snapshot();
        assert_eq!(snapshot.security_copy_button_text, "Copied");
    }

    #[test]
    fn dismiss_security_dialog_hides_modal() {
        let mut state = DesktopState::new("@alice:example.org", 50);
        state.show_security_dialog("x", "y");
        state.dismiss_security_dialog();
        assert!(!state.snapshot().show_security_dialog);
        assert_eq!(state.recovery_key_for_copy(), None);
    }

    #[test]
    fn recovery_status_does_not_override_recovery_key_dialog() {
        let mut state = DesktopState::new("@alice:example.org", 50);
        state.handle_backend_event(BackendEvent::RecoveryEnableAck(
            backend_core::RecoveryEnableAck {
                client_txn_id: "txn-r1".to_owned(),
                recovery_key: Some("word1 word2 word3".to_owned()),
                error_code: None,
            },
        ));
        state.handle_backend_event(BackendEvent::RecoveryStatus(RecoveryStatus {
            backup_state: KeyBackupState::Enabled,
            backup_enabled: true,
            backup_exists_on_server: true,
            recovery_state: RecoveryState::Enabled,
        }));

        let snapshot = state.snapshot();
        assert_eq!(snapshot.security_dialog_title, "Identity Backup Created");
        assert!(snapshot.security_dialog_body.contains("word1 word2 word3"));
    }

    #[test]
    fn recovery_enable_already_enabled_shows_status_dialog() {
        let mut state = DesktopState::new("@alice:example.org", 50);
        state.handle_backend_event(BackendEvent::RecoveryEnableAck(
            backend_core::RecoveryEnableAck {
                client_txn_id: "txn-r1".to_owned(),
                recovery_key: None,
                error_code: Some("recovery_already_enabled".to_owned()),
            },
        ));

        let snapshot = state.snapshot();
        assert!(snapshot.show_security_dialog);
        assert_eq!(snapshot.security_dialog_title, "Identity Backup Status");
        assert!(
            snapshot
                .security_dialog_body
                .contains("already enabled on this client")
        );
        assert_eq!(snapshot.error_text, None);
    }

    #[test]
    fn recovery_enable_backup_exists_shows_status_dialog() {
        let mut state = DesktopState::new("@alice:example.org", 50);
        state.handle_backend_event(BackendEvent::RecoveryEnableAck(
            backend_core::RecoveryEnableAck {
                client_txn_id: "txn-r1".to_owned(),
                recovery_key: None,
                error_code: Some("recovery_backup_exists".to_owned()),
            },
        ));

        let snapshot = state.snapshot();
        assert!(snapshot.show_security_dialog);
        assert_eq!(snapshot.security_dialog_title, "Identity Backup Status");
        assert!(snapshot.security_dialog_body.contains("already exists"));
        assert_eq!(snapshot.error_text, None);
    }

    #[test]
    fn restore_prompt_exposes_input_controls() {
        let mut state = DesktopState::new("@alice:example.org", 50);
        state.show_security_restore_prompt_dialog(
            SECURITY_TITLE_RESTORE,
            "Enter your recovery key or passphrase.",
        );

        let snapshot = state.snapshot();
        assert!(snapshot.show_security_dialog);
        assert_eq!(snapshot.security_dialog_title, "Identity Restore");
        assert!(snapshot.security_show_restore_input);
        assert_eq!(snapshot.security_restore_button_text, "Restore");
        assert!(!snapshot.security_restore_in_flight);
        assert_eq!(snapshot.error_text, None);
    }

    #[test]
    fn begin_restore_request_is_idempotent_until_dialog_resets() {
        let mut state = DesktopState::new("@alice:example.org", 50);
        state.show_security_restore_prompt_dialog(
            SECURITY_TITLE_RESTORE,
            "Enter your recovery key or passphrase.",
        );

        assert!(state.begin_restore_request());
        assert!(!state.begin_restore_request());

        let snapshot = state.snapshot();
        assert!(snapshot.security_restore_in_flight);
        assert_eq!(snapshot.security_restore_button_text, "Restoring...");
    }

    #[test]
    fn recovery_restore_ack_success_closes_input_mode_and_shows_success_dialog() {
        let mut state = DesktopState::new("@alice:example.org", 50);
        state.show_security_restore_prompt_dialog(
            SECURITY_TITLE_RESTORE,
            "Enter your recovery key or passphrase.",
        );
        assert!(state.begin_restore_request());
        state.handle_backend_event(BackendEvent::RecoveryRestoreAck(
            backend_core::RecoveryRestoreAck {
                client_txn_id: "txn-restore-1".to_owned(),
                error_code: None,
            },
        ));

        let snapshot = state.snapshot();
        assert!(snapshot.show_security_dialog);
        assert_eq!(snapshot.security_dialog_title, "Identity Restore");
        assert!(
            snapshot
                .security_dialog_body
                .contains("restored successfully")
        );
        assert!(!snapshot.security_show_restore_input);
        assert_eq!(snapshot.security_restore_button_text, "Restore");
        assert!(!snapshot.security_restore_in_flight);
        assert_eq!(snapshot.error_text, None);
    }

    #[test]
    fn recovery_restore_ack_failure_shows_dialog_with_error_code_and_resets_input_state() {
        let mut state = DesktopState::new("@alice:example.org", 50);
        state.show_security_restore_prompt_dialog(
            SECURITY_TITLE_RESTORE,
            "Enter your recovery key or passphrase.",
        );
        assert!(state.begin_restore_request());
        state.handle_backend_event(BackendEvent::RecoveryRestoreAck(
            backend_core::RecoveryRestoreAck {
                client_txn_id: "txn-restore-2".to_owned(),
                error_code: Some("bad_recovery_key".to_owned()),
            },
        ));

        let snapshot = state.snapshot();
        assert!(snapshot.show_security_dialog);
        assert_eq!(snapshot.security_dialog_title, "Identity Restore");
        assert!(
            snapshot
                .security_dialog_body
                .contains("Verify your recovery key or passphrase")
        );
        assert!(
            snapshot
                .security_dialog_body
                .contains("Error code: bad_recovery_key")
        );
        assert!(!snapshot.security_show_restore_input);
        assert_eq!(snapshot.security_restore_button_text, "Restore");
        assert!(!snapshot.security_restore_in_flight);
        assert_eq!(snapshot.error_text, None);
    }
}
