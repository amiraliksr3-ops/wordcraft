//! WordCraft on Android.
//!
//! `wordcraft-ui-egui` already owns the whole interface; this crate is the
//! activity shell plus the platform services the desktop app gets from
//! `apps/wordcraft`: Storage Access Framework file dialogs (Android has no
//! `rfd`), the clipboard, and the on-screen keyboard.
//!
//! Android's app folders differ from a desktop's (there is no `$HOME` and no
//! XDG config directory), so `android_main` points `WORDCRAFT_PROFILE_DIR` at
//! the app's own writable `files/config` before anything reads it: preferences,
//! templates, the log and the user's imported fonts all live there.

mod platform;

use std::path::Path;

use egui::Context;
use wordcraft_engine::Session;
use wordcraft_ui_egui::{Services, UiState, WordApp};

/// The desktop app wraps `WordApp` with window geometry, dialogs and
/// printing; on a phone the wrapper owns the platform services instead.
struct Shell {
    app: WordApp,
    /// The touch-sized style is installed on the first frame, once the context
    /// exists.
    styled: bool,
    /// Whether the soft keyboard is up, so it is only shown or hidden on change.
    keyboard: bool,
    /// Whether the page has been touched yet. Something holds keyboard focus
    /// from the first frame, so without this the keyboard would cover half the
    /// screen the moment the app opens.
    touched: bool,
}

impl eframe::App for Shell {
    fn logic(&mut self, ctx: &Context, _frame: &mut eframe::Frame) {
        platform::set_context(ctx);
        if !self.styled {
            self.styled = true;
            touch_style(ctx);
        }
        // Typed characters, key presses, finished SAF saves and picker answers
        // that arrived since the last frame.
        platform::pump(ctx);
        self.app.logic(ctx);
        if ctx.input(|input| input.pointer.any_pressed()) {
            self.touched = true;
        }
        // The keyboard belongs up while something has keyboard focus, and down
        // otherwise: a phone has no room to leave it open behind a menu. The
        // document is a custom canvas rather than a text field, so "wants a
        // keyboard" alone never comes true there; a focused widget is the signal
        // that covers both, and the touch gate keeps it down until the page is
        // touched.
        let focused = ctx.egui_wants_keyboard_input() || ctx.memory(|memory| memory.focused().is_some());
        let wanted = self.touched && focused;
        if wanted != self.keyboard {
            self.keyboard = wanted;
            platform::set_keyboard_visible(wanted);
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.app.ui(ui);
    }
}

/// The desktop UI at its desktop sizes is a mouse UI: 20 pt rows and hover
/// tooltips. These are the few style numbers a fingertip needs, applied over
/// the theme rather than replacing it, so the layout stays the Word-like one.
fn touch_style(ctx: &Context) {
    ctx.all_styles_mut(|style| {
        // 48 dp is Android's touch minimum; the desktop's rows are about 20.
        style.spacing.interact_size.y = 48.0;
        style.spacing.button_padding = egui::vec2(12.0, 8.0);
        style.spacing.item_spacing = egui::vec2(8.0, 6.0);
        style.spacing.menu_margin = egui::Margin::same(8);
        style.spacing.slider_width = 160.0;
        // A finger that moves a few pixels is still a tap, and labels should not
        // turn into selectable text under one.
        style.interaction.selectable_labels = false;
        style.interaction.show_tooltips_only_when_still = true;
    });
}

fn services() -> Services {
    Services {
        file_dialog: Some(Box::new(platform::file_dialog)),
        clipboard_text: Some(Box::new(platform::clipboard_text)),
        clipboard_html: Some(Box::new(platform::clipboard_html)),
        set_clipboard_rich: Some(Box::new(platform::set_clipboard_rich)),
        clipboard_formats: Some(Box::new(platform::clipboard_formats)),
        save_prefs: Some(Box::new(save_prefs)),
        templates_dir: wordcraft_control_key::settings_dir().map(|d| d.join("Templates")),
        animations_enabled: Some(Box::new(|| Some(true))),
        ..Default::default()
    }
}

/// Write the preferences now, as the desktop app does (#2716): Android kills
/// backgrounded apps without a normal close, so waiting for the exit would lose
/// the recent list and the theme.
fn save_prefs(app: &WordApp) -> Result<bool, String> {
    let Some(dir) = wordcraft_control_key::settings_dir() else { return Ok(false) };
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let bytes = serde_json::to_vec_pretty(&app.prefs()).map_err(|e| format!("cannot serialize: {e}"))?;
    let path = dir.join("ui.json");
    let tmp = dir.join("ui.json.tmp");
    std::fs::write(&tmp, &bytes).map_err(|e| format!("cannot save {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("cannot save {}: {e}", path.display()))?;
    Ok(true)
}

/// The preferences saved last run, for the theme, the recent list and the
/// language.
fn saved_ui() -> Option<UiState> {
    let path = wordcraft_control_key::settings_dir()?.join("ui.json");
    UiState::from_json(&std::fs::read(path).ok()?)
}

/// The activity handle, kept for the whole process.
///
/// Dropping the last `AndroidApp` destroys android-activity's native state, and
/// its `Drop` detaches the input queue from the framework's looper. A
/// configuration change (a rotation, a window resize, the keyboard coming up)
/// can land in the middle of that and aborts the process with
/// "pthread_mutex_lock called on a destroyed mutex" from
/// `InputQueue::detachLooper`. android-activity documents holding a handle in a
/// `'static` as supported, so this keeps one.
static ACTIVITY: std::sync::OnceLock<android_activity::AndroidApp> = std::sync::OnceLock::new();

fn new_shell() -> Shell {
    let session = Session::new(wordcraft_doc::Document::new());
    let mut app = WordApp::new(session, services());
    if let Some(ui) = saved_ui() {
        app.apply_prefs(ui);
    }
    // `keyboard` starts true so the first frame hides the keyboard: the
    // framework can restore one when the window is created, and nothing should
    // cover the page until something is touched.
    Shell { app, styled: false, keyboard: true, touched: false }
}

/// `AndroidLog`: `log` has no platform sink here, and a phone has no stderr a
/// person can read. Info and above go to logcat under the `WordCraft` tag.
struct AndroidLog;

#[cfg(target_os = "android")]
#[link(name = "log")]
unsafe extern "C" {
    fn __android_log_write(priority: i32, tag: *const i8, text: *const i8) -> i32;
}

impl log::Log for AndroidLog {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= log::Level::Info
    }

    fn log(&self, record: &log::Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let line = format!("{}\0", record.args());
        let tag = b"WordCraft\0";
        // SAFETY: both strings are NUL-terminated and live for the call.
        #[cfg(target_os = "android")]
        unsafe {
            __android_log_write(4, tag.as_ptr().cast(), line.as_ptr().cast());
        }
        #[cfg(not(target_os = "android"))]
        let _ = (tag, line);
    }

    fn flush(&self) {}
}

fn install_logger() {
    static LOGGER: AndroidLog = AndroidLog;
    let _ = log::set_logger(&LOGGER);
    log::set_max_level(log::LevelFilter::Info);
}

/// The app's writable folders, made before anything reads them: `config` holds
/// preferences, templates and imported fonts; `media` holds files the Storage
/// Access Framework hands over (the engine reads and writes real paths only).
fn prepare_folders(files: &Path) -> std::path::PathBuf {
    for dir in [files.join("config"), files.join("config/fonts"), files.join("media")] {
        if let Err(e) = std::fs::create_dir_all(&dir) {
            log::error!("cannot create {}: {e}", dir.display());
        }
    }
    files.join("config")
}

/// Android's entry point, called by `android-activity` from the running activity.
#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
fn android_main(app: android_activity::AndroidApp) {
    install_logger();
    let Some(files) = app.internal_data_path() else {
        log::error!("the app has no data folder; WordCraft cannot start");
        return;
    };
    let config = prepare_folders(&files);
    // SAFETY: this runs before any thread reads the environment, and every
    // later reader (preferences, templates, fonts, logs) sees it.
    unsafe {
        std::env::set_var("WORDCRAFT_PROFILE_DIR", &config);
    }
    // Fonts the user imported through the picker load before the first frame,
    // like the desktop's (#582).
    let (loaded, skipped) = wordcraft_fonts::FontDb::global().load_user_fonts(&config.join("fonts"));
    log::info!("user fonts: {} loaded, {} skipped", loaded.len(), skipped.len());
    platform::init(&app, &files.join("media"));
    let _ = ACTIVITY.set(app.clone());

    let mut options = eframe::NativeOptions::default();
    options.android_app = Some(app);
    if let Err(err) = eframe::run_native(
        "WordCraft",
        options,
        Box::new(|cc| {
            platform::set_context(&cc.egui_ctx);
            Ok(Box::new(new_shell()))
        }),
    ) {
        log::error!("WordCraft could not start: {err}");
    }
}
