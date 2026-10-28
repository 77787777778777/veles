package chat.janitor.veles;

import android.Manifest;
import android.app.NotificationManager;
import android.content.BroadcastReceiver;
import android.content.Context;
import android.content.Intent;
import android.content.IntentFilter;
import android.graphics.Bitmap;
import android.graphics.Rect;
import android.os.Handler;
import android.os.Looper;
import android.view.PixelCopy;
import android.content.pm.PackageManager;
import android.os.Build;
import android.os.Bundle;
import android.os.PowerManager;
import android.os.Process;
import android.service.notification.StatusBarNotification;
import android.util.Log;
import android.system.ErrnoException;
import android.system.Os;
import android.view.KeyEvent;
import android.view.View;
import android.view.ViewConfiguration;
import android.view.WindowInsets;
import android.window.OnBackInvokedDispatcher;
import java.io.File;
import java.io.FileOutputStream;
import java.nio.charset.StandardCharsets;
import org.libsdl.app.SDLActivity;

/**
 * The desktop frontend: veles-desktop's Tcl/Tk tree, run by AndroWish.
 * AndroWish mounts the APK at /assets and runs /assets/app/main.tcl
 * (android/desktop.tcl), which starts the desktop against this app's
 * gateway.
 *
 * Opening it starts the gateway, as `veles gui` does on a laptop: the
 * window needs one. The engine stops on its own five minutes after
 * anyone last used it, so the app starts it on every open.
 *
 * It runs in its own process (the manifest's `:desktop`). Tcl's `exit`
 * ends the process it runs in, and the gateway's service must not go
 * with it.
 *
 * AndroWish runs Tcl once per process. So the manifest names every
 * configuration change (an undeclared one recreates the activity), a
 * finished desktop ends its process, and an open that finds the process
 * already used goes through Relauncher to a fresh one.
 */
public final class DesktopActivity extends tk.tcl.wish.AndroWish {
    /**
     * A picture of part of the window, for the agent's {@code app
     * screenshot} (veles-desktop/lib/apps.tcl): Tk has no way to read its
     * own pixels back, Android can. The desktop sends this broadcast
     * ({@code borg broadcast send}) with x, y, w, h in surface pixels and
     * the path to write the PNG to; registered not-exported, so only this
     * app can ask, and only a path inside its own files is written.
     */
    static final String SNAP = "chat.janitor.veles.SNAP";
    private static final String TAG = "veles-desktop";
    private final BroadcastReceiver snapper = new BroadcastReceiver() {
        @Override
        public void onReceive(Context c, Intent i) {
            snap(i);
        }
    };
    private boolean snapping;
    private boolean stale;
    /** Between onPause and onResume: the padding holds still (onCreate's listener). */
    private boolean paused;

    @Override
    public void onCreate(Bundle saved) {
        // AndroWish's onCreate finishes a second instance on its own;
        // this one has to be noticed before it does.
        stale = mSingleton != null;
        startForegroundService(new Intent(this, GatewayService.class));
        // A long press as this phone has it, for AndroWish's touch
        // translation (android/androwish/touch.patch): how long a finger
        // is held (the user's "touch and hold delay") and how far it may
        // wander meanwhile. Read before Tk starts, in super.onCreate.
        try {
            Os.setenv("SDLTK_LONGPRESS_MS",
                    Integer.toString(ViewConfiguration.getLongPressTimeout()), true);
            Os.setenv("SDLTK_TOUCH_SLOP",
                    Integer.toString(ViewConfiguration.get(this).getScaledTouchSlop()), true);
        } catch (ErrnoException e) {
            // sdl2tk's own defaults.
        }
        super.onCreate(saved);
        if (stale) {
            startActivity(new Intent(this, Relauncher.class)
                    .putExtra(Relauncher.EXTRA_INTENT, getIntent()));
            finish();
            Process.killProcess(Process.myPid());
            return;
        }
        clearSessionNotifications(getIntent());
        // Back is Tk's to decide (lib/android.tcl's back: the menu, the
        // drawer, the window on top, else the background). Predictive back
        // (Android 16 for this targetSdk, the manifest's opt-in before)
        // never shows the key to SDL, and Android's own answer finished
        // the activity, which ended Tcl: the desktop quit on Back
        // (emulator, 2026-09-30). Below Android 13 SDL's key listener
        // hands it over itself.
        if (Build.VERSION.SDK_INT >= 33) {
            getOnBackInvokedDispatcher().registerOnBackInvokedCallback(
                    OnBackInvokedDispatcher.PRIORITY_DEFAULT, DesktopActivity::toTk);
        }
        if (Build.VERSION.SDK_INT >= 33) {
            registerReceiver(snapper, new IntentFilter(SNAP), Context.RECEIVER_NOT_EXPORTED);
        } else {
            registerReceiver(snapper, new IntentFilter(SNAP));
        }
        snapping = true;
        // The desktop is the entry people open, so it asks for what the
        // gateway's notifications need (Android 13 on), as MainActivity
        // does.
        if (Build.VERSION.SDK_INT >= 33
                && checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS)
                        != PackageManager.PERMISSION_GRANTED) {
            requestPermissions(new String[] {Manifest.permission.POST_NOTIFICATIONS}, 1);
        }
        // The keyboard covers the window instead of shrinking it: from
        // Android 15 every app is edge to edge, and a fullscreen window
        // was never resized for it anyway. So the content pads itself by
        // the keyboard and the navigation bar. SDL's surface shrinks with
        // it and Tk lays the desktop out above the keyboard, where the
        // composer is (emulator, 2026-09-30: it was under the keyboard).
        //
        // Not while paused: leaving with the keyboard up resized the
        // surface twice after SDL had paused (the keyboard going, then
        // its inset coming back), and SDL resumed onto a surface it never
        // drew again, a black screen (emulator, 2026-09-30). The padding
        // catches up on resume, as an ordinary resize.
        View content = findViewById(android.R.id.content);
        content.setOnApplyWindowInsetsListener((v, insets) -> {
            if (paused) {
                return insets;
            }
            int bottom;
            if (Build.VERSION.SDK_INT >= 30) {
                bottom = insets.getInsets(WindowInsets.Type.ime() | WindowInsets.Type.navigationBars()).bottom;
            } else {
                bottom = insets.getSystemWindowInsetBottom();
            }
            if (v.getPaddingBottom() != bottom) {
                v.setPadding(0, 0, 0, bottom);
            }
            return insets;
        });
    }

    @Override
    public void onPause() {
        paused = true;
        super.onPause();
    }

    @Override
    public void onNewIntent(Intent i) {
        super.onNewIntent(i);
        clearSessionNotifications(i);
    }

    /**
     * A notification opened the window on its session: that session's
     * ask and turn notifications go, since the window shows them now. A
     * tap cancels the one notification tapped, but Android 16 bundles an
     * app's notifications into a group of its own, and a tap on the
     * bundle opened veles while the ask itself stayed (the emulator
     * smoke's ask-tap-clears-it: posted 5 s before the tap, still there
     * after, the same ask; 2026-10-12).
     */
    private void clearSessionNotifications(Intent i) {
        String session = i == null ? null : i.getStringExtra("session");
        if (session == null || session.isEmpty()) {
            return;
        }
        NotificationManager nm = getSystemService(NotificationManager.class);
        if (nm == null) {
            return;
        }
        int seen = 0, gone = 0;
        try {
            for (StatusBarNotification sbn : nm.getActiveNotifications()) {
                seen++;
                String tag = sbn.getTag();
                if (("ask".equals(tag) || "turn".equals(tag)) && session.equals(
                        sbn.getNotification().extras.getString(Notifier.EXTRA_SESSION))) {
                    nm.cancel(tag, sbn.getId());
                    gone++;
                }
            }
        } catch (RuntimeException e) {
            Log.w(TAG, "notifications of " + session + ": " + e);
        }
        Log.i(TAG, "opened on session " + session + ": took down " + gone + " of "
                + seen + " notifications");
    }

    @Override
    public void onResume() {
        super.onResume();
        paused = false;
        // The engine stops five minutes after anyone last used it, and
        // bringing the desktop back is not a new onCreate: start it on
        // every resume. A running one ignores the start.
        startForegroundService(new Intent(this, GatewayService.class));
        writeStatus();
        View content = findViewById(android.R.id.content);
        if (content != null) {
            content.requestApplyInsets();
        }
    }

    /**
     * What only Java can see, for the desktop's Phone page
     * (veles-desktop/lib/phone.tcl): whether Android shows veles'
     * notifications, whether it lets veles run in the background, the
     * location grant, the maker, free space. Written on every resume, so
     * coming back from Android's settings refreshes the page. One
     * `key=value` per line; written aside and renamed, so the page never
     * reads half a file.
     */
    private void writeStatus() {
        try {
            NotificationManager nm = getSystemService(NotificationManager.class);
            PowerManager pm = getSystemService(PowerManager.class);
            String location = "off";
            if (checkSelfPermission(Manifest.permission.ACCESS_FINE_LOCATION)
                    == PackageManager.PERMISSION_GRANTED) {
                location = "precise";
            } else if (checkSelfPermission(Manifest.permission.ACCESS_COARSE_LOCATION)
                    == PackageManager.PERMISSION_GRANTED) {
                location = "approximate";
            }
            File files = getFilesDir();
            String text = "notifications=" + (nm != null && nm.areNotificationsEnabled() ? 1 : 0) + "\n"
                    + "background=" + (pm != null && pm.isIgnoringBatteryOptimizations(getPackageName()) ? 1 : 0) + "\n"
                    + "location=" + location + "\n"
                    + "listener=" + (NotificationListener.granted(this) ? 1 : 0) + "\n"
                    + "boot=" + (WakeReceiver.bootFlag(this).exists() ? 1 : 0) + "\n"
                    // The accessibility driver: absent from the store's
                    // build, else whether the person turned it on.
                    + "driver=" + (!AccessibilityDriver.declared(this) ? "absent"
                            : AccessibilityDriver.enabled(this) ? "1" : "0") + "\n"
                    + "maker=" + Build.MANUFACTURER.replace('\n', ' ') + "\n"
                    + "sdk=" + Build.VERSION.SDK_INT + "\n"
                    + "free=" + files.getUsableSpace() + "\n"
                    + "total=" + files.getTotalSpace() + "\n"
                    + "package=" + getPackageName() + "\n";
            File tmp = new File(files, "phone-status.tmp");
            try (FileOutputStream out = new FileOutputStream(tmp)) {
                out.write(text.getBytes(StandardCharsets.UTF_8));
            }
            if (!tmp.renameTo(new File(files, "phone-status"))) {
                tmp.delete();
            }
        } catch (Exception e) {
            // The page says it does not know.
        }
    }

    /** Back, as AndroWish's Tk knows it: the Break key. */
    private static void toTk() {
        SDLActivity.onNativeKeyDown(KeyEvent.KEYCODE_BACK);
        SDLActivity.onNativeKeyUp(KeyEvent.KEYCODE_BACK);
    }

    private static int intExtra(Intent i, String key) {
        try {
            return Integer.parseInt(String.valueOf(i.getStringExtra(key)).trim());
        } catch (NumberFormatException e) {
            return -1;
        }
    }

    /**
     * What borg's {@code broadcast send} sends to veles itself (an action
     * under this package's name, such as {@link #SNAP}) is addressed to
     * this package. Android dispatched the implicit SNAP to no receiver
     * at all, though this activity had one registered not-exported (the
     * real phone, Android 16, 2026-10-12: sent, dispatched to none), and
     * borg has no slot for a package.
     */
    @Override
    public void sendBroadcast(Intent i) {
        String act = i.getAction();
        if (act != null && act.startsWith(getPackageName() + ".")
                && i.getPackage() == null && i.getComponent() == null) {
            i.setPackage(getPackageName());
        }
        super.sendBroadcast(i);
    }

    /**
     * Copy the asked-for part of the SDL surface into a PNG. A refusal
     * writes its reason beside it ({@code <path>.err}), which the desktop
     * reads, so a picture that cannot be taken says why.
     */
    private void snap(Intent i) {
        final String path = i.getStringExtra("path");
        final File out;
        try {
            if (path == null) {
                Log.w(TAG, "snap: no path");
                return;
            }
            out = new File(path).getCanonicalFile();
            if (!out.getPath().startsWith(getFilesDir().getCanonicalPath() + File.separator)) {
                Log.w(TAG, "snap: refused a path outside the app's files: " + path);
                return;
            }
        } catch (java.io.IOException e) {
            Log.w(TAG, "snap: " + e);
            return;
        }
        int x = intExtra(i, "x"), y = intExtra(i, "y"), w = intExtra(i, "w"), h = intExtra(i, "h");
        if (mSurface == null) {
            snapFailed(out, "the window has no surface");
            return;
        }
        if (x < 0 || y < 0 || w <= 0 || h <= 0 || w > 8192 || h > 8192) {
            snapFailed(out, "not an area of the window: " + x + "," + y + " " + w + "x" + h);
            return;
        }
        final Bitmap bm = Bitmap.createBitmap(w, h, Bitmap.Config.ARGB_8888);
        final BroadcastReceiver.PendingResult pending = snapper.goAsync();
        try {
            PixelCopy.request(mSurface, new Rect(x, y, x + w, y + h), bm, result -> {
                try {
                    if (result == PixelCopy.SUCCESS) {
                        File tmp = new File(out.getPath() + ".tmp");
                        try (FileOutputStream f = new FileOutputStream(tmp)) {
                            bm.compress(Bitmap.CompressFormat.PNG, 100, f);
                        }
                        if (!tmp.renameTo(out)) {
                            snapFailed(out, "could not write the picture");
                        }
                    } else {
                        snapFailed(out, "PixelCopy " + result);
                    }
                } catch (java.io.IOException e) {
                    snapFailed(out, e.toString());
                } finally {
                    pending.finish();
                }
            }, new Handler(Looper.getMainLooper()));
        } catch (RuntimeException e) {
            // PixelCopy refuses a surface that is gone, or an area beyond it.
            snapFailed(out, e.toString());
            pending.finish();
        }
    }

    private static void snapFailed(File out, String why) {
        Log.w(TAG, "snap: " + why);
        try (FileOutputStream f = new FileOutputStream(new File(out.getPath() + ".err"))) {
            f.write(why.getBytes(StandardCharsets.UTF_8));
        } catch (java.io.IOException e) {
            // The desktop times out and says so.
        }
    }

    @Override
    protected void onDestroy() {
        if (snapping) {
            snapping = false;
            try {
                unregisterReceiver(snapper);
            } catch (IllegalArgumentException e) {
                // Never registered (a stale instance).
            }
        }
        super.onDestroy();
        // Finished (Tcl's `exit`, or Android closing it): the next open
        // must find a process that has not run Tcl yet. A recreation keeps
        // the process, and the new instance's onCreate relaunches.
        if (isFinishing() && !stale) {
            Process.killProcess(Process.myPid());
        }
    }
}
