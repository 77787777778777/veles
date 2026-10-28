package chat.janitor.veles;

import android.app.Activity;
import android.app.ActivityManager;
import android.content.Intent;
import android.os.Bundle;
import android.os.Handler;
import android.os.Looper;

/**
 * Opens the desktop again in a fresh process.
 *
 * AndroWish runs Tcl once per process: a second DesktopActivity in a
 * process that already ran one finishes at once (AndroWish.onCreate's
 * singleton check), and every later open does the same until the process
 * dies. DesktopActivity sends a stale open here, in the gateway's process,
 * and ends its own; this waits until `:desktop` is gone and opens the
 * desktop, which Android then starts in a new process.
 *
 * Emulator, 2026-09-30: the first open after an install was relaunched
 * by Android for a configuration change the manifest did not name
 * (assetsPaths), and the desktop never appeared.
 */
public final class Relauncher extends Activity {
    static final String EXTRA_INTENT = "chat.janitor.veles.extra.INTENT";
    private static final int POLL_MS = 100;
    private static final int POLLS = 50;

    private final Handler ui = new Handler(Looper.getMainLooper());
    private int polls;

    @Override
    protected void onCreate(Bundle saved) {
        super.onCreate(saved);
        ui.post(this::openWhenGone);
    }

    private void openWhenGone() {
        if (desktopRunning() && polls++ < POLLS) {
            ui.postDelayed(this::openWhenGone, POLL_MS);
            return;
        }
        Intent open = new Intent(this, DesktopActivity.class);
        @SuppressWarnings("deprecation")
        Intent was = getIntent().getParcelableExtra(EXTRA_INTENT);
        if (was != null && was.getExtras() != null) {
            open.putExtras(was.getExtras());
        }
        startActivity(open);
        finish();
        overridePendingTransition(0, 0);
    }

    private boolean desktopRunning() {
        String name = getPackageName() + ":desktop";
        ActivityManager am = getSystemService(ActivityManager.class);
        if (am.getRunningAppProcesses() == null) {
            return false;
        }
        for (ActivityManager.RunningAppProcessInfo p : am.getRunningAppProcesses()) {
            if (name.equals(p.processName)) {
                return true;
            }
        }
        return false;
    }
}
