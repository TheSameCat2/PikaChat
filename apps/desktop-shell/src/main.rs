slint::include_modules!();

fn main() -> Result<(), slint::PlatformError> {
    let ui = MainWindow::new()?;

    let weak = ui.as_weak();
    ui.on_quit_requested(move || {
        if let Some(ui) = weak.upgrade() {
            let _ = ui.hide();
        }
        let _ = slint::quit_event_loop();
    });

    let weak = ui.as_weak();
    ui.on_about_slint_requested(move || {
        if let Some(ui) = weak.upgrade() {
            ui.set_show_about_screen(true);
        }
    });

    ui.run()
}
