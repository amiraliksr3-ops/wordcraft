package ai.storyteller.wordcraft;

import android.app.NativeActivity;
import android.content.ClipData;
import android.content.ClipboardManager;
import android.content.Context;
import android.content.Intent;
import android.content.SharedPreferences;
import android.database.Cursor;
import android.net.Uri;
import android.os.Build;
import android.os.Bundle;
import android.os.Looper;
import android.provider.OpenableColumns;
import android.text.Editable;
import android.text.InputType;
import android.text.TextWatcher;
import android.view.KeyEvent;
import android.view.ViewGroup;
import android.view.WindowManager;
import android.view.inputmethod.EditorInfo;
import android.view.inputmethod.InputMethodManager;
import android.widget.EditText;
import android.widget.Toast;

import java.io.File;
import java.io.FileInputStream;
import java.io.FileOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.util.ArrayList;
import java.util.List;

/**
 * The Android activity: a {@link NativeActivity} that paints through eframe, with
 * the platform pieces a native activity doesn't have.
 *
 * <p>{@code NativeActivity} has no file pickers (results come back through
 * {@link #onActivityResult}, which native code never sees) and no input
 * connection (a soft keyboard's characters never reach the app), so a class that
 * can override those is unavoidable. Everything here is small on purpose: the
 * editor is the Rust side; this is the bridge.
 *
 * <p>The engine reads and writes real paths, so a picked document is copied into
 * {@code files/media} and edited there. {@link #commitStaged} writes that copy
 * back over the {@code content://} document the user picked, which is what makes
 * Save (and Export, which is also how the ribbon's Share works) reach the file
 * they chose.
 */
public class WordCraftActivity extends NativeActivity {
    private static final int REQ_OPEN = 1001;
    private static final int REQ_SAVE = 1002;
    private static final int REQ_FONT = 1003;

    private static final String SAF = "saf";
    private static final String URI_PREFIX = "uri:";

    /** The picker waiting for an answer, and what it is for. */
    private int openId;
    private int saveId;
    private String suggestedName;
    private String openPurpose;

    /** The hidden field that owns the input connection. */
    private EditText imeSink;
    private String imeText = "";

    private long lastBack;

    @Override
    protected void onCreate(Bundle state) {
        super.onCreate(state);
        getWindow().addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON);
        if (Build.VERSION.SDK_INT >= 30) {
            // A word processor wants the window to fit the system bars rather
            // than draw under them: the page has its own margins and a stray
            // tap on a cut-off toolbar is worse than a smaller canvas.
            getWindow().setDecorFitsSystemWindows(true);
        }
        installKeyboardSink();
    }

    // ---------------------------------------------------------------- pickers

    /** Show the open picker; {@code purpose} is the app's ({@code document},
     *  {@code picture}, {@code font}, {@code recipients}, {@code sources}). */
    public void pickOpen(final String purpose, final int id) {
        runOnUiThread(() -> {
            openId = id;
            openPurpose = purpose;
            Intent intent = new Intent(Intent.ACTION_OPEN_DOCUMENT);
            intent.addCategory(Intent.CATEGORY_OPENABLE);
            intent.setType("*/*");
            intent.putExtra(Intent.EXTRA_MIME_TYPES, mimesFor(purpose));
            if ("font".equals(purpose)) {
                intent.putExtra(Intent.EXTRA_ALLOW_MULTIPLE, true);
            }
            intent.addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION
                    | Intent.FLAG_GRANT_PERSISTABLE_URI_PERMISSION);
            try {
                startActivityForResult(intent, "font".equals(purpose) ? REQ_FONT : REQ_OPEN);
            } catch (Exception e) {
                nativePicked(id, new String[0]);
            }
        });
    }

    /** Show the create-document picker for a Save As / Export destination. */
    public void pickSave(final String name, final int id) {
        runOnUiThread(() -> {
            saveId = id;
            suggestedName = name;
            Intent intent = new Intent(Intent.ACTION_CREATE_DOCUMENT);
            intent.addCategory(Intent.CATEGORY_OPENABLE);
            intent.setType("application/octet-stream");
            intent.putExtra(Intent.EXTRA_TITLE, name);
            intent.addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION
                    | Intent.FLAG_GRANT_WRITE_URI_PERMISSION
                    | Intent.FLAG_GRANT_PERSISTABLE_URI_PERMISSION);
            try {
                startActivityForResult(intent, REQ_SAVE);
            } catch (Exception e) {
                nativePicked(id, new String[0]);
            }
        });
    }

    @Override
    protected void onActivityResult(int request, int result, Intent data) {
        super.onActivityResult(request, result, data);
        if (request == REQ_SAVE) {
            Uri uri = result == RESULT_OK && data != null ? data.getData() : null;
            if (uri == null) {
                nativePicked(saveId, new String[0]);
                return;
            }
            try {
                getContentResolver().takePersistableUriPermission(uri,
                        Intent.FLAG_GRANT_READ_URI_PERMISSION | Intent.FLAG_GRANT_WRITE_URI_PERMISSION);
            } catch (Exception ignored) {
                // Some providers only grant for this session; the write still works.
            }
            String name = suggestedName == null || suggestedName.isEmpty() ? "document.docx" : suggestedName;
            File staged = new File(mediaDir(), "save-" + System.nanoTime() + "-" + safeName(name));
            rememberUri(staged, uri);
            nativePicked(saveId, new String[]{staged.getAbsolutePath()});
            return;
        }
        if (request != REQ_FONT && request != REQ_OPEN) {
            return;
        }
        if (result != RESULT_OK || data == null) {
            nativePicked(openId, new String[0]);
            return;
        }
        List<Uri> uris = new ArrayList<>();
        ClipData clip = data.getClipData();
        if (clip != null) {
            for (int i = 0; i < clip.getItemCount(); i++) {
                Uri uri = clip.getItemAt(i).getUri();
                if (uri != null) {
                    uris.add(uri);
                }
            }
        } else if (data.getData() != null) {
            uris.add(data.getData());
        }
        List<String> staged = new ArrayList<>();
        for (Uri uri : uris) {
            String path = stage(uri);
            if (path != null) {
                staged.add(path);
            }
        }
        nativePicked(openId, staged.toArray(new String[0]));
    }

    private String[] mimesFor(String purpose) {
        if ("picture".equals(purpose)) {
            return new String[]{"image/*"};
        }
        if ("font".equals(purpose)) {
            return new String[]{"font/ttf", "font/otf", "font/ttc", "font/otc",
                    "application/font-sfnt", "application/x-font-ttf",
                    "application/x-font-opentype", "application/octet-stream"};
        }
        if ("sources".equals(purpose)) {
            return new String[]{"text/xml", "application/xml"};
        }
        if ("recipients".equals(purpose)) {
            return new String[]{"text/csv", "text/comma-separated-values",
                    "text/tab-separated-values", "text/plain",
                    "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
                    "application/vnd.ms-excel"};
        }
        return new String[]{
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
                "application/vnd.ms-word.document.macroenabled.12",
                "application/vnd.openxmlformats-officedocument.wordprocessingml.template",
                "application/vnd.ms-word.template.macroenabled.12",
                "application/vnd.oasis.opendocument.text",
                "application/rtf", "text/rtf", "text/plain", "text/markdown",
                "text/html", "application/pdf", "application/json",
                "application/octet-stream"};
    }

    // ------------------------------------------------------------- file access

    private File mediaDir() {
        File dir = new File(getFilesDir(), "media");
        if (!dir.isDirectory() && !dir.mkdirs()) {
            Toast.makeText(this, "WordCraft cannot use its own folder", Toast.LENGTH_LONG).show();
        }
        return dir;
    }

    /** Copy a picked document into the app's own folder; null when it can't be read. */
    private String stage(Uri uri) {
        String name = displayName(uri);
        File out = new File(mediaDir(), "open-" + System.nanoTime() + "-" + safeName(name));
        try (InputStream in = getContentResolver().openInputStream(uri);
             OutputStream os = new FileOutputStream(out)) {
            if (in == null) {
                return null;
            }
            byte[] buffer = new byte[64 * 1024];
            int read;
            while ((read = in.read(buffer)) > 0) {
                os.write(buffer, 0, read);
            }
        } catch (Exception e) {
            return null;
        }
        rememberUri(out, uri);
        return out.getAbsolutePath();
    }

    /** Write a staged copy back over the document it came from. */
    public void commitStaged(final String path) {
        final String stored = prefs().getString(URI_PREFIX + path, null);
        if (stored == null) {
            return;
        }
        final Uri uri = Uri.parse(stored);
        Thread writer = new Thread(() -> {
            try (InputStream in = new FileInputStream(path);
                 OutputStream os = getContentResolver().openOutputStream(uri, "wt")) {
                if (os == null) {
                    throw new IOException("the document is no longer writable");
                }
                byte[] buffer = new byte[64 * 1024];
                int read;
                while ((read = in.read(buffer)) > 0) {
                    os.write(buffer, 0, read);
                }
                os.flush();
                runOnUiThread(() -> Toast.makeText(this, "Saved", Toast.LENGTH_SHORT).show());
            } catch (Exception e) {
                runOnUiThread(() -> Toast.makeText(this,
                        "Couldn't write the document: " + e.getMessage(), Toast.LENGTH_LONG).show());
            }
        }, "wordcraft-save");
        writer.start();
    }

    private void rememberUri(File staged, Uri uri) {
        prefs().edit().putString(URI_PREFIX + staged.getAbsolutePath(), uri.toString()).apply();
    }

    private SharedPreferences prefs() {
        return getSharedPreferences(SAF, MODE_PRIVATE);
    }

    private String displayName(Uri uri) {
        try (Cursor cursor = getContentResolver().query(uri, new String[]{OpenableColumns.DISPLAY_NAME},
                null, null, null)) {
            if (cursor != null && cursor.moveToFirst()) {
                int column = cursor.getColumnIndex(OpenableColumns.DISPLAY_NAME);
                if (column >= 0) {
                    String name = cursor.getString(column);
                    if (name != null && !name.isEmpty()) {
                        return name;
                    }
                }
            }
        } catch (Exception ignored) {
            // Fall back to the URI's own last segment.
        }
        String last = uri.getLastPathSegment();
        return last == null ? "document" : last;
    }

    private static String safeName(String name) {
        StringBuilder safe = new StringBuilder();
        for (int i = 0; i < name.length() && safe.length() < 80; i++) {
            char c = name.charAt(i);
            boolean ok = (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z')
                    || (c >= '0' && c <= '9') || c == '.' || c == '-' || c == '_';
            safe.append(ok ? c : '_');
        }
        return safe.length() == 0 ? "document" : safe.toString();
    }

    // -------------------------------------------------------------- clipboard

    public String getClipboardText() {
        return onUi(() -> {
            ClipData clip = clipboard().getPrimaryClip();
            if (clip == null || clip.getItemCount() == 0) {
                return null;
            }
            CharSequence text = clip.getItemAt(0).coerceToText(this);
            return text == null ? null : text.toString();
        });
    }

    public String getClipboardHtml() {
        return onUi(() -> {
            ClipData clip = clipboard().getPrimaryClip();
            if (clip == null || clip.getItemCount() == 0) {
                return null;
            }
            String html = clip.getItemAt(0).getHtmlText();
            return html == null || html.isEmpty() ? null : html;
        });
    }

    public boolean setClipboardRich(String text, String html) {
        Boolean done = onUi(() -> {
            ClipData clip = html == null || html.isEmpty()
                    ? ClipData.newPlainText("WordCraft", text)
                    : ClipData.newHtmlText("WordCraft", text, html);
            clipboard().setPrimaryClip(clip);
            return Boolean.TRUE;
        });
        return Boolean.TRUE.equals(done);
    }

    public String[] clipboardFormats() {
        return new String[]{"text/plain", "text/html"};
    }

    private ClipboardManager clipboard() {
        return (ClipboardManager) getSystemService(Context.CLIPBOARD_SERVICE);
    }

    /** Run something that needs the UI thread (the clipboard service, mostly) from
     *  the render thread, and wait briefly for its answer. */
    private <T> T onUi(java.util.function.Supplier<T> work) {
        if (Looper.myLooper() == Looper.getMainLooper()) {
            return work.get();
        }
        final Object[] result = new Object[1];
        final java.util.concurrent.CountDownLatch done = new java.util.concurrent.CountDownLatch(1);
        runOnUiThread(() -> {
            result[0] = work.get();
            done.countDown();
        });
        try {
            done.await(800, java.util.concurrent.TimeUnit.MILLISECONDS);
        } catch (InterruptedException ignored) {
            Thread.currentThread().interrupt();
        }
        @SuppressWarnings("unchecked")
        T value = (T) result[0];
        return value;
    }

    // --------------------------------------------------------------- keyboard

    /**
     * A one-pixel transparent {@link EditText} that holds the input connection.
     * Its text is drained to the app as it changes: the difference from what was
     * already sent is typed (or deleted) in the editor. Persian and other
     * composing keyboards replace their composition as it is confirmed, which
     * shows up as the same delete-and-type difference.
     */
    private void installKeyboardSink() {
        imeSink = new EditText(this);
        imeSink.setAlpha(0f);
        imeSink.setBackgroundColor(0);
        imeSink.setSingleLine(false);
        imeSink.setFocusable(true);
        imeSink.setFocusableInTouchMode(true);
        imeSink.setInputType(InputType.TYPE_CLASS_TEXT | InputType.TYPE_TEXT_FLAG_MULTI_LINE);
        imeSink.setImeOptions(EditorInfo.IME_FLAG_NO_FULLSCREEN | EditorInfo.IME_FLAG_NO_EXTRACT_UI
                | EditorInfo.IME_ACTION_NONE);
        imeSink.addTextChangedListener(new TextWatcher() {
            @Override
            public void beforeTextChanged(CharSequence s, int start, int count, int after) {
            }

            @Override
            public void onTextChanged(CharSequence s, int start, int before, int count) {
            }

            @Override
            public void afterTextChanged(Editable editable) {
                drained(editable.toString());
            }
        });
        imeSink.setOnKeyListener((view, code, event) -> {
            if (event.getAction() != KeyEvent.ACTION_DOWN) {
                return false;
            }
            if (code == KeyEvent.KEYCODE_DEL) {
                nativeImeKey(67);
                return true;
            }
            if (code == KeyEvent.KEYCODE_ENTER) {
                nativeImeKey(66);
                return true;
            }
            return false;
        });
        addContentView(imeSink, new ViewGroup.LayoutParams(2, 2));
    }

    private void drained(String current) {
        if (current.equals(imeText)) {
            return;
        }
        int shared = 0;
        while (shared < imeText.length() && shared < current.length()
                && imeText.charAt(shared) == current.charAt(shared)) {
            shared++;
        }
        for (int i = shared; i < imeText.length(); i++) {
            nativeImeKey(67);
        }
        if (shared < current.length()) {
            nativeImeText(current.substring(shared));
        }
        imeText = current;
        // A long session would grow the sink without bound; nothing needs more
        // than the last few characters of it.
        if (current.length() > 256) {
            imeText = "";
            imeSink.setText("");
        }
    }

    /** Show or hide the soft keyboard (the app asks as text fields gain focus). */
    public void setKeyboardVisible(final boolean visible) {
        runOnUiThread(() -> {
            InputMethodManager imm = (InputMethodManager) getSystemService(Context.INPUT_METHOD_SERVICE);
            if (imm == null) {
                return;
            }
            if (visible) {
                imeSink.requestFocus();
                imm.showSoftInput(imeSink, 0);
            } else {
                imm.hideSoftInputFromWindow(imeSink.getWindowToken(), 0);
                imeSink.clearFocus();
                imeText = "";
                imeSink.setText("");
            }
        });
    }

    // ------------------------------------------------------------------- back

    @Override
    public void onBackPressed() {
        nativeBack();
        long now = System.currentTimeMillis();
        if (now - lastBack < 2500) {
            super.onBackPressed();
            return;
        }
        lastBack = now;
        Toast.makeText(this, "Press back again to leave WordCraft", Toast.LENGTH_SHORT).show();
    }

    // ----------------------------------------------------------------- natives

    private native void nativePicked(int id, String[] paths);

    private native void nativeImeText(String text);

    private native void nativeImeKey(int key);

    private native void nativeBack();
}
