package chat.janitor.veles;

import android.content.Context;
import android.content.Intent;
import android.content.IntentFilter;
import android.os.BatteryManager;

import org.json.JSONArray;
import org.json.JSONException;
import org.json.JSONObject;

import java.io.BufferedReader;
import java.io.IOException;
import java.io.InputStream;
import java.io.InputStreamReader;
import java.io.OutputStream;
import java.net.HttpURLConnection;
import java.net.URL;
import java.net.URLEncoder;
import java.nio.charset.StandardCharsets;

/**
 * The phone as a device the agent can use while its window is closed
 * (PLAN-sessions-and-phone.md A0/A1, USER decision D3: Java in the
 * gateway service for what works in the background). It says hello to the
 * gateway as {@code phone}, the window's own name, so the agent sees one
 * phone with both sets of actions (crates/veles-core/src/devices.rs);
 * requests come on {@code /api/device/events}, answers go to
 * {@code /api/device/result}, as lib/device.tcl does it.
 *
 * Its actions answer from the service's own process, which Android does
 * not freeze: {@code notifications} (what other apps posted lately, read
 * by NotificationListener when the person allowed it) and {@code battery}.
 * The {@code device} tool is the owner's alone.
 *
 * Every request it makes is passive ({@code X-Veles-Passive: 1}): the
 * service listening is not the person using the gateway.
 */
final class ServiceDevice {
    private static final String BASE = "http://127.0.0.1:8642";
    private static final String CLIENT_ID = "phone-service";

    private final Context ctx;
    private volatile boolean running;
    private volatile HttpURLConnection conn;
    private Thread thread;

    ServiceDevice(Context c) {
        ctx = c.getApplicationContext();
    }

    void start() {
        if (thread != null) {
            return;
        }
        running = true;
        thread = new Thread(this::loop, "veles-device");
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

    private void loop() {
        long backoff = 1000;
        while (running) {
            boolean connected = false;
            try {
                hello();
                connected = true;
                listen();
            } catch (IOException | JSONException | RuntimeException e) {
                // Not up yet, restarting, or no token yet.
            }
            if (!running) {
                break;
            }
            if (connected) {
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

    private static JSONObject cap(String name, String description, JSONObject props)
            throws JSONException {
        return new JSONObject()
                .put("name", name)
                .put("description", description)
                .put("schema", new JSONObject().put("type", "object")
                        .put("properties", props))
                .put("foreground", false)
                .put("timeout_secs", 10);
    }

    private void hello() throws IOException, JSONException {
        JSONArray caps = new JSONArray()
                .put(cap("notifications",
                        "What other apps on the phone notified lately, newest first: app, title, "
                                + "text, time (only when the person allowed veles to read "
                                + "notifications; limit 1..50, default 10).",
                        new JSONObject().put("limit", new JSONObject().put("type", "integer"))))
                .put(cap("battery",
                        "The phone's battery: level in percent, and whether it is charging.",
                        new JSONObject()));
        JSONObject hello = new JSONObject()
                .put("client_id", CLIENT_ID)
                .put("name", "phone")
                .put("platform", "android")
                .put("capabilities", caps);
        int code = post("/api/device/hello", hello);
        if (code != 200) {
            throw new IOException("hello: " + code);
        }
    }

    private void listen() throws IOException {
        String token = Engine.bearer(ctx);
        if (token.isEmpty()) {
            throw new IOException("no bearer token yet");
        }
        HttpURLConnection c = (HttpURLConnection) new URL(BASE + "/api/device/events?client_id="
                + URLEncoder.encode(CLIENT_ID, "UTF-8")).openConnection();
        c.setRequestProperty("Authorization", "Bearer " + token);
        c.setRequestProperty("Accept", "text/event-stream");
        c.setRequestProperty("X-Veles-Passive", "1");
        c.setConnectTimeout(5_000);
        // The stream sends a keep-alive comment every 15 s.
        c.setReadTimeout(60_000);
        conn = c;
        try (BufferedReader r = new BufferedReader(
                new InputStreamReader(c.getInputStream(), StandardCharsets.UTF_8))) {
            String event = null;
            StringBuilder data = new StringBuilder();
            for (String line; running && (line = r.readLine()) != null; ) {
                if (line.isEmpty()) {
                    if ("request".equals(event)) {
                        final String req = data.toString();
                        new Thread(() -> answer(req), "veles-device-call").start();
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

    private void answer(String raw) {
        String id = "";
        try {
            JSONObject req = new JSONObject(raw);
            id = req.optString("id");
            JSONObject args = req.optJSONObject("args");
            Object result = run(req.optString("action"), args == null ? new JSONObject() : args);
            post("/api/device/result", new JSONObject().put("id", id).put("ok", true)
                    .put("result", result));
        } catch (Exception e) {
            try {
                post("/api/device/result", new JSONObject().put("id", id).put("ok", false)
                        .put("result", String.valueOf(e.getMessage())));
            } catch (IOException | JSONException ignored) {
                // The call times out and says so.
            }
        }
    }

    private Object run(String action, JSONObject args) throws JSONException {
        switch (action) {
            case "notifications": {
                if (!NotificationListener.granted(ctx)) {
                    throw new IllegalStateException("veles may not read this phone's notifications: "
                            + "the person turns that on in veles' Phone page (Reading notifications)");
                }
                int limit = Math.max(1, Math.min(50, args.optInt("limit", 10)));
                return NotificationListener.recent(limit);
            }
            case "battery": {
                Intent b = ctx.registerReceiver(null, new IntentFilter(Intent.ACTION_BATTERY_CHANGED));
                if (b == null) {
                    throw new IllegalStateException("Android did not say");
                }
                int level = b.getIntExtra(BatteryManager.EXTRA_LEVEL, -1);
                int scale = b.getIntExtra(BatteryManager.EXTRA_SCALE, 100);
                int plugged = b.getIntExtra(BatteryManager.EXTRA_PLUGGED, 0);
                int status = b.getIntExtra(BatteryManager.EXTRA_STATUS, -1);
                String by = plugged == BatteryManager.BATTERY_PLUGGED_AC ? "ac"
                        : plugged == BatteryManager.BATTERY_PLUGGED_USB ? "usb"
                        : plugged == BatteryManager.BATTERY_PLUGGED_WIRELESS ? "wireless" : "";
                return new JSONObject()
                        .put("level", scale > 0 && level >= 0 ? level * 100 / scale : -1)
                        .put("charging", status == BatteryManager.BATTERY_STATUS_CHARGING
                                || status == BatteryManager.BATTERY_STATUS_FULL)
                        .put("plugged", by);
            }
            default:
                throw new IllegalArgumentException("the phone's service cannot " + action);
        }
    }

    private int post(String path, JSONObject body) throws IOException {
        String token = Engine.bearer(ctx);
        if (token.isEmpty()) {
            throw new IOException("no bearer token yet");
        }
        byte[] bytes = body.toString().getBytes(StandardCharsets.UTF_8);
        HttpURLConnection c = (HttpURLConnection) new URL(BASE + path).openConnection();
        try {
            c.setRequestMethod("POST");
            c.setConnectTimeout(5_000);
            c.setReadTimeout(20_000);
            c.setDoOutput(true);
            c.setRequestProperty("Authorization", "Bearer " + token);
            c.setRequestProperty("Content-Type", "application/json");
            c.setRequestProperty("X-Veles-Passive", "1");
            c.setFixedLengthStreamingMode(bytes.length);
            try (OutputStream o = c.getOutputStream()) {
                o.write(bytes);
            }
            int code = c.getResponseCode();
            InputStream in = code < 400 ? c.getInputStream() : c.getErrorStream();
            if (in != null) {
                in.close();
            }
            return code;
        } finally {
            c.disconnect();
        }
    }
}
