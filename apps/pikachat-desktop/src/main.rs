mod bridge;
mod config;
mod logging;
mod state;

use std::sync::Arc;

use backend_matrix::{MatrixFrontendAdapter, spawn_runtime};
use bridge::{DesktopBridge, UiUpdateCallback};
use config::DesktopConfig;
use state::{DesktopSnapshot, clamp_sidebar_width};
use tracing::{debug, error, info, trace};

slint::include_modules!();

fn main() -> Result<(), slint::PlatformError> {
    logging::init();
    info!("starting pikachat-desktop");

    let ui = MainWindow::new()?;

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
                homeserver = %config.homeserver,
                user_id = %config.user_id,
                data_dir = %config.data_dir.display(),
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
                ui.on_room_selected(move |index| {
                    debug!(index, "room row selected from ui");
                    bridge.select_room_by_index(index);
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

            ui.on_room_selected(|_| {});
            ui.on_send_message_requested(|_| false);
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
    drop(bridge);
    drop(runtime);
    run_result
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
    let rooms = snapshot
        .rooms
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
        })
        .collect::<Vec<_>>();

    let messages = snapshot
        .messages
        .into_iter()
        .map(|message| MessageRow {
            event_id: message.event_id.unwrap_or_default().into(),
            sender: message.sender.into(),
            body: message.body.into(),
            is_own: message.is_own,
        })
        .collect::<Vec<_>>();

    let error_text = snapshot.error_text.unwrap_or_default();

    ui.set_rooms(slint::ModelRc::new(slint::VecModel::from(rooms)));
    ui.set_messages(slint::ModelRc::new(slint::VecModel::from(messages)));
    ui.set_selected_room_id(snapshot.selected_room_id.unwrap_or_default().into());
    ui.set_status_text(snapshot.status_text.into());
    ui.set_error_text(error_text.clone().into());
    ui.set_has_error(!error_text.is_empty());
    ui.set_can_send(snapshot.can_send);
    ui.set_show_security_screen(snapshot.show_security_dialog);
    ui.set_security_title(snapshot.security_dialog_title.into());
    ui.set_security_body(snapshot.security_dialog_body.into());
    ui.set_security_show_copy_button(snapshot.security_show_copy_button);
    ui.set_security_copy_button_text(snapshot.security_copy_button_text.into());
    ui.set_security_show_restore_input(snapshot.security_show_restore_input);
    ui.set_security_restore_button_text(snapshot.security_restore_button_text.into());
    ui.set_security_restore_in_flight(snapshot.security_restore_in_flight);
}
