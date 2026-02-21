//! Frontend-facing state reducer for `pikachat-desktop`.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use backend_core::types::{InviteAction, InviteActionAck, RoomMembership};
use backend_core::{
    BackendEvent, BackendLifecycleState, KeyBackupState, MediaSourceRef, RecoveryState,
    RecoveryStatus, RoomSummary, SendAck, SyncStatus, TimelineContent, TimelineItem, TimelineOp,
};
use tracing::{debug, trace, warn};

const DEFAULT_STATUS: &str = "Idle";
const SIDEBAR_MIN_WIDTH_PX: f32 = 220.0;
const SIDEBAR_MAX_FRACTION: f32 = 0.45;
const SECURITY_TITLE_STATUS: &str = "Identity Backup Status";
const SECURITY_TITLE_CREATING: &str = "Creating Identity Backup";
const SECURITY_TITLE_CREATED: &str = "Identity Backup Created";
const SECURITY_TITLE_RESTORE: &str = "Identity Restore";
const MAX_PENDING_ATTACHMENT_BYTES: u64 = 20 * 1024 * 1024;

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
    pub local_id: Option<String>,
    pub sender: String,
    pub body: String,
    pub event_kind: String,
    pub caption: String,
    pub media_status: String,
    pub media_source: Option<String>,
    pub media_cached_path: Option<String>,
    pub media_key: String,
    pub media_is_gif: bool,
    pub media_hint: Option<String>,
    pub can_retry: bool,
    pub can_remove: bool,
    pub is_own: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposerAttachmentView {
    pub file_name: String,
    pub local_path: String,
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
    pub show_login_screen: bool,
    pub login_homeserver: String,
    pub login_user_id: String,
    pub login_password: String,
    pub login_remember_password: bool,
    pub login_in_flight: bool,
    pub show_logout_confirm: bool,
    pub show_security_dialog: bool,
    pub security_dialog_title: String,
    pub security_dialog_body: String,
    pub security_show_copy_button: bool,
    pub security_copy_button_text: String,
    pub security_show_restore_input: bool,
    pub security_restore_button_text: String,
    pub security_restore_in_flight: bool,
    pub composer_attachment: Option<ComposerAttachmentView>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaStatus {
    Missing,
    Downloading,
    Ready,
    Failed,
}

impl MediaStatus {
    fn as_ui_label(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::Downloading => "downloading",
            Self::Ready => "ready",
            Self::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingOutgoingMediaStatus {
    Uploading,
    Sending,
    UploadFailed,
    SendFailed,
}

impl PendingOutgoingMediaStatus {
    fn as_ui_label(self) -> &'static str {
        match self {
            Self::Uploading => "uploading",
            Self::Sending => "sending",
            Self::UploadFailed => "upload_failed",
            Self::SendFailed => "send_failed",
        }
    }

    fn can_retry(self) -> bool {
        matches!(self, Self::UploadFailed | Self::SendFailed)
    }
}

#[derive(Debug, Clone)]
struct MediaState {
    status: MediaStatus,
    cached_path: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PendingAttachment {
    pub file_name: String,
    pub local_path: String,
    pub content_type: String,
    pub size_bytes: u64,
}

#[derive(Debug, Clone)]
pub struct PendingOutgoingMedia {
    pub room_id: String,
    pub local_id: String,
    pub sender: String,
    pub caption: String,
    pub local_path: String,
    pub content_type: Option<String>,
    pub media_source: Option<String>,
    pub status: PendingOutgoingMediaStatus,
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
    media: HashMap<String, MediaState>,
    pending_attachment: Option<PendingAttachment>,
    pending_outgoing_media: HashMap<String, PendingOutgoingMedia>,
    pagination: PaginationTracker,
    show_login_screen: bool,
    login_homeserver: String,
    login_user_id: String,
    login_password: String,
    login_remember_password: bool,
    login_in_flight: bool,
    show_logout_confirm: bool,
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
            media: HashMap::new(),
            pending_attachment: None,
            pending_outgoing_media: HashMap::new(),
            pagination: PaginationTracker::default(),
            show_login_screen: false,
            login_homeserver: String::new(),
            login_user_id: String::new(),
            login_password: String::new(),
            login_remember_password: false,
            login_in_flight: false,
            show_logout_confirm: false,
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
            show_login_screen: self.show_login_screen,
            login_homeserver: self.login_homeserver.clone(),
            login_user_id: self.login_user_id.clone(),
            login_password: self.login_password.clone(),
            login_remember_password: self.login_remember_password,
            login_in_flight: self.login_in_flight,
            show_logout_confirm: self.show_logout_confirm,
            show_security_dialog: self.show_security_dialog,
            security_dialog_title: self.security_dialog_title.clone(),
            security_dialog_body: self.security_dialog_body.clone(),
            security_show_copy_button: self.security_show_copy_button,
            security_copy_button_text: self.security_copy_button_text.clone(),
            security_show_restore_input: self.security_show_restore_input,
            security_restore_button_text: self.security_restore_button_text.clone(),
            security_restore_in_flight: self.security_restore_in_flight,
            composer_attachment: self.pending_attachment.as_ref().map(|attachment| {
                ComposerAttachmentView {
                    file_name: attachment.file_name.clone(),
                    local_path: attachment.local_path.clone(),
                }
            }),
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

    /// Set login-form values shown by the UI.
    pub fn set_login_form(
        &mut self,
        homeserver: impl Into<String>,
        user_id: impl Into<String>,
        password: impl Into<String>,
        remember_password: bool,
    ) {
        self.login_homeserver = homeserver.into();
        self.login_user_id = user_id.into();
        self.login_password = password.into();
        self.login_remember_password = remember_password;
    }

    /// Show login pane and clear in-flight login state.
    pub fn show_login_screen(&mut self) {
        self.show_login_screen = true;
        self.login_in_flight = false;
        self.show_logout_confirm = false;
    }

    /// Hide login pane after successful authentication.
    pub fn hide_login_screen(&mut self) {
        self.show_login_screen = false;
        self.login_in_flight = false;
        self.show_logout_confirm = false;
    }

    /// Mark restore-session flow as in-flight.
    pub fn begin_restore_attempt(&mut self) {
        self.show_login_screen = false;
        self.login_in_flight = true;
        self.show_logout_confirm = false;
        self.status_text = "Restoring session...".to_owned();
    }

    /// Mark manual login as in-flight and update login form values.
    pub fn begin_manual_login(
        &mut self,
        homeserver: impl Into<String>,
        user_id: impl Into<String>,
        password: impl Into<String>,
        remember_password: bool,
    ) {
        let user_id = user_id.into();
        self.own_user_id = user_id.clone();
        self.set_login_form(homeserver, user_id, password, remember_password);
        self.show_login_screen = true;
        self.login_in_flight = true;
        self.show_logout_confirm = false;
        self.status_text = "Authenticating...".to_owned();
    }

    /// Show/hide logout confirmation dialog.
    pub fn set_logout_confirm_visible(&mut self, visible: bool) {
        self.show_logout_confirm = visible;
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

    pub fn set_pending_attachment(&mut self, attachment: PendingAttachment) {
        self.pending_attachment = Some(attachment);
    }

    pub fn clear_pending_attachment(&mut self) {
        self.pending_attachment = None;
    }

    pub fn take_pending_attachment(&mut self) -> Option<PendingAttachment> {
        self.pending_attachment.take()
    }

    pub fn validate_attachment(
        local_path: String,
        file_name: String,
        content_type: String,
        size_bytes: u64,
    ) -> Result<PendingAttachment, String> {
        if size_bytes > MAX_PENDING_ATTACHMENT_BYTES {
            return Err("attachment is too large (max 20MB)".to_owned());
        }
        let ext = file_name
            .rsplit('.')
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        let is_allowed = matches!(ext.as_str(), "jpg" | "jpeg" | "png" | "gif");
        if !is_allowed {
            return Err("unsupported attachment type (allowed: jpg, png, gif)".to_owned());
        }
        Ok(PendingAttachment {
            file_name,
            local_path,
            content_type,
            size_bytes,
        })
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
        let was_pending = self.pending_sends.remove(&ack.client_txn_id);
        if !was_pending {
            return;
        }
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

    pub fn selected_room_uncached_image_sources(&self) -> Vec<MediaSourceRef> {
        let Some(room_id) = &self.selected_room_id else {
            return Vec::new();
        };
        let Some(items) = self.timelines.get(room_id) else {
            return Vec::new();
        };

        let mut unique = HashSet::new();
        items
            .iter()
            .filter_map(image_source_of_item)
            .filter(|source| !self.media.contains_key(&media_source_key(source)))
            .filter(|source| unique.insert(media_source_key(source)))
            .collect()
    }

    pub fn media_source_ref_for_key(&self, source_key: &str) -> Option<MediaSourceRef> {
        self.timelines
            .values()
            .flat_map(|items| items.iter())
            .find_map(|item| {
                let source = image_source_of_item(item)?;
                if media_source_key(&source) == source_key {
                    Some(source)
                } else {
                    None
                }
            })
    }

    pub fn mark_media_download_started(&mut self, source: String) {
        self.media
            .entry(source)
            .and_modify(|state| state.status = MediaStatus::Downloading)
            .or_insert(MediaState {
                status: MediaStatus::Downloading,
                cached_path: None,
            });
        self.rebuild_visible_messages();
    }

    pub fn mark_media_download_ready(&mut self, source: String, cached_path: String) {
        self.media.insert(
            source,
            MediaState {
                status: MediaStatus::Ready,
                cached_path: Some(cached_path),
            },
        );
        self.rebuild_visible_messages();
    }

    pub fn mark_media_download_failed(&mut self, source: String) {
        self.media
            .entry(source)
            .and_modify(|state| state.status = MediaStatus::Failed)
            .or_insert(MediaState {
                status: MediaStatus::Failed,
                cached_path: None,
            });
        self.rebuild_visible_messages();
    }

    pub fn upsert_pending_outgoing_media(
        &mut self,
        local_id: String,
        room_id: String,
        caption: String,
        local_path: String,
        status: PendingOutgoingMediaStatus,
    ) {
        self.pending_outgoing_media.insert(
            local_id.clone(),
            PendingOutgoingMedia {
                room_id,
                local_id,
                sender: self.own_user_id.clone(),
                caption,
                content_type: infer_media_content_type_from_local_path(&local_path),
                local_path,
                media_source: None,
                status,
            },
        );
        self.rebuild_visible_messages();
    }

    pub fn pending_outgoing_media(&self, local_id: &str) -> Option<PendingOutgoingMedia> {
        self.pending_outgoing_media.get(local_id).cloned()
    }

    pub fn set_pending_outgoing_upload_failed(&mut self, local_id: &str) {
        if let Some(item) = self.pending_outgoing_media.get_mut(local_id) {
            item.status = PendingOutgoingMediaStatus::UploadFailed;
            self.rebuild_visible_messages();
        }
    }

    pub fn set_pending_outgoing_sending(&mut self, local_id: &str, source: String) {
        if let Some(item) = self.pending_outgoing_media.get_mut(local_id) {
            item.status = PendingOutgoingMediaStatus::Sending;
            item.media_source = Some(source);
            self.rebuild_visible_messages();
        }
    }

    pub fn set_pending_outgoing_send_failed(&mut self, local_id: &str) {
        if let Some(item) = self.pending_outgoing_media.get_mut(local_id) {
            item.status = PendingOutgoingMediaStatus::SendFailed;
            self.rebuild_visible_messages();
        }
    }

    pub fn remove_pending_outgoing_media(&mut self, local_id: &str) {
        self.pending_outgoing_media.remove(local_id);
        self.rebuild_visible_messages();
    }
    /// Feed one backend event into the reducer.
    pub fn handle_backend_event(&mut self, event: BackendEvent) {
        match event {
            BackendEvent::StateChanged { state } => {
                self.status_text = lifecycle_label(state).to_owned();
                if state == BackendLifecycleState::LoggedOut {
                    self.clear_authenticated_data();
                    self.show_login_screen();
                }
            }
            BackendEvent::AuthResult {
                success,
                error_code,
            } => {
                if success {
                    self.status_text = "Authenticated".to_owned();
                    self.hide_login_screen();
                    self.clear_error();
                } else {
                    let code = error_code.unwrap_or_else(|| "unknown".to_owned());
                    self.status_text = "Authentication failed".to_owned();
                    self.error_text = Some(auth_error_text(&code));
                    self.show_login_screen();
                }
                self.login_in_flight = false;
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
                self.login_in_flight = false;
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

    fn clear_authenticated_data(&mut self) {
        self.rooms.clear();
        self.selected_room_id = None;
        self.timelines.clear();
        self.messages.clear();
        self.pending_sends.clear();
        self.pending_invite_actions.clear();
        self.pending_invite_actions_by_txn.clear();
        self.pagination.clear_in_flight();
        self.dismiss_security_dialog();
    }

    fn rebuild_visible_messages(&mut self) {
        let Some(room_id) = &self.selected_room_id else {
            self.messages.clear();
            return;
        };

        let mut messages = self
            .timelines
            .get(room_id)
            .map(|items| {
                items
                    .iter()
                    .map(|item| {
                        let source = image_source_of_item(item);
                        if let Some(source_ref) = source {
                            let source = media_source_key(&source_ref);
                            let media = self.media.get(&source);
                            let media_status = media
                                .map(|state| state.status)
                                .unwrap_or(MediaStatus::Missing)
                                .as_ui_label()
                                .to_owned();
                            let media_key = make_stable_media_key(
                                item.event_id.as_deref(),
                                None,
                                Some(&source),
                            );
                            MessageView {
                                event_id: item.event_id.clone(),
                                local_id: None,
                                sender: item.sender.clone(),
                                body: item.body.clone(),
                                event_kind: "image".to_owned(),
                                caption: item.body.clone(),
                                media_status,
                                media_source: Some(source),
                                media_cached_path: media
                                    .and_then(|state| state.cached_path.clone()),
                                media_key,
                                media_is_gif: timeline_item_is_gif(item),
                                media_hint: None,
                                can_retry: media.map(|state| state.status)
                                    == Some(MediaStatus::Failed),
                                can_remove: false,
                                is_own: item.sender == self.own_user_id,
                            }
                        } else {
                            MessageView {
                                event_id: item.event_id.clone(),
                                local_id: None,
                                sender: item.sender.clone(),
                                body: item.body.clone(),
                                event_kind: "text".to_owned(),
                                caption: String::new(),
                                media_status: "none".to_owned(),
                                media_source: None,
                                media_cached_path: None,
                                media_key: String::new(),
                                media_is_gif: false,
                                media_hint: None,
                                can_retry: false,
                                can_remove: false,
                                is_own: item.sender == self.own_user_id,
                            }
                        }
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        let mut pending = self
            .pending_outgoing_media
            .values()
            .filter(|item| &item.room_id == room_id)
            .cloned()
            .collect::<Vec<_>>();
        pending.sort_by(|a, b| a.local_id.cmp(&b.local_id));

        messages.extend(pending.into_iter().map(|item| MessageView {
            event_id: None,
            local_id: Some(item.local_id.clone()),
            sender: item.sender,
            body: item.caption.clone(),
            event_kind: "image".to_owned(),
            caption: item.caption,
            media_status: item.status.as_ui_label().to_owned(),
            media_source: item.media_source.clone(),
            media_cached_path: Some(item.local_path.clone()),
            media_key: make_stable_media_key(
                None,
                Some(item.local_id.as_str()),
                item.media_source.as_deref(),
            ),
            media_is_gif: pending_outgoing_media_is_gif(
                &item.local_path,
                item.content_type.as_deref(),
            ),
            media_hint: None,
            can_retry: item.status.can_retry(),
            can_remove: true,
            is_own: true,
        }));
        self.messages = messages;
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
            TimelineOp::Replace { event_id, item } => {
                if let Some(existing) = items
                    .iter_mut()
                    .find(|it| it.event_id.as_deref() == Some(event_id.as_str()))
                {
                    *existing = item.clone();
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

fn image_source_of_item(item: &TimelineItem) -> Option<MediaSourceRef> {
    match &item.content {
        TimelineContent::Image(image) => Some(image.source.clone()),
        _ => None,
    }
}

fn media_source_key(source: &MediaSourceRef) -> String {
    source.to_string()
}

fn make_stable_media_key(
    event_id: Option<&str>,
    local_id: Option<&str>,
    media_source: Option<&str>,
) -> String {
    if let Some(event_id) = event_id.filter(|value| !value.is_empty()) {
        return format!("evt:{event_id}");
    }
    if let Some(local_id) = local_id.filter(|value| !value.is_empty()) {
        return format!("local:{local_id}");
    }
    if let Some(media_source) = media_source.filter(|value| !value.is_empty()) {
        return format!("src:{media_source}");
    }
    String::new()
}

fn timeline_item_is_gif(item: &TimelineItem) -> bool {
    match &item.content {
        TimelineContent::Image(image) => content_type_is_gif(
            image
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.content_type.as_deref()),
        ),
        _ => false,
    }
}

fn pending_outgoing_media_is_gif(local_path: &str, content_type: Option<&str>) -> bool {
    local_path_is_gif(local_path) || content_type_is_gif(content_type)
}

fn local_path_is_gif(path: &str) -> bool {
    Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| extension.eq_ignore_ascii_case("gif"))
        .unwrap_or(false)
}

fn content_type_is_gif(content_type: Option<&str>) -> bool {
    content_type
        .map(str::trim)
        .and_then(|value| value.split(';').next())
        .map(str::trim)
        .map(|value| value.eq_ignore_ascii_case("image/gif"))
        .unwrap_or(false)
}

fn infer_media_content_type_from_local_path(path: &str) -> Option<String> {
    let extension = Path::new(path)
        .extension()
        .and_then(|value| value.to_str())?
        .to_ascii_lowercase();
    match extension.as_str() {
        "gif" => Some("image/gif".to_owned()),
        "jpg" | "jpeg" => Some("image/jpeg".to_owned()),
        "png" => Some("image/png".to_owned()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(event_id: &str, sender: &str, body: &str) -> TimelineItem {
        TimelineItem {
            event_id: Some(event_id.to_owned()),
            sender: sender.to_owned(),
            body: body.to_owned(),
            content: TimelineContent::Text,
            timestamp_ms: 1_700_000_000,
        }
    }

    fn image_item(
        event_id: Option<&str>,
        sender: &str,
        body: &str,
        source: &str,
        content_type: Option<&str>,
    ) -> TimelineItem {
        TimelineItem {
            event_id: event_id.map(ToOwned::to_owned),
            sender: sender.to_owned(),
            body: body.to_owned(),
            content: TimelineContent::Image(backend_core::TimelineImageContent {
                source: MediaSourceRef::PlainMxc {
                    uri: source.to_owned(),
                },
                metadata: Some(backend_core::TimelineImageMetadata {
                    content_type: content_type.map(ToOwned::to_owned),
                    ..Default::default()
                }),
            }),
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
    fn media_key_generation_prefers_event_then_local_then_source() {
        assert_eq!(
            make_stable_media_key(Some("$evt"), Some("local-1"), Some("mxc://example/source")),
            "evt:$evt"
        );
        assert_eq!(
            make_stable_media_key(None, Some("local-1"), Some("mxc://example/source")),
            "local:local-1"
        );
        assert_eq!(
            make_stable_media_key(None, None, Some("mxc://example/source")),
            "src:mxc://example/source"
        );
    }

    #[test]
    fn timeline_image_uses_metadata_content_type_for_gif_detection() {
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
                image_item(
                    Some("$gif"),
                    "@bob:example.org",
                    "gif image",
                    "mxc://example.org/gif",
                    Some("IMAGE/GIF"),
                ),
                image_item(
                    Some("$png"),
                    "@bob:example.org",
                    "png image",
                    "mxc://example.org/png",
                    Some("image/png"),
                ),
            ],
        });

        let snapshot = state.snapshot();
        assert_eq!(snapshot.messages.len(), 2);
        assert_eq!(snapshot.messages[0].media_key, "evt:$gif");
        assert!(snapshot.messages[0].media_is_gif);
        assert!(!snapshot.messages[1].media_is_gif);
    }

    #[test]
    fn pending_outgoing_gif_detects_local_gif_extension() {
        let mut state = DesktopState::new("@alice:example.org", 50);
        state.replace_rooms(vec![room(
            "!r1:example.org",
            "R1",
            0,
            0,
            RoomMembership::Joined,
        )]);
        state.select_room("!r1:example.org".to_owned());
        state.upsert_pending_outgoing_media(
            "local-1".to_owned(),
            "!r1:example.org".to_owned(),
            "cat gif".to_owned(),
            "/tmp/cat.GIF".to_owned(),
            PendingOutgoingMediaStatus::Uploading,
        );

        let snapshot = state.snapshot();
        assert_eq!(snapshot.messages.len(), 1);
        assert_eq!(snapshot.messages[0].media_key, "local:local-1");
        assert!(snapshot.messages[0].media_is_gif);
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
