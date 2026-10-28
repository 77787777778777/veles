package chat.janitor.veles;

import android.app.ActivityManager;
import android.app.Notification;
import android.app.NotificationChannel;
import android.app.NotificationManager;
import android.app.PendingIntent;
import android.content.Context;
import android.content.Intent;

import org.json.JSONException;
import org.json.JSONObject;

import java.io.BufferedReader;
import java.io.IOException;
import java.io.InputStreamReader;
import java.net.HttpURLConnection;
import java.net.URL;
import java.nio.charset.StandardCharsets;
import java.util.HashSet;
import java.util.List;
import java.util.Set;

/**
 * Android notifications for what happens while the desktop is not on
 * the screen (USER 2026-09-30: "Asks and finished turns"): a question an
 * agent asks, and the end of a turn started at the desktop.
 *
 * It reads the gateway's GET /api/notify/events (the engine decides what
 * is worth a notification) from the gateway's own foreground service,
 * which Android does not freeze; the desktop's process, in the
 * background, it does. A notification opens the desktop on its session.
 * Nothing is posted while the desktop is in the foreground: it shows
 * the question itself.
 */
final class Notifier {
    private static final String CH_ASKS = "asks";
    private static final String CH_TURNS = "turns";
    private static final String TAG_ASK = "ask";
    private static final String TAG_TURN = "turn";
    /** The session a notification is about, in its extras (DesktopActivity clears them). */
    static final String EXTRA_SESSION = "chat.janitor.veles.session";
    /** Where the desktop API listens, as android/desktop.tcl has it. */
    private static final String EVENTS = "http://127.0.0.1:8642/api/notify/events";

    private final Context ctx;
    private final NotificationManager nm;
    /** Asks seen in this service's life: a reconnect sends the open ones again. */
    private final Set<String> seen = new HashSet<>();
    private volatile boolean running;
    /** Set by listen() once the stream is open: the wait starts over. */
    private volatile boolean connected;
    private volatile HttpURLConnection conn;
    private Thread thread;

    Notifier(Context c) {
        ctx = c;
        nm = c.getSystemService(NotificationManager.class);
        NotificationChannel asks = new NotificationChannel(
                CH_ASKS, c.getString(R.string.channel_asks), NotificationManager.IMPORTANCE_HIGH);
        NotificationChannel turns = new NotificationChannel(
                CH_TURNS, c.getString(R.string.channel_turns), NotificationManager.IMPORTANCE_LOW);
        turns.setShowBadge(false);
        nm.createNotificationChannel(asks);
        nm.createNotificationChannel(turns);
    }

    void start() {
        if (thread != null) {
            return;
        }
        running = true;
        thread = new Thread(this::loop, "veles-notifier");
        thread.start();
    }

    void stop() {
        running = false;
        HttpURLConnection c = conn;
        if (c != null) {
            c.disconnect();
        }
        Thread t = thread;
        if (t != null) {
            t.interrupt();
        }
        thread = null;
    }

    /**
     * Listens, and while the gateway is starting or restarting, waits and
     * tries again. The wait starts over whenever a connection was made,
     * and stays short: the gateway is on this phone and comes back in
     * seconds, and a turn that ends while nobody listens is not sent again
     * (open asks are). It used to double to a minute and start over only
     * when a stream ended cleanly; a turn the smoke test finished soon
     * after restarting the engine went unnotified.
     */
    private void loop() {
        long backoff = 1000;
        while (running) {
            try {
                listen();
            } catch (IOException | RuntimeException e) {
                // Not up yet, restarting, or the token not written yet.
            }
            if (!running) {
                break;
            }
            if (connected) {
                connected = false;
                backoff = 1000;
            }
            try {
                Thread.sleep(backoff);
            } catch (InterruptedException e) {
                break;
            }
            backoff = Math.min(backoff * 2, 8_000);
        }
    }

    private void listen() throws IOException {
        String token = token();
        if (token.isEmpty()) {
            throw new IOException("no bearer token yet");
        }
        HttpURLConnection c = (HttpURLConnection) new URL(EVENTS).openConnection();
        c.setRequestProperty("Authorization", "Bearer " + token);
        c.setRequestProperty("Accept", "text/event-stream");
        c.setConnectTimeout(5_000);
        // The stream sends a keep-alive comment every 15 s: a minute of
        // silence is a dead connection.
        c.setReadTimeout(60_000);
        conn = c;
        try (BufferedReader r = new BufferedReader(
                new InputStreamReader(c.getInputStream(), StandardCharsets.UTF_8))) {
            connected = true;
            String event = null;
            StringBuilder data = new StringBuilder();
            for (String line; running && (line = r.readLine()) != null; ) {
                if (line.isEmpty()) {
                    if (event != null) {
                        handle(event, data.toString());
                    }
                    event = null;
                    data.setLength(0);
                } else if (line.startsWith("event:")) {
                    event = line.substring(6).trim();
                } else if (line.startsWith("data:")) {
                    if (data.length() > 0) {
                        data.append('\n');
                    }
                    data.append(line.substring(5).trim());
                }
            }
        } finally {
            c.disconnect();
            conn = null;
        }
    }

    private void handle(String event, String data) {
        JSONObject o;
        try {
            o = new JSONObject(data);
        } catch (JSONException e) {
            return;
        }
        String session = o.optString("session_id");
        String title = o.optString("title");
        if (title.isEmpty()) {
            title = ctx.getString(R.string.app_name);
        }
        switch (event) {
            case "ask": {
                String id = o.optString("id");
                if (id.isEmpty() || !seen.add(id) || desktopInFront()) {
                    return;
                }
                post(TAG_ASK, id.hashCode(), CH_ASKS, title, o.optString("prompt"), session);
                break;
            }
            case "ask-resolved":
                nm.cancel(TAG_ASK, o.optString("id").hashCode());
                break;
            case "turn-done": {
                if (desktopInFront()) {
                    return;
                }
                String text = o.optString("text");
                if (text.isEmpty()) {
                    text = ctx.getString(R.string.notify_turn_done);
                }
                post(TAG_TURN, session.hashCode(), CH_TURNS, title, text, session);
                break;
            }
            default:
                break;
        }
    }

    private void post(String tag, int id, String channel, String title, String text, String session) {
        Intent open = new Intent(ctx, DesktopActivity.class)
                .putExtra("session", session)
                .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK);
        PendingIntent pi = PendingIntent.getActivity(ctx, (tag + session).hashCode(), open,
                PendingIntent.FLAG_IMMUTABLE | PendingIntent.FLAG_UPDATE_CURRENT);
        Notification n = new Notification.Builder(ctx, channel)
                .setSmallIcon(R.drawable.ic_notification)
                .setContentTitle(title)
                .setContentText(text)
                .setStyle(new Notification.BigTextStyle().bigText(text))
                .setContentIntent(pi)
                .setAutoCancel(true)
                .addExtras(sessionExtra(session))
                .setCategory(TAG_ASK.equals(tag) ? Notification.CATEGORY_MESSAGE : Notification.CATEGORY_STATUS)
                .build();
        nm.notify(tag, id, n);
    }

    private static android.os.Bundle sessionExtra(String session) {
        android.os.Bundle b = new android.os.Bundle();
        b.putString(EXTRA_SESSION, session);
        return b;
    }

    /** The desktop's process holds the screen: it shows the question itself. */
    private boolean desktopInFront() {
        String name = ctx.getPackageName() + ":desktop";
        ActivityManager am = ctx.getSystemService(ActivityManager.class);
        List<ActivityManager.RunningAppProcessInfo> ps = am.getRunningAppProcesses();
        if (ps == null) {
            return false;
        }
        for (ActivityManager.RunningAppProcessInfo p : ps) {
            if (name.equals(p.processName)
                    && p.importance == ActivityManager.RunningAppProcessInfo.IMPORTANCE_FOREGROUND) {
                return true;
            }
        }
        return false;
    }

    /** DESKTOP_BEARER_TOKEN from the gateway's .env (Engine.bearer). */
    private String token() {
        return Engine.bearer(ctx);
    }
}
