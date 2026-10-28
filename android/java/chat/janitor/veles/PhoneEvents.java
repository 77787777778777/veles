package chat.janitor.veles;

import android.content.Context;
import android.os.SystemClock;

import org.json.JSONObject;

import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.net.HttpURLConnection;
import java.net.URL;
import java.nio.charset.StandardCharsets;
import java.util.ArrayDeque;

/**
 * What happens on the phone, told to its own gateway
 * (PLAN-sessions-and-phone.md P4): {@code POST /api/device/event}, where
 * the {@code device_event} hook hears it and the person's triggers on it
 * run (crates/veles-core/src/device_events.rs).
 *
 * While the engine runs, every event goes; the gateway counts none of them
 * as use (its idle clock is for people). While it is stopped, only a
 * notification some trigger listens to starts it again (Wake), and waits
 * here until the gateway answers. A queued event is kept ten minutes at
 * most, fifty at most: a phone that cannot start the engine does not pile
 * them up.
 */
final class PhoneEvents {
    private static final String EVENT = "http://127.0.0.1:8642/api/device/event";
    private static final int MAX_QUEUED = 50;
    private static final long KEEP_MS = 10 * 60_000L;

    private static final class Pending {
        final JSONObject ev;
        final long at;

        Pending(JSONObject ev, long at) {
            this.ev = ev;
            this.at = at;
        }
    }

    /** Guarded by itself, as is {@link #sender}. */
    private static final ArrayDeque<Pending> queue = new ArrayDeque<>();
    private static Thread sender;

    private PhoneEvents() {}

    private static boolean running() {
        return GatewayService.state == GatewayService.RUNNING
                || GatewayService.state == GatewayService.STARTING;
    }

    /**
     * A notification (NotificationListener). Sent while the engine runs;
     * otherwise only when a trigger listens to its app, and then the engine
     * is started for it.
     */
    static void notification(Context c, JSONObject ev) {
        if (!running()) {
            if (!Wake.read(c).wantsNotification(ev.optString("source"))) {
                return;
            }
            if (!GatewayService.wake(c, "a notification from " + ev.optString("source"))) {
                return;
            }
        }
        enqueue(c, ev);
    }

    /** An event that means something only while the engine runs (power). */
    static void whileRunning(Context c, JSONObject ev) {
        if (running()) {
            enqueue(c, ev);
        }
    }

    private static void enqueue(Context c, JSONObject ev) {
        final Context app = c.getApplicationContext();
        synchronized (queue) {
            while (queue.size() >= MAX_QUEUED) {
                queue.removeFirst();
            }
            queue.addLast(new Pending(ev, SystemClock.elapsedRealtime()));
            if (sender == null) {
                sender = new Thread(() -> drain(app), "veles-events");
                sender.start();
            }
        }
    }

    /** Send what is queued, oldest first, waiting while the engine starts. */
    private static void drain(Context c) {
        long backoff = 500;
        while (true) {
            Pending p;
            synchronized (queue) {
                p = queue.peekFirst();
                if (p == null) {
                    sender = null;
                    return;
                }
                if (SystemClock.elapsedRealtime() - p.at > KEEP_MS) {
                    queue.removeFirst();
                    continue;
                }
            }
            if (send(c, p.ev)) {
                synchronized (queue) {
                    queue.remove(p);
                }
                backoff = 500;
                continue;
            }
            try {
                Thread.sleep(backoff);
            } catch (InterruptedException e) {
                synchronized (queue) {
                    sender = null;
                }
                return;
            }
            backoff = Math.min(backoff * 2, 8_000);
        }
    }

    /**
     * One POST. True when the gateway took it, or refused it for good (a
     * 4xx: sending it again would be refused again); false when it is not
     * up yet.
     */
    private static boolean send(Context c, JSONObject ev) {
        String token = Engine.bearer(c);
        if (token.isEmpty()) {
            return false;
        }
        HttpURLConnection conn = null;
        try {
            byte[] body = ev.toString().getBytes(StandardCharsets.UTF_8);
            conn = (HttpURLConnection) new URL(EVENT).openConnection();
            conn.setRequestMethod("POST");
            conn.setConnectTimeout(3_000);
            conn.setReadTimeout(30_000);
            conn.setDoOutput(true);
            conn.setRequestProperty("Authorization", "Bearer " + token);
            conn.setRequestProperty("Content-Type", "application/json");
            conn.setFixedLengthStreamingMode(body.length);
            try (OutputStream o = conn.getOutputStream()) {
                o.write(body);
            }
            int code = conn.getResponseCode();
            InputStream in = code < 400 ? conn.getInputStream() : conn.getErrorStream();
            if (in != null) {
                in.close();
            }
            return code < 500;
        } catch (IOException e) {
            return false;
        } finally {
            if (conn != null) {
                conn.disconnect();
            }
        }
    }
}
