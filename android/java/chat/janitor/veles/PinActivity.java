package chat.janitor.veles;

import android.app.Activity;
import android.content.Intent;
import android.content.pm.ShortcutInfo;
import android.content.pm.ShortcutManager;
import android.graphics.Bitmap;
import android.graphics.BitmapFactory;
import android.graphics.drawable.Icon;
import android.net.Uri;
import android.os.Bundle;
import android.util.Base64;
import android.util.Log;

/**
 * Pins one of the person's apps to the home screen (PLAN-sessions-and-
 * phone.md A4, USER decision D5). The launcher's "Pin to home screen"
 * starts this with the app's name, label and icon; Android shows its own
 * "add to home screen?" and the shortcut it makes opens
 * {@code veles-app://<name>} in the desktop, which runs that app.
 *
 * AndroWish's own {@code borg shortcut add} cannot do this here: its
 * shortcut targets {@code tk.tcl.wish.AndroWishLauncher}, a class this
 * app does not have.
 *
 * The shortcut carries a name and nothing else; the desktop checks the
 * name against the apps it has and runs it under the person's grants, so
 * a shortcut can open an app but never widen one.
 */
public final class PinActivity extends Activity {
    private static final String TAG = "veles-pin";

    @Override
    protected void onCreate(Bundle saved) {
        super.onCreate(saved);
        try {
            pin(getIntent());
        } catch (Exception e) {
            Log.w(TAG, "pin failed", e);
        }
        finish();
    }

    private void pin(Intent in) {
        String name = in.getStringExtra("name");
        if (name == null || !name.matches("[a-z0-9][a-z0-9-]{0,31}")) {
            Log.w(TAG, "not an app name: " + name);
            return;
        }
        String label = in.getStringExtra("label");
        if (label == null || label.isEmpty()) {
            label = name;
        }
        ShortcutManager sm = getSystemService(ShortcutManager.class);
        if (sm == null || !sm.isRequestPinShortcutSupported()) {
            Log.w(TAG, "this launcher cannot pin shortcuts");
            return;
        }
        Intent open = new Intent(Intent.ACTION_VIEW, Uri.parse("veles-app://" + name))
                .setClass(this, DesktopActivity.class)
                .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK | Intent.FLAG_ACTIVITY_CLEAR_TOP);
        Icon icon = null;
        String b64 = in.getStringExtra("icon");
        if (b64 != null && !b64.isEmpty()) {
            try {
                byte[] png = Base64.decode(b64, Base64.DEFAULT);
                Bitmap bm = BitmapFactory.decodeByteArray(png, 0, png.length);
                if (bm != null) {
                    icon = Icon.createWithBitmap(bm);
                }
            } catch (IllegalArgumentException e) {
                // The app's own icon is optional; veles' stands in.
            }
        }
        if (icon == null) {
            icon = Icon.createWithResource(this, R.mipmap.ic_launcher);
        }
        ShortcutInfo info = new ShortcutInfo.Builder(this, "app-" + name)
                .setShortLabel(label.length() > 24 ? label.substring(0, 24) : label)
                .setLongLabel(label)
                .setIcon(icon)
                .setIntent(open)
                .build();
        sm.requestPinShortcut(info, null);
    }
}
