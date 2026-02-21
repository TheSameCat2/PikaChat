mod auth_profile;
mod bridge;
mod config;
mod gif_player;
mod logging;
mod media_cache;
mod state;

use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    path::Path,
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};

use backend_core::types::RoomMembership;
use backend_matrix::{MatrixFrontendAdapter, spawn_runtime};
use bridge::{DesktopBridge, UiUpdateCallback};
use config::DesktopConfig;
use gif_player::{GifPlaybackController, MIN_FRAME_DELAY_MS, VISIBLE_PING_INTERVAL_MS};
use slint::Model;
use state::{DesktopSnapshot, clamp_sidebar_width};
use tracing::{debug, error, info, trace, warn};

slint::include_modules!();

thread_local! {
    static UI_RUNTIME_STATE: RefCell<Option<UiRuntimeState>> = const { RefCell::new(None) };
}

struct UiRuntimeState {
    started_at: Instant,
    messages_model: Rc<slint::VecModel<MessageRow>>,
    media_row_index_by_key: HashMap<String, usize>,
    gif_controller: GifPlaybackController,
    attached_gif_sources: HashMap<String, String>,
    fullscreen_media_key: Option<String>,
}

impl UiRuntimeState {
    fn new(messages_model: Rc<slint::VecModel<MessageRow>>) -> Self {
        Self {
            started_at: Instant::now(),
            messages_model,
            media_row_index_by_key: HashMap::new(),
            gif_controller: GifPlaybackController::new(),
            attached_gif_sources: HashMap::new(),
            fullscreen_media_key: None,
        }
    }

    fn now_ms(&self) -> u64 {
        let elapsed_ms = self.started_at.elapsed().as_millis();
        elapsed_ms.min(u128::from(u64::MAX)) as u64
    }
}

fn main() -> Result<(), slint::PlatformError> {
    logging::init();
    info!("starting pikachat-desktop");

    let ui = MainWindow::new()?;
    initialize_ui_runtime(&ui);

    let gif_tick_timer = slint::Timer::default();
    let scheduler_tick_ms = MIN_FRAME_DELAY_MS.min(VISIBLE_PING_INTERVAL_MS);
    {
        let weak = ui.as_weak();
        gif_tick_timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(scheduler_tick_ms),
            move || {
                if let Some(ui) = weak.upgrade() {
                    run_gif_scheduler_tick(&ui);
                }
            },
        );
    }

    // Menu callbacks are always available, regardless of backend startup outcome.
    let weak = ui.as_weak();
    ui.on_quit_requested(move || {
        info!("quit requested from menu");
        if let Some(ui) = weak.upgrade() {
            let _ = ui.hide();
        }
        let _ = slint::quit_event_loop();
    });

    let weak = ui.as_weak();
    ui.on_about_slint_requested(move || {
        debug!("about slint requested");
        if let Some(ui) = weak.upgrade() {
            ui.set_show_about_screen(true);
        }
    });

    ui.set_sidebar_width_px(280.0);

    let weak = ui.as_weak();
    ui.on_sidebar_width_requested(move |requested_width_px, window_width_px| {
        if let Some(ui) = weak.upgrade() {
            let clamped = clamp_sidebar_width(window_width_px, requested_width_px);
            trace!(
                requested_width_px,
                window_width_px, clamped, "sidebar resize requested"
            );
            ui.set_sidebar_width_px(clamped);
        }
    });

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("pikachat-desktop")
        .build()
        .map_err(|err| slint::PlatformError::from(err.to_string()))?;

    let desktop_config = DesktopConfig::from_env();
    let mut bridge: Option<Arc<DesktopBridge>> = None;

    match desktop_config {
        Ok(config) => {
            info!(
                timeline_max_items = config.timeline_max_items,
                paginate_limit = config.paginate_limit,
                "desktop config loaded"
            );
            let adapter = {
                let _enter_guard = runtime.enter();
                Arc::new(MatrixFrontendAdapter::with_config(
                    spawn_runtime(),
                    512,
                    config.timeline_max_items,
                ))
            };

            let weak = ui.as_weak();
            let ui_update: UiUpdateCallback = Arc::new(move |snapshot: DesktopSnapshot| {
                let weak = weak.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = weak.upgrade() {
                        apply_snapshot_to_ui(&ui, snapshot);
                    }
                });
            });

            let spawned_bridge =
                DesktopBridge::spawn(config, adapter, runtime.handle().clone(), ui_update);

            {
                let bridge = Arc::clone(&spawned_bridge);
                ui.on_login_requested(move |homeserver, user_id, password, remember_password| {
                    bridge.submit_login(
                        homeserver.to_string(),
                        user_id.to_string(),
                        password.to_string(),
                        remember_password,
                    );
                });
            }
            {
                let bridge = Arc::clone(&spawned_bridge);
                ui.on_logout_requested(move || {
                    bridge.request_logout_confirmation();
                });
            }
            {
                let bridge = Arc::clone(&spawned_bridge);
                ui.on_logout_confirmed(move || {
                    bridge.confirm_logout();
                });
            }
            {
                let bridge = Arc::clone(&spawned_bridge);
                ui.on_logout_cancelled(move || {
                    bridge.cancel_logout_confirmation();
                });
            }
            {
                let bridge = Arc::clone(&spawned_bridge);
                ui.on_room_selected(move |index| {
                    debug!(index, "room row selected from ui");
                    bridge.select_room_by_index(index);
                });
            }
            {
                let bridge = Arc::clone(&spawned_bridge);
                ui.on_room_invite_accept_requested(move |room_id| {
                    bridge.accept_room_invite(room_id.to_string());
                });
            }
            {
                let bridge = Arc::clone(&spawned_bridge);
                ui.on_room_invite_reject_requested(move |room_id| {
                    bridge.reject_room_invite(room_id.to_string());
                });
            }
            {
                let bridge = Arc::clone(&spawned_bridge);
                ui.on_send_message_requested(move |text| {
                    let body = text.to_string();
                    debug!(body_len = body.len(), "send requested from ui");
                    let queued = bridge.send_message(body);
                    debug!(queued, "send request outcome");
                    queued
                });
            }
            {
                let bridge = Arc::clone(&spawned_bridge);
                ui.on_attachment_pick_requested(move || {
                    bridge.pick_attachment();
                });
            }
            {
                let bridge = Arc::clone(&spawned_bridge);
                ui.on_attachment_clear_requested(move || {
                    bridge.clear_attachment();
                });
            }
            {
                let bridge = Arc::clone(&spawned_bridge);
                ui.on_media_retry_requested(move |source| {
                    bridge.retry_media_download(source.to_string());
                });
            }
            {
                let bridge = Arc::clone(&spawned_bridge);
                ui.on_outgoing_media_retry_requested(move |local_id| {
                    bridge.retry_outgoing_media(local_id.to_string());
                });
            }
            {
                let bridge = Arc::clone(&spawned_bridge);
                ui.on_outgoing_media_remove_requested(move |local_id| {
                    bridge.remove_outgoing_media(local_id.to_string());
                });
            }
            ui.on_gif_visible_ping(move |media_key| {
                register_gif_visibility_ping(media_key.as_str());
            });
            {
                let weak = ui.as_weak();
                ui.on_media_screen_open_requested(move |media_key| {
                    if let Some(ui) = weak.upgrade() {
                        open_media_screen(&ui, media_key.as_str());
                    }
                });
            }
            {
                let weak = ui.as_weak();
                ui.on_media_screen_close_requested(move || {
                    if let Some(ui) = weak.upgrade() {
                        close_media_screen(&ui);
                    }
                });
            }
            {
                let bridge = Arc::clone(&spawned_bridge);
                ui.on_timeline_scrolled(move |viewport_y| {
                    trace!(viewport_y, "timeline scrolled");
                    bridge.on_timeline_scrolled(viewport_y);
                });
            }
            {
                let bridge = Arc::clone(&spawned_bridge);
                ui.on_security_status_requested(move || {
                    debug!("security status requested from ui");
                    bridge.request_recovery_status();
                });
            }
            {
                let bridge = Arc::clone(&spawned_bridge);
                ui.on_backup_identity_requested(move || {
                    debug!("identity backup requested from ui");
                    bridge.backup_identity();
                });
            }
            {
                let bridge = Arc::clone(&spawned_bridge);
                ui.on_reset_identity_requested(move || {
                    debug!("identity backup reset requested from ui");
                    bridge.reset_identity_backup();
                });
            }
            {
                let bridge = Arc::clone(&spawned_bridge);
                ui.on_restore_identity_requested(move || {
                    debug!("identity restore requested from ui");
                    bridge.prompt_restore_identity();
                });
            }
            {
                let bridge = Arc::clone(&spawned_bridge);
                ui.on_security_dialog_dismissed(move || {
                    bridge.dismiss_security_dialog();
                });
            }
            {
                let bridge = Arc::clone(&spawned_bridge);
                ui.on_security_copy_requested(move || {
                    bridge.copy_recovery_key();
                });
            }
            {
                let bridge = Arc::clone(&spawned_bridge);
                ui.on_security_restore_requested(move |recovery_key| {
                    bridge.restore_identity(recovery_key.to_string());
                });
            }

            bridge = Some(spawned_bridge);
        }
        Err(err) => {
            error!(error = %err, "desktop config invalid");
            ui.set_status_text("Configuration error".into());
            ui.set_error_text(err.to_string().into());
            ui.set_has_error(true);
            ui.set_can_send(false);

            ui.on_login_requested(|_, _, _, _| {});
            ui.on_logout_requested(|| {});
            ui.on_logout_confirmed(|| {});
            ui.on_logout_cancelled(|| {});
            ui.on_room_selected(|_| {});
            ui.on_room_invite_accept_requested(|_| {});
            ui.on_room_invite_reject_requested(|_| {});
            ui.on_send_message_requested(|_| false);
            ui.on_attachment_pick_requested(|| {});
            ui.on_attachment_clear_requested(|| {});
            ui.on_media_retry_requested(|_| {});
            ui.on_outgoing_media_retry_requested(|_| {});
            ui.on_outgoing_media_remove_requested(|_| {});
            ui.on_gif_visible_ping(|_| {});
            ui.on_media_screen_open_requested(|_| {});
            ui.on_media_screen_close_requested(|| {});
            ui.on_timeline_scrolled(|_| {});
            ui.on_security_status_requested(|| {});
            ui.on_backup_identity_requested(|| {});
            ui.on_reset_identity_requested(|| {});
            ui.on_restore_identity_requested(|| {});
            ui.on_security_dialog_dismissed(|| {});
            ui.on_security_copy_requested(|| {});
            ui.on_security_restore_requested(|_| {});
        }
    }

    let run_result = ui.run();
    info!("ui event loop exited");
    drop(gif_tick_timer);
    drop(bridge);
    drop(runtime);
    run_result
}

fn initialize_ui_runtime(ui: &MainWindow) {
    let messages_model = Rc::new(slint::VecModel::from(Vec::<MessageRow>::new()));
    ui.set_messages(messages_model.clone().into());
    UI_RUNTIME_STATE.with(|slot| {
        *slot.borrow_mut() = Some(UiRuntimeState::new(messages_model));
    });
}

fn with_ui_runtime_mut<R>(f: impl FnOnce(&mut UiRuntimeState) -> R) -> Option<R> {
    UI_RUNTIME_STATE.with(|slot| {
        let mut borrow = slot.borrow_mut();
        let runtime = borrow.as_mut()?;
        Some(f(runtime))
    })
}

fn register_gif_visibility_ping(media_key: &str) {
    let media_key = media_key.trim();
    if media_key.is_empty() {
        return;
    }
    let _ = with_ui_runtime_mut(|runtime| {
        let now_ms = runtime.now_ms();
        runtime
            .gif_controller
            .register_visibility_ping(media_key.to_owned(), now_ms);
    });
}

fn open_media_screen(ui: &MainWindow, media_key: &str) {
    let media_key = media_key.trim();
    if media_key.is_empty() {
        return;
    }

    let screen_image = with_ui_runtime_mut(|runtime| {
        let row_index = runtime.media_row_index_by_key.get(media_key).copied()?;
        let row = runtime.messages_model.row_data(row_index)?;
        let mut image = row.media_image.clone();
        if row.media_is_gif {
            let now_ms = runtime.now_ms();
            runtime
                .gif_controller
                .register_visibility_ping(media_key.to_owned(), now_ms);
            if let Some(frame) = runtime.gif_controller.current_frame_for(media_key) {
                image = frame;
            }
            runtime.fullscreen_media_key = Some(media_key.to_owned());
        } else {
            runtime.fullscreen_media_key = None;
        }
        Some(image)
    })
    .flatten();

    if let Some(image) = screen_image {
        ui.set_media_screen_image(image);
        ui.set_show_media_screen(true);
    }
}

fn close_media_screen(ui: &MainWindow) {
    let _ = with_ui_runtime_mut(|runtime| {
        runtime.fullscreen_media_key = None;
    });
    ui.set_show_media_screen(false);
}

fn run_gif_scheduler_tick(ui: &MainWindow) {
    let mut fullscreen_image = None;
    let _ = with_ui_runtime_mut(|runtime| {
        let now_ms = runtime.now_ms();
        if let Some(media_key) = runtime.fullscreen_media_key.clone() {
            runtime
                .gif_controller
                .register_visibility_ping(media_key, now_ms);
        }

        for update in runtime.gif_controller.tick(now_ms) {
            let Some(row_index) = runtime
                .media_row_index_by_key
                .get(&update.media_key)
                .copied()
            else {
                continue;
            };
            let Some(mut row) = runtime.messages_model.row_data(row_index) else {
                continue;
            };
            row.media_image = update.frame.clone();
            row.has_media_image = true;
            runtime.messages_model.set_row_data(row_index, row);

            if runtime.fullscreen_media_key.as_deref() == Some(update.media_key.as_str()) {
                fullscreen_image = Some(update.frame);
            }
        }

        if fullscreen_image.is_none()
            && let Some(media_key) = runtime.fullscreen_media_key.as_deref()
        {
            fullscreen_image = runtime.gif_controller.current_frame_for(media_key);
        }
    });

    if let Some(image) = fullscreen_image {
        ui.set_media_screen_image(image);
    }
}

fn apply_snapshot_to_ui(ui: &MainWindow, snapshot: DesktopSnapshot) {
    trace!(
        rooms = snapshot.rooms.len(),
        messages = snapshot.messages.len(),
        selected = snapshot.selected_room_id.as_deref().unwrap_or(""),
        status = %snapshot.status_text,
        has_error = snapshot.error_text.is_some(),
        "applying snapshot to ui"
    );
    let DesktopSnapshot {
        rooms,
        messages,
        selected_room_id,
        status_text,
        error_text,
        can_send,
        show_login_screen,
        login_homeserver,
        login_user_id,
        login_password,
        login_remember_password,
        login_in_flight,
        show_logout_confirm,
        show_security_dialog,
        security_dialog_title,
        security_dialog_body,
        security_show_copy_button,
        security_copy_button_text,
        security_show_restore_input,
        security_restore_button_text,
        security_restore_in_flight,
        composer_attachment,
        ..
    } = snapshot;

    let rooms = rooms
        .into_iter()
        .map(|room| RoomRow {
            room_id: room.room_id.into(),
            display_name: room.display_name.into(),
            unread_badge: if room.unread_notifications > 0 {
                room.unread_notifications.to_string().into()
            } else {
                "".into()
            },
            highlight_badge: if room.highlight_count > 0 {
                room.highlight_count.to_string().into()
            } else {
                "".into()
            },
            has_unread: room.unread_notifications > 0,
            has_highlight: room.highlight_count > 0,
            is_selected: room.is_selected,
            is_invite: room.membership == RoomMembership::Invited,
            invite_pending: room.invite_pending,
            invite_pending_text: room.invite_pending_text.into(),
        })
        .collect::<Vec<_>>();

    let (fullscreen_image, fullscreen_hidden) = with_ui_runtime_mut(|runtime| {
        let mut media_row_index_by_key = HashMap::new();
        let mut active_gif_keys = HashSet::new();
        let mapped_messages = messages
            .into_iter()
            .enumerate()
            .map(|(row_index, message)| {
                let mut media_image = message
                    .media_cached_path
                    .as_deref()
                    .and_then(load_media_image_from_path);
                let mut media_hint = message.media_hint.unwrap_or_default();

                if message.event_kind == "image"
                    && message.media_is_gif
                    && !message.media_key.is_empty()
                    && message.media_cached_path.is_some()
                {
                    let media_key = message.media_key.clone();
                    let cached_path = message.media_cached_path.clone().unwrap_or_default();
                    active_gif_keys.insert(media_key.clone());

                    let should_attach = runtime
                        .attached_gif_sources
                        .get(&media_key)
                        .map(|path| path != &cached_path)
                        .unwrap_or(true);
                    if should_attach {
                        runtime
                            .gif_controller
                            .attach_source(media_key.clone(), Path::new(&cached_path));
                        runtime
                            .attached_gif_sources
                            .insert(media_key.clone(), cached_path);
                    }

                    if let Some(frame) = runtime.gif_controller.current_frame_for(&media_key) {
                        media_image = Some(frame);
                    }
                    if media_hint.is_empty()
                        && let Some(hint) = runtime.gif_controller.hint_for(&media_key)
                    {
                        media_hint = hint;
                    }
                }

                if !message.media_key.is_empty() {
                    media_row_index_by_key.insert(message.media_key.clone(), row_index);
                }

                MessageRow {
                    event_id: message.event_id.unwrap_or_default().into(),
                    local_id: message.local_id.unwrap_or_default().into(),
                    sender: message.sender.into(),
                    body: message.body.into(),
                    event_kind: message.event_kind.into(),
                    caption: message.caption.into(),
                    media_status: message.media_status.into(),
                    media_source: message.media_source.unwrap_or_default().into(),
                    media_cached_path: message.media_cached_path.unwrap_or_default().into(),
                    media_key: message.media_key.into(),
                    media_is_gif: message.media_is_gif,
                    media_hint: media_hint.into(),
                    has_media_image: media_image.is_some(),
                    media_image: media_image.unwrap_or_default(),
                    can_retry: message.can_retry,
                    can_remove: message.can_remove,
                    is_own: message.is_own,
                }
            })
            .collect::<Vec<_>>();

        runtime
            .attached_gif_sources
            .retain(|media_key, _| active_gif_keys.contains(media_key));
        runtime.media_row_index_by_key = media_row_index_by_key;
        runtime.messages_model.set_vec(mapped_messages);

        let fullscreen_hidden = runtime
            .fullscreen_media_key
            .as_ref()
            .is_some_and(|media_key| !runtime.media_row_index_by_key.contains_key(media_key));
        if fullscreen_hidden {
            runtime.fullscreen_media_key = None;
        }

        let fullscreen_image = runtime
            .fullscreen_media_key
            .as_deref()
            .and_then(|media_key| runtime.gif_controller.current_frame_for(media_key));

        (fullscreen_image, fullscreen_hidden)
    })
    .unwrap_or_else(|| {
        warn!("ui runtime state not initialized while applying snapshot");
        (None, false)
    });

    let error_text = error_text.unwrap_or_default();

    ui.set_rooms(slint::ModelRc::new(slint::VecModel::from(rooms)));
    ui.set_selected_room_id(selected_room_id.unwrap_or_default().into());
    ui.set_status_text(status_text.into());
    ui.set_error_text(error_text.clone().into());
    ui.set_has_error(!error_text.is_empty());
    ui.set_can_send(can_send);
    ui.set_show_login_screen(show_login_screen);
    ui.set_login_homeserver(login_homeserver.into());
    ui.set_login_user_id(login_user_id.into());
    ui.set_login_password(login_password.into());
    ui.set_login_remember_password(login_remember_password);
    ui.set_login_in_flight(login_in_flight);
    ui.set_show_logout_confirm(show_logout_confirm);
    if let Some(attachment) = composer_attachment {
        ui.set_composer_attachment_name(attachment.file_name.into());
        ui.set_has_composer_attachment(true);
    } else {
        ui.set_composer_attachment_name("".into());
        ui.set_has_composer_attachment(false);
    }
    ui.set_show_security_screen(show_security_dialog);
    ui.set_security_title(security_dialog_title.into());
    ui.set_security_body(security_dialog_body.into());
    ui.set_security_show_copy_button(security_show_copy_button);
    ui.set_security_copy_button_text(security_copy_button_text.into());
    ui.set_security_show_restore_input(security_show_restore_input);
    ui.set_security_restore_button_text(security_restore_button_text.into());
    ui.set_security_restore_in_flight(security_restore_in_flight);

    if fullscreen_hidden {
        ui.set_show_media_screen(false);
    } else if let Some(image) = fullscreen_image {
        ui.set_media_screen_image(image);
    }
}

fn load_media_image_from_path(path: &str) -> Option<slint::Image> {
    slint::Image::load_from_path(Path::new(path)).ok()
}
