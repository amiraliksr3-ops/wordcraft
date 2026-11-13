//! What Android does differently: file dialogs, the clipboard and the keyboard.
//!
//! `rfd` doesn't support Android and `arboard` has no Android clipboard backend,
//! so the desktop hooks can't be reused. Instead the app's activity
//! (`WordCraftActivity.java`) shows the Storage Access Framework pickers and
//! owns a clipboard and a hidden `EditText` titled keyboard; this module is the
//! JNI bridge to it, and turns what comes back into the shapes `wordcraft-ui-egui`
//! expects in `Services`.
//!
//! Two Android facts shape the file handling:
//!
//! * The engine reads and writes real paths, and a `content://` URI is not one.
//!   A picked file is therefore copied into the app's `files/media`, and the
//!   app works on that copy.
//! * Saving has to land in the document the user picked, not only in that copy.
//!   Every staged file remembers the URI it came from, and each frame this
//!   module notices a staged file that has been written and asks the activity to
//!   copy it back over the URI (the `SaveMirror` in the artcraft suite works the
//!   same way).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, UNIX_EPOCH};

use egui::Context;
use jni::JNIEnv;
use jni::JavaVM;
use jni::objects::{GlobalRef, JObject, JObjectArray, JString, JValue};
use jni::sys::{jboolean, jint};
use wordcraft_ui_egui::{FileDialogAnswer, FileDialogRequest};

/// The Java VM, for calls made from the render thread.
static VM: OnceLock<JavaVM> = OnceLock::new();
/// The running activity, as a global reference (the pointer `android-activity`
/// hands over is borrowed and may be a local reference).
static ACTIVITY: OnceLock<GlobalRef> = OnceLock::new();
/// The system bars' edges in physical pixels, left, top, right, bottom, as the
/// activity's window insets report them.
static INSETS_PX: [AtomicI32; 4] = [const { AtomicI32::new(0) }; 4];
/// Whether the keyboard is on screen, from the activity's window insets: the
/// only truth about it, since a phone hides the keyboard without telling the
/// app and without the page losing focus.
static KEYBOARD_UP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// The egui context, so an answer that arrives between frames still repaints.
static CONTEXT: OnceLock<Context> = OnceLock::new();
/// Where picked files are staged.
static MEDIA: OnceLock<PathBuf> = OnceLock::new();

/// Asked pickers waiting for an answer: request id, whether it picks a save
/// destination, and the channel the answer goes back through.
static PICKERS: Mutex<Vec<(i32, bool, Sender<FileDialogAnswer>)>> = Mutex::new(Vec::new());
/// Staged files to write back to their document: the path and the size and
/// modification time last seen.
static STAGED: Mutex<Vec<(PathBuf, (u64, u64))>> = Mutex::new(Vec::new());
/// Characters and keys typed on the soft keyboard since the last frame.
static TYPED: Mutex<Vec<Typed>> = Mutex::new(Vec::new());

static NEXT_ID: AtomicI32 = AtomicI32::new(1);
/// After a save destination is picked, keep asking for frames for a while: the
/// frame that notices the written file is what copies it back, and an idle app
/// would otherwise not draw again.
static MIRROR_UNTIL: Mutex<Option<Instant>> = Mutex::new(None);

/// One thing the keyboard produced.
enum Typed {
    Text(String),
    /// A key code: 67 is Backspace, 66 Enter.
    Key(i32),
}

pub fn set_context(ctx: &Context) {
    if CONTEXT.get().is_none() {
        let _ = CONTEXT.set(ctx.clone());
    }
}

fn request_repaint() {
    if let Some(ctx) = CONTEXT.get() {
        ctx.request_repaint();
    }
}

/// Attach to the VM, make our own global reference to the activity, and keep
/// where picked files are staged.
pub fn init(app: &android_activity::AndroidApp, media: &Path) {
    let _ = MEDIA.set(media.to_path_buf());
    // SAFETY: `vm_as_ptr` is the running VM, owned by the process.
    let vm = match unsafe { JavaVM::from_raw(app.vm_as_ptr().cast()) } {
        Ok(vm) => vm,
        Err(e) => return log::error!("no Java VM: {e}"),
    };
    {
        let Ok(env) = vm.attach_current_thread() else {
            return log::error!("cannot attach to the Java VM");
        };
        // SAFETY: the pointer is a valid activity reference; we only read it to
        // make a global reference of our own, and never delete it.
        let borrowed = unsafe { JObject::from_raw(app.activity_as_ptr().cast()) };
        match env.new_global_ref(&borrowed) {
            Ok(activity) => {
                let _ = ACTIVITY.set(activity);
            }
            Err(e) => log::error!("cannot keep the activity: {e}"),
        }
    }
    let _ = VM.set(vm);
}

/// Run `f` with a JNI environment and the activity. `None` when the bridge isn't
/// up yet or the call failed (a Java exception is cleared, never left pending).
fn with_env<R>(f: impl FnOnce(&mut JNIEnv<'_>, &JObject<'_>) -> jni::errors::Result<R>) -> Option<R> {
    let vm = VM.get()?;
    let activity = ACTIVITY.get()?;
    let mut guard = match vm.attach_current_thread() {
        Ok(guard) => guard,
        Err(e) => {
            log::warn!("cannot attach to the Java VM: {e}");
            return None;
        }
    };
    let env: &mut JNIEnv<'_> = &mut guard;
    match f(env, activity.as_obj()) {
        Ok(value) => Some(value),
        Err(e) => {
            log::warn!("JNI call failed: {e}");
            let _ = env.exception_clear();
            None
        }
    }
}

fn call_str(env: &mut JNIEnv<'_>, activity: &JObject<'_>, method: &str, arg: &str) -> jni::errors::Result<()> {
    let arg = env.new_string(arg)?;
    let arg: JObject<'_> = arg.into();
    env.call_method(activity, method, "(Ljava/lang/String;)V", &[JValue::Object(&arg)])?;
    Ok(())
}

fn call_str_int(env: &mut JNIEnv<'_>, activity: &JObject<'_>, method: &str, arg: &str, id: i32) -> jni::errors::Result<()> {
    let arg = env.new_string(arg)?;
    let arg: JObject<'_> = arg.into();
    let id = JValue::Int(id);
    env.call_method(activity, method, "(Ljava/lang/String;I)V", &[JValue::Object(&arg), id])?;
    Ok(())
}

fn take_string(env: &mut JNIEnv<'_>, obj: JObject<'_>) -> Option<String> {
    if obj.is_null() {
        return None;
    }
    env.get_string(&JString::from(obj)).ok().map(String::from)
}

/// The `Services::file_dialog` hook: ask the activity for a picker and hand the
/// app a channel its answer arrives on later, so no frame blocks on the user.
pub fn file_dialog(request: FileDialogRequest) -> Receiver<FileDialogAnswer> {
    let (tx, rx) = channel();
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let saves = matches!(request, FileDialogRequest::Save { .. });
    if let Ok(mut pickers) = PICKERS.lock() {
        pickers.push((id, saves, tx));
    }
    let asked = match &request {
        FileDialogRequest::Save { name, .. } => {
            with_env(|env, activity| call_str_int(env, activity, "pickSave", name, id)).is_some()
        }
        FileDialogRequest::Open { purpose } | FileDialogRequest::OpenMany { purpose } => {
            with_env(|env, activity| call_str_int(env, activity, "pickOpen", purpose, id)).is_some()
        }
    };
    if !asked {
        log::warn!("the activity could not show a picker for {request:?}");
        if let Ok(mut pickers) = PICKERS.lock()
            && let Some(at) = pickers.iter().position(|p| p.0 == id)
        {
            let (_, _, tx) = pickers.remove(at);
            let _ = tx.send(Ok(Vec::new()));
        }
    }
    rx
}

/// The text on the system clipboard, if any (`Services::clipboard_text`).
pub fn clipboard_text() -> Option<String> {
    with_env(|env, activity| {
        let value = env.call_method(activity, "getClipboardText", "()Ljava/lang/String;", &[])?;
        Ok(take_string(env, value.l()?))
    })
    .flatten()
}

/// The HTML on the system clipboard, if any (`Services::clipboard_html`).
pub fn clipboard_html() -> Option<String> {
    with_env(|env, activity| {
        let value = env.call_method(activity, "getClipboardHtml", "()Ljava/lang/String;", &[])?;
        Ok(take_string(env, value.l()?))
    })
    .flatten()
}

/// Put a copy on the clipboard as text and HTML (`Services::set_clipboard_rich`).
pub fn set_clipboard_rich(text: &str, html: &str, _rtf: &str, _mathml: &str) -> bool {
    with_env(|env, activity| {
        let text = env.new_string(text)?;
        let text: JObject<'_> = text.into();
        let html = env.new_string(html)?;
        let html: JObject<'_> = html.into();
        let done = env.call_method(
            activity,
            "setClipboardRich",
            "(Ljava/lang/String;Ljava/lang/String;)Z",
            &[JValue::Object(&text), JValue::Object(&html)],
        )?;
        done.z()
    })
    .unwrap_or(false)
}

/// The formats the clipboard holds (`Services::clipboard_formats`).
pub fn clipboard_formats() -> Vec<String> {
    with_env(|env, activity| {
        let value = env.call_method(activity, "clipboardFormats", "()[Ljava/lang/String;", &[])?;
        let array = value.l()?;
        let mut formats = Vec::new();
        if array.is_null() {
            return Ok(formats);
        }
        let array = JObjectArray::from(array);
        for index in 0..env.get_array_length(&array)? {
            let item = env.get_object_array_element(&array, index)?;
            if let Some(name) = take_string(env, item) {
                formats.push(name);
            }
        }
        Ok(formats)
    })
    .unwrap_or_default()
}

/// Show or hide the soft keyboard.
pub fn set_keyboard_visible(visible: bool) {
    with_env(|env, activity| {
        env.call_method(activity, "setKeyboardVisible", "(Z)V", &[JValue::Bool(visible as jboolean)])?;
        Ok(())
    });
}

/// Hand the app what was typed since the last frame, and write finished saves
/// back to the documents they came from. Called every frame from `logic`.
pub fn pump(ctx: &Context) {
    let typed = TYPED.lock().map(|mut t| std::mem::take(&mut *t)).unwrap_or_default();
    if !typed.is_empty() {
        ctx.input_mut(|input| {
            for event in typed {
                match event {
                    Typed::Text(text) => input.events.push(egui::Event::Text(text)),
                    Typed::Key(67) => input.events.push(key_event(egui::Key::Backspace)),
                    Typed::Key(66) => input.events.push(key_event(egui::Key::Enter)),
                    Typed::Key(27) => input.events.push(key_event(egui::Key::Escape)),
                    Typed::Key(_) => {}
                }
            }
        });
    }
    mirror_writes();
    keep_mirroring(ctx);
}

/// Keep drawing while a freshly picked save destination may be written, so the
/// write is copied back promptly (battery is why it stops).
fn keep_mirroring(ctx: &Context) {
    let Ok(mut until) = MIRROR_UNTIL.lock() else { return };
    match *until {
        Some(at) if Instant::now() < at => ctx.request_repaint_after(Duration::from_millis(300)),
        Some(_) => *until = None,
        None => {}
    }
}

fn key_event(key: egui::Key) -> egui::Event {
    egui::Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers: egui::Modifiers::NONE }
}

/// Remember a staged file so a write to it is copied back to the document.
fn watch(path: &str) {
    let path = PathBuf::from(path);
    let seen = stamp(&path);
    if let Ok(mut staged) = STAGED.lock()
        && !staged.iter().any(|(p, _)| *p == path)
    {
        staged.push((path, seen));
    }
}

fn stamp(path: &Path) -> (u64, u64) {
    let Ok(meta) = std::fs::metadata(path) else { return (0, 0) };
    let modified = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_millis() as u64);
    (meta.len(), modified)
}

/// Copy every staged file that was written since the last frame over the
/// `content://` document it came from.
fn mirror_writes() {
    let written: Vec<PathBuf> = {
        let Ok(mut staged) = STAGED.lock() else { return };
        let mut written = Vec::new();
        for (path, seen) in staged.iter_mut() {
            let now = stamp(path);
            if now != *seen {
                *seen = now;
                if now.0 > 0 {
                    written.push(path.clone());
                }
            }
        }
        written
    };
    for path in written {
        let Some(name) = path.to_str() else { continue };
        log::info!("saving {name} back to the document it came from");
        with_env(|env, activity| call_str(env, activity, "commitStaged", name));
    }
}

/// A picker answered (`WordCraftActivity.nativePicked`): the staged paths, empty
/// when the user cancelled.
///
/// # Safety
/// Called by the JVM with a valid `paths` array; the references are borrowed.
#[unsafe(no_mangle)]
pub extern "system" fn Java_ai_storyteller_wordcraft_WordCraftActivity_nativeInsets(
    _env: JNIEnv<'_>,
    _class: JObject<'_>,
    left: jint,
    top: jint,
    right: jint,
    bottom: jint,
    keyboard: jboolean,
) {
    INSETS_PX[0].store(left, Ordering::Relaxed);
    INSETS_PX[1].store(top, Ordering::Relaxed);
    INSETS_PX[2].store(right, Ordering::Relaxed);
    INSETS_PX[3].store(bottom, Ordering::Relaxed);
    KEYBOARD_UP.store(keyboard != 0, Ordering::Relaxed);
}

/// The system bars' edges in egui points: left, top, right, bottom.
pub fn insets(ctx: &Context) -> (f32, f32, f32, f32) {
    let scale = ctx.pixels_per_point().max(0.5);
    let read = |index: usize| INSETS_PX[index].load(Ordering::Relaxed).max(0) as f32 / scale;
    (read(0), read(1), read(2), read(3))
}

/// Whether the keyboard is up, as the activity's window insets say.
pub fn keyboard_shown() -> bool {
    KEYBOARD_UP.load(Ordering::Relaxed)
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_ai_storyteller_wordcraft_WordCraftActivity_nativePicked(
    mut env: JNIEnv<'_>,
    _class: JObject<'_>,
    id: jint,
    paths: JObjectArray<'_>,
) {
    let mut picked = Vec::new();
    if !paths.is_null()
        && let Ok(len) = env.get_array_length(&paths)
    {
        for index in 0..len {
            if let Ok(item) = env.get_object_array_element(&paths, index)
                && let Some(path) = take_string(&mut env, item)
            {
                picked.push(path);
            }
        }
    }
    let waiting = PICKERS
        .lock()
        .ok()
        .and_then(|mut pickers| pickers.iter().position(|p| p.0 == id).map(|at| pickers.remove(at)));
    if let Some((_, saves, answer)) = waiting {
        // An opened or saved file is a document from now on: writing to the
        // staged copy has to reach the file the user picked.
        if !saves {
            for path in &picked {
                watch(path);
            }
        } else if let Some(path) = picked.first() {
            watch(path);
            if let Ok(mut until) = MIRROR_UNTIL.lock() {
                *until = Some(Instant::now() + Duration::from_secs(30));
            }
        }
        let _ = answer.send(Ok(picked));
    }
    request_repaint();
}

/// Characters typed on the soft keyboard.
///
/// # Safety
/// Called by the JVM with a valid string.
#[unsafe(no_mangle)]
pub extern "system" fn Java_ai_storyteller_wordcraft_WordCraftActivity_nativeImeText(
    mut env: JNIEnv<'_>,
    _class: JObject<'_>,
    text: JString<'_>,
) {
    if let Some(text) = take_string(&mut env, text.into())
        && let Ok(mut typed) = TYPED.lock()
    {
        typed.push(Typed::Text(text));
    }
    request_repaint();
}

/// A key the soft keyboard sent (Backspace or Enter).
///
/// # Safety
/// Called by the JVM with a key code.
#[unsafe(no_mangle)]
pub extern "system" fn Java_ai_storyteller_wordcraft_WordCraftActivity_nativeImeKey(
    _env: JNIEnv<'_>,
    _class: JObject<'_>,
    key: jint,
) {
    if let Ok(mut typed) = TYPED.lock() {
        typed.push(Typed::Key(key));
    }
    request_repaint();
}

/// The back button was pressed: close whatever is open, as Escape does. The
/// activity exits on a second press, so nothing is lost by one stray tap.
///
/// # Safety
/// Called by the JVM.
#[unsafe(no_mangle)]
pub extern "system" fn Java_ai_storyteller_wordcraft_WordCraftActivity_nativeBack(
    _env: JNIEnv<'_>,
    _class: JObject<'_>,
) {
    if let Ok(mut typed) = TYPED.lock() {
        typed.push(Typed::Key(27));
    }
    request_repaint();
}
