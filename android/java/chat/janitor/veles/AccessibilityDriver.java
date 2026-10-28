package chat.janitor.veles;

import android.accessibilityservice.AccessibilityService;
import android.accessibilityservice.AccessibilityServiceInfo;
import android.accessibilityservice.GestureDescription;
import android.app.Notification;
import android.app.NotificationChannel;
import android.app.NotificationManager;
import android.app.PendingIntent;
import android.content.BroadcastReceiver;
import android.content.Context;
import android.content.Intent;
import android.content.IntentFilter;
import android.content.pm.PackageManager;
import android.content.pm.ResolveInfo;
import android.graphics.Bitmap;
import android.graphics.Canvas;
import android.graphics.Color;
import android.graphics.Paint;
import android.graphics.Path;
import android.graphics.Rect;
import android.graphics.drawable.Icon;
import android.os.Build;
import android.os.Bundle;
import android.os.Handler;
import android.os.Looper;
import android.util.DisplayMetrics;
import android.util.Log;
import android.view.Display;
import android.view.accessibility.AccessibilityEvent;
import android.view.accessibility.AccessibilityNodeInfo;
import android.view.accessibility.AccessibilityWindowInfo;

import org.json.JSONArray;
import org.json.JSONException;
import org.json.JSONObject;

import java.io.BufferedInputStream;
import java.io.ByteArrayOutputStream;
import java.io.File;
import java.io.FileOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.net.InetAddress;
import java.net.ServerSocket;
import java.net.Socket;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.List;
import java.util.Locale;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicReference;

/**
 * The agent's hands on the phone (PLAN-sessions-and-phone.md A3, round P5;
 * USER decision D4): an accessibility service that speaks the protocol of
 * cua-driver, so veles' `computer_use` tool drives the phone unchanged —
 * {@code POST http://127.0.0.1:9876/action} with the tool's own arguments.
 *
 * <ul>
 * <li>Only in the sideload build: the Play bundle's manifest leaves it out
 * (flake.nix, velesAab) and its build fails if it is still there.
 * <li>Off until the person turns it on in Android's accessibility settings
 * (the Phone page's "Using the phone, for the agent" opens them).
 * <li>Only veles on this phone reaches it: it listens on loopback and
 * answers only a request bearing the gateway's own bearer token (the
 * owner's, which `computer_use` sends — and on a phone the tool refuses
 * every session that is not the owner's).
 * <li>While the agent drives, an ongoing notification says so, with Stop,
 * which turns the service off (the person turns it on again in settings).
 * </ul>
 *
 * {@code capture} lists what can be acted on, numbered, as text
 * ({@code ax_text}), and saves a screenshot with the numbers drawn on it
 * ({@code screenshot_path}, mode {@code som}) or plain ({@code vision}).
 * The other actions take an element's number from the last capture or a
 * screen coordinate.
 */
public final class AccessibilityDriver extends AccessibilityService {
    private static final String TAG = "veles-driver";
    static final int PORT = 9876;
    private static final String CHANNEL = "driving";
    private static final int NOTIFICATION = 4;
    static final String ACTION_STOP = "chat.janitor.veles.action.STOP_DRIVING";
    /** The notification goes this long after the agent's last action. */
    private static final long DRIVING_MS = 30_000;

    private final Handler main = new Handler(Looper.getMainLooper());
    private volatile ServerSocket server;
    /** The elements of the last capture, by number - 1. Guarded by this. */
    private final List<AccessibilityNodeInfo> elements = new ArrayList<>();
    private final Runnable quiet = () -> {
        NotificationManager nm = getSystemService(NotificationManager.class);
        if (nm != null) {
            nm.cancel(NOTIFICATION);
        }
    };
    private final BroadcastReceiver stop = new BroadcastReceiver() {
        @Override
        public void onReceive(Context c, Intent i) {
            Log.i(TAG, "the person stopped the driver");
            quiet.run();
            disableSelf();
        }
    };

    /** Is the driver declared in this build (the sideload APK)? */
    static boolean declared(Context c) {
        try {
            c.getPackageManager().getServiceInfo(
                    new android.content.ComponentName(c, AccessibilityDriver.class), 0);
            return true;
        } catch (PackageManager.NameNotFoundException e) {
            return false;
        }
    }

    /** Has the person turned it on? */
    static boolean enabled(Context c) {
        String on = android.provider.Settings.Secure.getString(c.getContentResolver(),
                android.provider.Settings.Secure.ENABLED_ACCESSIBILITY_SERVICES);
        return on != null && on.contains(c.getPackageName() + "/"
                + AccessibilityDriver.class.getName());
    }

    @Override
    protected void onServiceConnected() {
        NotificationChannel ch = new NotificationChannel(CHANNEL,
                getString(R.string.channel_driving), NotificationManager.IMPORTANCE_LOW);
        getSystemService(NotificationManager.class).createNotificationChannel(ch);
        if (Build.VERSION.SDK_INT >= 33) {
            registerReceiver(stop, new IntentFilter(ACTION_STOP), Context.RECEIVER_NOT_EXPORTED);
        } else {
            registerReceiver(stop, new IntentFilter(ACTION_STOP));
        }
        if (Build.VERSION.SDK_INT < 33) {
            // No clearCache() before Android 13 (fresh()): take the events
            // that keep Android's copy of the screen current instead.
            AccessibilityServiceInfo info = getServiceInfo();
            info.eventTypes |= AccessibilityEvent.TYPE_WINDOW_CONTENT_CHANGED
                    | AccessibilityEvent.TYPE_VIEW_SCROLLED
                    | AccessibilityEvent.TYPE_WINDOWS_CHANGED;
            setServiceInfo(info);
        }
        Thread t = new Thread(this::serve, "veles-driver");
        t.setDaemon(true);
        t.start();
    }

    @Override
    public void onDestroy() {
        ServerSocket s = server;
        server = null;
        if (s != null) {
            try {
                s.close();
            } catch (IOException ignored) {
                // Closing.
            }
        }
        try {
            unregisterReceiver(stop);
        } catch (IllegalArgumentException ignored) {
            // Never registered.
        }
        quiet.run();
        super.onDestroy();
    }

    @Override
    public void onAccessibilityEvent(AccessibilityEvent e) {
        // Nothing to follow: the agent asks when it wants to see.
    }

    @Override
    public void onInterrupt() {
    }

    // ── the loopback server ─────────────────────────────────────────

    private void serve() {
        try (ServerSocket s = new ServerSocket(PORT, 8, InetAddress.getByName("127.0.0.1"))) {
            server = s;
            Log.i(TAG, "listening on 127.0.0.1:" + PORT);
            while (server != null) {
                Socket c;
                try {
                    c = s.accept();
                } catch (IOException e) {
                    break;
                }
                new Thread(() -> handle(c), "veles-driver-call").start();
            }
        } catch (IOException e) {
            Log.w(TAG, "cannot listen on " + PORT + ": " + e);
        }
    }

    private void handle(Socket c) {
        try (Socket sock = c) {
            sock.setSoTimeout(60_000);
            InputStream in = new BufferedInputStream(sock.getInputStream());
            String request = line(in);
            String auth = "";
            int length = 0;
            for (String h; !(h = line(in)).isEmpty(); ) {
                int colon = h.indexOf(':');
                if (colon < 0) {
                    continue;
                }
                String k = h.substring(0, colon).trim().toLowerCase(Locale.ROOT);
                String v = h.substring(colon + 1).trim();
                if (k.equals("authorization")) {
                    auth = v;
                } else if (k.equals("content-length")) {
                    length = Integer.parseInt(v);
                }
            }
            if (length < 0 || length > 1 << 20) {
                reply(sock, 413, error("too large"));
                return;
            }
            byte[] body = new byte[length];
            for (int off = 0; off < length; ) {
                int n = in.read(body, off, length - off);
                if (n < 0) {
                    break;
                }
                off += n;
            }
            String token = Engine.bearer(this);
            if (token.isEmpty() || !auth.equals("Bearer " + token)) {
                reply(sock, 401, error("only veles on this phone, with its token, may drive it"));
                return;
            }
            if (!request.startsWith("POST /action ")) {
                reply(sock, 404, error("POST /action is the one route"));
                return;
            }
            JSONObject args = new JSONObject(new String(body, StandardCharsets.UTF_8));
            driving();
            JSONObject out;
            try {
                out = act(args);
            } catch (IllegalArgumentException | IllegalStateException e) {
                reply(sock, 400, error(String.valueOf(e.getMessage())));
                return;
            }
            reply(sock, 200, out);
        } catch (IOException | JSONException | RuntimeException e) {
            Log.w(TAG, "a call failed: " + e);
        }
    }

    private static String line(InputStream in) throws IOException {
        ByteArrayOutputStream b = new ByteArrayOutputStream();
        for (int ch; (ch = in.read()) >= 0 && ch != '\n'; ) {
            if (ch != '\r') {
                b.write(ch);
            }
            if (b.size() > 8192) {
                throw new IOException("a header line too long");
            }
        }
        return b.toString("UTF-8");
    }

    private static JSONObject error(String why) {
        try {
            return new JSONObject().put("error", why);
        } catch (JSONException e) {
            return new JSONObject();
        }
    }

    private static void reply(Socket s, int code, JSONObject body) throws IOException {
        byte[] b = body.toString().getBytes(StandardCharsets.UTF_8);
        OutputStream o = s.getOutputStream();
        o.write(("HTTP/1.1 " + code + (code == 200 ? " OK" : " Error") + "\r\n"
                + "Content-Type: application/json\r\nContent-Length: " + b.length
                + "\r\nConnection: close\r\n\r\n").getBytes(StandardCharsets.US_ASCII));
        o.write(b);
        o.flush();
    }

    /** "The agent is using this phone", with Stop; gone 30 s after the last action. */
    private void driving() {
        main.post(() -> {
            PendingIntent stopIt = PendingIntent.getBroadcast(this, 0,
                    new Intent(ACTION_STOP).setPackage(getPackageName()),
                    PendingIntent.FLAG_IMMUTABLE);
            String text = getString(R.string.notify_driving);
            Notification n = new Notification.Builder(this, CHANNEL)
                    .setSmallIcon(R.drawable.ic_notification)
                    .setContentTitle(getString(R.string.app_name))
                    .setContentText(text)
                    .setOngoing(true)
                    .setOnlyAlertOnce(true)
                    .addAction(new Notification.Action.Builder(
                            Icon.createWithResource(this, R.drawable.ic_notification),
                            getString(R.string.action_stop), stopIt).build())
                    .build();
            getSystemService(NotificationManager.class).notify(NOTIFICATION, n);
            main.removeCallbacks(quiet);
            main.postDelayed(quiet, DRIVING_MS);
        });
    }

    // ── the actions ──────────────────────────────────────────────────

    private JSONObject act(JSONObject a) throws JSONException {
        String action = a.optString("action");
        JSONObject out;
        switch (action) {
            case "capture":
                return capture(a.optString("mode", "som"));
            case "click":
            case "double_click":
            case "right_click":
            case "middle_click":
                out = click(a, action);
                break;
            case "type":
                out = type(a);
                break;
            case "key":
                out = key(a.optString("keys"));
                break;
            case "scroll":
                out = scroll(a);
                break;
            case "drag":
                out = drag(a);
                break;
            case "list_apps":
                out = listApps(a.optString("app"));
                break;
            case "list_windows":
                out = listWindows();
                break;
            case "focus_app":
                out = focusApp(a.optString("app"));
                break;
            case "wait":
                sleep((long) (Math.min(30, Math.max(0, a.optDouble("seconds", 1))) * 1000));
                out = result("waited");
                break;
            default:
                throw new IllegalArgumentException("unknown action " + action);
        }
        if (a.optBoolean("capture_after")) {
            sleep(600);
            JSONObject c = capture(a.optString("mode", "som"));
            c.put("result", out.optString("result"));
            return c;
        }
        return out;
    }

    private static JSONObject result(String s) throws JSONException {
        return new JSONObject().put("result", s);
    }

    private static void sleep(long ms) {
        try {
            Thread.sleep(ms);
        } catch (InterruptedException ignored) {
            Thread.currentThread().interrupt();
        }
    }

    /**
     * Android keeps a copy of every window's nodes in this process and
     * updates it from the events the service takes. This one takes only
     * window changes, since every app on the phone sends a service the
     * kinds of events it asks for, and content changes are many. So the
     * copy goes stale: a capture after a swipe answered the screen before
     * it (the emulator, 2026-10-12). From Android 13 it is dropped before
     * each look; before that, the service takes the content events too
     * (onServiceConnected).
     */
    private void fresh() {
        if (Build.VERSION.SDK_INT >= 33) {
            clearCache();
        }
    }

    /** What can be acted on, numbered, and a screenshot. */
    private JSONObject capture(String mode) throws JSONException {
        fresh();
        List<AccessibilityNodeInfo> found = new ArrayList<>();
        StringBuilder text = new StringBuilder();
        String app = "";
        String hidden = null;
        List<AccessibilityWindowInfo> windows = getWindows();
        for (AccessibilityWindowInfo w : windows) {
            AccessibilityNodeInfo root = w.getRoot();
            if (root == null) {
                if (w.isActive()) {
                    hidden = w.getTitle() == null ? "" : w.getTitle().toString();
                }
                continue;
            }
            if (w.isActive() && root.getPackageName() != null) {
                app = root.getPackageName().toString();
            }
            collect(root, found, 0);
        }
        JSONArray els = new JSONArray();
        Rect r = new Rect();
        for (int i = 0; i < found.size(); i++) {
            AccessibilityNodeInfo n = found.get(i);
            n.getBoundsInScreen(r);
            String label = label(n);
            String kind = shortClass(n);
            StringBuilder line = new StringBuilder();
            line.append('[').append(i + 1).append("] ").append(kind);
            if (!label.isEmpty()) {
                line.append(" \"").append(label).append('"');
            }
            // A row's switch, not the first one in a whole list.
            AccessibilityNodeInfo sw = n.isCheckable() ? n
                    : n.isClickable() && !n.isScrollable() ? checkableInside(n, 0) : null;
            if (sw != null) {
                line.append(sw.isChecked() ? " (on)" : " (off)");
            }
            if (!n.isEnabled()) {
                line.append(" (disabled)");
            }
            if (n.isEditable()) {
                line.append(" (text field)");
            }
            if (n.isScrollable()) {
                line.append(" (scrolls)");
            }
            line.append(" at ").append(r.centerX()).append(',').append(r.centerY());
            text.append(line).append('\n');
            els.put(new JSONObject()
                    .put("index", i + 1)
                    .put("role", kind)
                    .put("label", label)
                    .put("checked", n.isCheckable() ? n.isChecked() : JSONObject.NULL)
                    .put("bounds", new JSONArray().put(r.left).put(r.top).put(r.right).put(r.bottom)));
        }
        synchronized (this) {
            elements.clear();
            elements.addAll(found);
        }
        if (hidden != null) {
            // Measured on the emulator: Settings' Network page once Airplane
            // mode was switched there, before the driver was an accessibility
            // tool (2026-10-12); a capture while a new theme redrew the page
            // (2026-10-13). Said rather than an empty list.
            text.insert(0, "The active window" + (hidden.isEmpty() ? "" : " (" + hidden + ")")
                    + " shows the driver nothing right now: it may still be drawing (a new"
                    + " theme or a rotation redraws it; capture again in a moment), or Android"
                    + " keeps it from the driver. The screenshot still shows it, and Back"
                    + " works; opening the screen again can give one the driver reads.\n");
        }
        JSONObject out = new JSONObject()
                .put("elements", els)
                .put("app", app)
                .put("ax_text", "app: " + app + "\n" + text);
        if (!mode.equals("ax")) {
            String path = screenshot(mode.equals("som") ? found : null);
            if (path != null) {
                out.put("screenshot_path", path);
            }
        }
        return out;
    }

    private void collect(AccessibilityNodeInfo n, List<AccessibilityNodeInfo> out, int depth) {
        if (n == null || depth > 60 || out.size() >= 300) {
            return;
        }
        if (n.isVisibleToUser() && (n.isClickable() || n.isLongClickable() || n.isEditable()
                || n.isScrollable() || n.isCheckable())) {
            out.add(n);
        }
        for (int i = 0; i < n.getChildCount(); i++) {
            collect(n.getChild(i), out, depth + 1);
        }
    }

    /**
     * What a person would read for it: its text, its description, or the
     * text inside it (a Settings row is a layout whose title and summary
     * sit a few levels down), and the state of a switch inside it.
     */
    private static String label(AccessibilityNodeInfo n) {
        CharSequence t = n.getText();
        if (t == null || t.length() == 0) {
            t = n.getContentDescription();
        }
        String s;
        if (t != null && t.length() > 0) {
            s = t.toString();
        } else {
            StringBuilder b = new StringBuilder();
            inner(n, b, 0);
            s = b.toString();
        }
        s = s.replace('\n', ' ').replace('"', '\'').trim();
        return s.length() > 80 ? s.substring(0, 80) + "…" : s;
    }

    /** The text of n's descendants, title first, skipping ones that act on their own. */
    private static void inner(AccessibilityNodeInfo n, StringBuilder b, int depth) {
        if (depth > 4 || b.length() >= 80) {
            return;
        }
        for (int i = 0; i < n.getChildCount() && b.length() < 80; i++) {
            AccessibilityNodeInfo c = n.getChild(i);
            if (c == null || c.isClickable() && !c.isCheckable()) {
                continue;
            }
            CharSequence ct = c.getText() != null && c.getText().length() > 0
                    ? c.getText() : c.getContentDescription();
            if (ct != null && ct.length() > 0) {
                if (b.length() > 0) {
                    b.append(" · ");
                }
                b.append(ct);
            } else {
                inner(c, b, depth + 1);
            }
        }
    }

    /** A switch or check box inside n (a Settings row's), or null. */
    private static AccessibilityNodeInfo checkableInside(AccessibilityNodeInfo n, int depth) {
        if (depth > 4) {
            return null;
        }
        for (int i = 0; i < n.getChildCount(); i++) {
            AccessibilityNodeInfo c = n.getChild(i);
            if (c == null) {
                continue;
            }
            if (c.isCheckable()) {
                return c;
            }
            AccessibilityNodeInfo d = checkableInside(c, depth + 1);
            if (d != null) {
                return d;
            }
        }
        return null;
    }

    private static String shortClass(AccessibilityNodeInfo n) {
        CharSequence c = n.getClassName();
        if (c == null) {
            return "View";
        }
        String s = c.toString();
        return s.substring(s.lastIndexOf('.') + 1);
    }

    /** The screen as a PNG in the agent's cache, the elements' numbers drawn on it. */
    private String screenshot(List<AccessibilityNodeInfo> marks) {
        if (Build.VERSION.SDK_INT < 30) {
            return null;
        }
        CountDownLatch done = new CountDownLatch(1);
        AtomicReference<Bitmap> shot = new AtomicReference<>();
        takeScreenshot(Display.DEFAULT_DISPLAY, getMainExecutor(), new TakeScreenshotCallback() {
            @Override
            public void onSuccess(ScreenshotResult r) {
                Bitmap hw = Bitmap.wrapHardwareBuffer(r.getHardwareBuffer(), r.getColorSpace());
                if (hw != null) {
                    shot.set(hw.copy(Bitmap.Config.ARGB_8888, true));
                }
                r.getHardwareBuffer().close();
                done.countDown();
            }

            @Override
            public void onFailure(int code) {
                Log.w(TAG, "screenshot failed: " + code);
                done.countDown();
            }
        });
        try {
            done.await(5, TimeUnit.SECONDS);
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
        }
        Bitmap bm = shot.get();
        if (bm == null) {
            return null;
        }
        if (marks != null) {
            Canvas cv = new Canvas(bm);
            Paint box = new Paint();
            box.setStyle(Paint.Style.STROKE);
            box.setStrokeWidth(3);
            box.setColor(Color.MAGENTA);
            Paint tag = new Paint();
            tag.setColor(Color.MAGENTA);
            Paint num = new Paint(Paint.ANTI_ALIAS_FLAG);
            num.setColor(Color.WHITE);
            num.setTextSize(28);
            Rect r = new Rect();
            for (int i = 0; i < marks.size(); i++) {
                marks.get(i).getBoundsInScreen(r);
                cv.drawRect(r, box);
                String s = Integer.toString(i + 1);
                float w = num.measureText(s) + 10;
                cv.drawRect(r.left, r.top, r.left + w, r.top + 34, tag);
                cv.drawText(s, r.left + 5, r.top + 27, num);
            }
        }
        File dir = new File(Engine.home(this), "cache/screen");
        if (!dir.isDirectory() && !dir.mkdirs()) {
            return null;
        }
        File f = new File(dir, "screen-" + System.currentTimeMillis() + ".png");
        try (FileOutputStream o = new FileOutputStream(f)) {
            bm.compress(Bitmap.CompressFormat.PNG, 90, o);
        } catch (IOException e) {
            return null;
        }
        // Keep the last ten.
        File[] old = dir.listFiles();
        if (old != null && old.length > 10) {
            java.util.Arrays.sort(old, (x, y) -> Long.compare(x.lastModified(), y.lastModified()));
            for (int i = 0; i < old.length - 10; i++) {
                old[i].delete();
            }
        }
        return f.getPath();
    }

    private AccessibilityNodeInfo element(JSONObject a, String key) {
        int i = a.optInt(key, 0);
        if (i <= 0) {
            return null;
        }
        synchronized (this) {
            if (i > elements.size()) {
                throw new IllegalArgumentException("no element " + i + " in the last capture ("
                        + elements.size() + "); capture again");
            }
            AccessibilityNodeInfo n = elements.get(i - 1);
            if (!n.refresh()) {
                throw new IllegalStateException("element " + i + " is gone from the screen; capture again");
            }
            return n;
        }
    }

    private int[] point(JSONObject a, String key) {
        JSONArray c = a.optJSONArray(key);
        if (c == null || c.length() < 2) {
            return null;
        }
        return new int[] {c.optInt(0), c.optInt(1)};
    }

    private static int[] center(AccessibilityNodeInfo n) {
        Rect r = new Rect();
        n.getBoundsInScreen(r);
        return new int[] {r.centerX(), r.centerY()};
    }

    private JSONObject click(JSONObject a, String how) throws JSONException {
        AccessibilityNodeInfo n = element(a, "element");
        int[] p = point(a, "coordinate");
        boolean longPress = how.equals("right_click");
        if (n != null) {
            int act = longPress ? AccessibilityNodeInfo.ACTION_LONG_CLICK
                    : AccessibilityNodeInfo.ACTION_CLICK;
            for (AccessibilityNodeInfo t = n; t != null; t = t.getParent()) {
                if ((longPress ? t.isLongClickable() : t.isClickable()) && t.performAction(act)) {
                    if (how.equals("double_click")) {
                        sleep(120);
                        t.performAction(act);
                    }
                    return result((longPress ? "long-pressed " : "clicked ") + label(n));
                }
            }
            p = center(n);
        }
        if (p == null) {
            throw new IllegalArgumentException("click needs element (a number from capture) or coordinate [x, y]");
        }
        tap(p[0], p[1], longPress ? 700 : 60);
        if (how.equals("double_click")) {
            sleep(120);
            tap(p[0], p[1], 60);
        }
        return result("tapped " + p[0] + "," + p[1]);
    }

    private void tap(int x, int y, long ms) {
        Path path = new Path();
        path.moveTo(x, y);
        gesture(new GestureDescription.Builder()
                .addStroke(new GestureDescription.StrokeDescription(path, 0, ms)).build());
    }

    private void gesture(GestureDescription g) {
        CountDownLatch done = new CountDownLatch(1);
        AtomicReference<Boolean> ok = new AtomicReference<>(false);
        main.post(() -> dispatchGesture(g, new GestureResultCallback() {
            @Override
            public void onCompleted(GestureDescription d) {
                ok.set(true);
                done.countDown();
            }

            @Override
            public void onCancelled(GestureDescription d) {
                done.countDown();
            }
        }, main));
        try {
            done.await(5, TimeUnit.SECONDS);
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
        }
        if (!ok.get()) {
            throw new IllegalStateException("Android cancelled the gesture");
        }
    }

    /** Puts the text in the field (replacing what it held). */
    private JSONObject type(JSONObject a) throws JSONException {
        AccessibilityNodeInfo n = element(a, "element");
        if (n == null) {
            n = findFocus(AccessibilityNodeInfo.FOCUS_INPUT);
        }
        if (n == null || !n.isEditable()) {
            throw new IllegalStateException("no text field has the focus: click one first, or name it with element");
        }
        Bundle args = new Bundle();
        args.putCharSequence(AccessibilityNodeInfo.ACTION_ARGUMENT_SET_TEXT_CHARSEQUENCE,
                a.optString("text"));
        if (!n.performAction(AccessibilityNodeInfo.ACTION_SET_TEXT, args)) {
            throw new IllegalStateException("the field refused the text");
        }
        return result("typed into " + label(n));
    }

    private JSONObject key(String keys) throws JSONException {
        String k = keys.trim().toLowerCase(Locale.ROOT);
        int g;
        switch (k) {
            case "back":
            case "escape":
            case "esc":
                g = GLOBAL_ACTION_BACK;
                break;
            case "home":
                g = GLOBAL_ACTION_HOME;
                break;
            case "recents":
            case "overview":
                g = GLOBAL_ACTION_RECENTS;
                break;
            case "notifications":
                g = GLOBAL_ACTION_NOTIFICATIONS;
                break;
            case "quick_settings":
                g = GLOBAL_ACTION_QUICK_SETTINGS;
                break;
            case "return":
            case "enter": {
                AccessibilityNodeInfo f = findFocus(AccessibilityNodeInfo.FOCUS_INPUT);
                if (f != null && Build.VERSION.SDK_INT >= 30 && f.performAction(
                        AccessibilityNodeInfo.AccessibilityAction.ACTION_IME_ENTER.getId())) {
                    return result("pressed enter");
                }
                throw new IllegalStateException("no text field to press enter in");
            }
            default:
                throw new IllegalArgumentException("on a phone, key is one of back, home, recents, "
                        + "notifications, quick_settings, enter");
        }
        if (!performGlobalAction(g)) {
            throw new IllegalStateException("Android refused " + k);
        }
        return result("pressed " + k);
    }

    private JSONObject scroll(JSONObject a) throws JSONException {
        String dir = a.optString("direction", "down");
        int amount = Math.max(1, Math.min(10, a.optInt("amount", 3)));
        AccessibilityNodeInfo n = element(a, "element");
        if (n != null && n.isScrollable()) {
            int act = dir.equals("up") || dir.equals("left")
                    ? AccessibilityNodeInfo.ACTION_SCROLL_BACKWARD
                    : AccessibilityNodeInfo.ACTION_SCROLL_FORWARD;
            boolean moved = false;
            for (int i = 0; i < amount; i++) {
                moved |= n.performAction(act);
            }
            return result(moved ? "scrolled " + dir
                    : "nothing moved: the element is at its end that way");
        }
        DisplayMetrics dm = getResources().getDisplayMetrics();
        int[] p = point(a, "coordinate");
        int x = p != null ? p[0] : dm.widthPixels / 2;
        int y = p != null ? p[1] : dm.heightPixels / 2;
        int d = dm.heightPixels / 4;
        for (int i = 0; i < amount; i++) {
            int dx = dir.equals("left") ? d : dir.equals("right") ? -d : 0;
            int dy = dir.equals("up") ? d : dir.equals("down") ? -d : 0;
            Path path = new Path();
            path.moveTo(x - dx / 2f, y - dy / 2f);
            path.lineTo(x + dx / 2f, y + dy / 2f);
            gesture(new GestureDescription.Builder()
                    .addStroke(new GestureDescription.StrokeDescription(path, 0, 250)).build());
            sleep(150);
        }
        return result("scrolled " + dir);
    }

    private JSONObject drag(JSONObject a) throws JSONException {
        AccessibilityNodeInfo from = element(a, "from_element");
        AccessibilityNodeInfo to = element(a, "to_element");
        int[] p = from != null ? center(from) : point(a, "from_coordinate");
        int[] q = to != null ? center(to) : point(a, "to_coordinate");
        if (p == null || q == null) {
            throw new IllegalArgumentException("drag needs a start and an end (elements or coordinates)");
        }
        Path path = new Path();
        path.moveTo(p[0], p[1]);
        path.lineTo(q[0], q[1]);
        gesture(new GestureDescription.Builder()
                .addStroke(new GestureDescription.StrokeDescription(path, 0, 600)).build());
        return result("dragged");
    }

    private List<ResolveInfo> launchable() {
        Intent i = new Intent(Intent.ACTION_MAIN).addCategory(Intent.CATEGORY_LAUNCHER);
        return getPackageManager().queryIntentActivities(i, 0);
    }

    private JSONObject listApps(String filter) throws JSONException {
        String f = filter.toLowerCase(Locale.ROOT);
        PackageManager pm = getPackageManager();
        JSONArray apps = new JSONArray();
        StringBuilder text = new StringBuilder();
        for (ResolveInfo r : launchable()) {
            String name = r.loadLabel(pm).toString();
            String pkg = r.activityInfo.packageName;
            if (!f.isEmpty() && !name.toLowerCase(Locale.ROOT).contains(f) && !pkg.contains(f)) {
                continue;
            }
            apps.put(new JSONObject().put("name", name).put("package", pkg));
            text.append(name).append(" (").append(pkg).append(")\n");
        }
        return new JSONObject().put("apps", apps).put("result", text.toString().trim());
    }

    private JSONObject listWindows() throws JSONException {
        fresh();
        StringBuilder text = new StringBuilder();
        for (AccessibilityWindowInfo w : getWindows()) {
            AccessibilityNodeInfo root = w.getRoot();
            CharSequence title = w.getTitle();
            text.append(title == null ? "" : title).append(" — ")
                    .append(root != null && root.getPackageName() != null ? root.getPackageName() : "")
                    .append(w.isActive() ? " (active)" : "").append('\n');
        }
        return result(text.toString().trim());
    }

    private JSONObject focusApp(String app) throws JSONException {
        String want = app.trim().toLowerCase(Locale.ROOT);
        if (want.isEmpty()) {
            throw new IllegalArgumentException("focus_app needs app: a name or a package");
        }
        PackageManager pm = getPackageManager();
        for (ResolveInfo r : launchable()) {
            String name = r.loadLabel(pm).toString();
            String pkg = r.activityInfo.packageName;
            if (name.toLowerCase(Locale.ROOT).equals(want) || pkg.equals(want)) {
                Intent go = pm.getLaunchIntentForPackage(pkg);
                if (go == null) {
                    break;
                }
                startActivity(go.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK));
                return result("opened " + name);
            }
        }
        throw new IllegalArgumentException("no app called " + app + " (list_apps shows them)");
    }
}
