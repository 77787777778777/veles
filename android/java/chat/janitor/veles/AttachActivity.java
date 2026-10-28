package chat.janitor.veles;

import android.app.Activity;
import android.content.ClipData;
import android.content.Intent;
import android.database.Cursor;
import android.net.Uri;
import android.os.Bundle;
import android.provider.OpenableColumns;
import java.io.File;
import java.io.FileOutputStream;
import java.io.InputStream;
import java.io.OutputStream;
import java.util.ArrayList;
import java.util.List;

/**
 * The desktop's 📎 on a phone: Android's own file picker, whose files
 * are then copied where the app can read them by path, and handed back
 * to the desktop (its intent's "attach" extra, one path a line; the
 * desktop's `borg onintent` attaches them to the next message).
 *
 * Tk's file dialog cannot do this: it opened at `/`, and a phone's
 * photos and downloads are behind Android's storage access, which gives
 * an app a content URI, not a path (emulator, 2026-09-30).
 *
 * The copies go to the app's cache, which Android may clear when space
 * runs low: an attachment is read when its message is sent.
 */
public final class AttachActivity extends Activity {
    static final String EXTRA_ATTACH = "attach";
    static final String EXTRA_NONCE = "attach_nonce";
    private static final int PICK = 1;

    @Override
    protected void onCreate(Bundle saved) {
        super.onCreate(saved);
        if (saved != null) {
            return;
        }
        // Mint the word the desktop will demand of the "attach" intent,
        // BEFORE the picker can return. A relaunched picker replaces it.
        AttachNonce.mint();
        Intent pick = new Intent(Intent.ACTION_OPEN_DOCUMENT)
                .addCategory(Intent.CATEGORY_OPENABLE)
                .setType("*/*")
                .putExtra(Intent.EXTRA_ALLOW_MULTIPLE, true);
        startActivityForResult(pick, PICK);
    }

    @Override
    protected void onActivityResult(int request, int result, Intent data) {
        if (request != PICK || result != RESULT_OK || data == null) {
            AttachNonce.clear();
            finish();
            return;
        }
        List<Uri> picked = new ArrayList<>();
        ClipData clip = data.getClipData();
        if (clip != null) {
            for (int i = 0; i < clip.getItemCount(); i++) {
                picked.add(clip.getItemAt(i).getUri());
            }
        } else if (data.getData() != null) {
            picked.add(data.getData());
        }
        // Off the UI thread: a video is not copied in the time Android
        // gives a window to answer.
        new Thread(() -> {
            List<String> paths = new ArrayList<>();
            for (Uri u : picked) {
                String p = copy(u);
                if (p != null) {
                    paths.add(p);
                }
            }
            runOnUiThread(() -> {
                // The word stays set: the desktop reads it when the intent
                // reaches Tcl, after this (AttachNonce.java), and spends it.
                if (!paths.isEmpty()) {
                    startActivity(new Intent(this, DesktopActivity.class)
                            .putExtra(EXTRA_ATTACH, String.join("\n", paths))
                            .putExtra(EXTRA_NONCE, AttachNonce.nonce));
                } else {
                    AttachNonce.clear();
                }
                finish();
            });
        }).start();
    }

    /** The picked file, copied under its own name; null when it cannot be read. */
    private String copy(Uri uri) {
        File dir = new File(getCacheDir(), "attachments");
        if (!dir.isDirectory() && !dir.mkdirs()) {
            return null;
        }
        File out = unique(dir, name(uri));
        try (InputStream in = getContentResolver().openInputStream(uri);
                OutputStream o = new FileOutputStream(out)) {
            if (in == null) {
                return null;
            }
            byte[] buf = new byte[64 << 10];
            for (int n; (n = in.read(buf)) > 0; ) {
                o.write(buf, 0, n);
            }
            return out.getAbsolutePath();
        } catch (Exception e) {
            out.delete();
            return null;
        }
    }

    /** The name the provider shows for it, as a plain file name. */
    private String name(Uri uri) {
        String name = null;
        try (Cursor c = getContentResolver().query(
                uri, new String[] {OpenableColumns.DISPLAY_NAME}, null, null, null)) {
            if (c != null && c.moveToFirst()) {
                name = c.getString(0);
            }
        } catch (Exception e) {
            name = null;
        }
        if (name == null) {
            name = uri.getLastPathSegment();
        }
        if (name == null) {
            name = "attachment";
        }
        name = name.replaceAll("[/\\\\\\x00-\\x1f]", "_");
        if (name.isEmpty() || name.equals(".") || name.equals("..")) {
            name = "attachment";
        }
        return name;
    }

    /** dir/name, or dir/name-2, -3… when a file of that name is there. */
    private static File unique(File dir, String name) {
        File f = new File(dir, name);
        int dot = name.lastIndexOf('.');
        String stem = dot > 0 ? name.substring(0, dot) : name;
        String ext = dot > 0 ? name.substring(dot) : "";
        for (int i = 2; f.exists(); i++) {
            f = new File(dir, stem + "-" + i + ext);
        }
        return f;
    }
}
