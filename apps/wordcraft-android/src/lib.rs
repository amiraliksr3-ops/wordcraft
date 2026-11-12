//! WordCraft on Android.
//!
//! `wordcraft-ui-egui` already owns the whole interface; this crate is only the
//! activity shell: eframe needs the `AndroidApp` handle handed to `android_main`
//! before it can build an event loop, and the services it talks to are the
//! Android-shaped subset (no printers, no desktop file dialogs yet).

use egui::Context;
use wordcraft_engine::Session;
use wordcraft_ui_egui::{Services, WordApp};

/// The desktop app wraps `WordApp` with window geometry, dialogs and
/// printing; on a phone the wrapper only has to advance frames.
struct Shell(WordApp);

impl eframe::App for Shell {
    fn logic(&mut self, ctx: &Context, _frame: &mut eframe::Frame) {
        self.0.logic(ctx);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.0.ui(ui);
    }
}

fn new_shell() -> Shell {
    let session = Session::new(wordcraft_doc::Document::new());
    Shell(WordApp::new(session, Services::default()))
}

/// Android's entry point, called by `android-activity` from the running activity.
#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
fn android_main(app: android_activity::AndroidApp) {
    let mut options = eframe::NativeOptions::default();
    options.android_app = Some(app);
    if let Err(err) = eframe::run_native(
        "WordCraft",
        options,
        Box::new(|_cc| Ok(Box::new(new_shell()))),
    ) {
        log::error!("WordCraft could not start: {err}");
    }
}
