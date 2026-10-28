package chat.janitor.veles;

import android.app.Notification;
import android.content.ComponentName;
import android.content.Context;
import android.content.pm.ApplicationInfo;
import android.content.pm.PackageManager;
import android.os.Build;
import android.os.Bundle;
import android.provider.Settings;
import android.service.notification.NotificationListenerService;
import android.service.notification.StatusBarNotification;

import org.json.JSONArray;
import org.json.JSONException;
import org.json.JSONObject;

import java.util.ArrayDeque;
import java.util.Iterator;
import java.util.LinkedHashMap;
import java.util.Map;

/**
 * The notifications other apps post, read for the person's triggers
 * (PLAN-sessions-and-phone.md A1, round P4). Android runs this only after
 * the person turned on veles under Settings → Notifications → Notification
 * access (the Phone page's "Reading notifications" opens that screen); it
 * cannot be granted any other way.
 *
 * Each new notification goes to the gateway on this phone (PhoneEvents),
 * which runs the triggers that listen to its app, and nothing else: the
 * gateway does not keep it. The last fifty are held here, in memory only,
 * for the agent's {@code device notifications} action (ServiceDevice),
 * which only the owner's agent may call.
 *
 * Left out: veles' own notifications, ongoing ones (music, navigation,
 * downloads, a call), group summaries (they repeat their children), and an
 * update that changes neither title nor text (a progress bar).
 */
public final class NotificationListener extends NotificationListenerService {
    private static final int RECENT = 50;
    /** Newest last. Guarded by itself. */
    private static final ArrayDeque<JSONObject> recent = new ArrayDeque<>();
    /** Each notification's key → its last title and text, to skip repeats. */
    private static final Map<String, Integer> lastSeen = new LinkedHashMap<String, Integer>() {
        @Override
        protected boolean removeEldestEntry(Map.Entry<String, Integer> e) {
            return size() > 200;
        }
    };

    /** Has the person given veles notification access? */
    static boolean granted(Context c) {
        ComponentName me = new ComponentName(c, NotificationListener.class);
        if (Build.VERSION.SDK_INT >= 27) {
            android.app.NotificationManager nm =
                    c.getSystemService(android.app.NotificationManager.class);
            return nm != null && nm.isNotificationListenerAccessGranted(me);
        }
        // Android 8.0 has no call for it: the setting it keeps.
        String on = Settings.Secure.getString(c.getContentResolver(),
                "enabled_notification_listeners");
        return on != null && on.contains(me.flattenToString());
    }

    /** The most recent notifications, newest first. */
    static JSONArray recent(int limit) {
        JSONArray out = new JSONArray();
        synchronized (recent) {
            Iterator<JSONObject> it = recent.descendingIterator();
            while (it.hasNext() && out.length() < limit) {
                out.put(it.next());
            }
        }
        return out;
    }

    @Override
    public void onNotificationPosted(StatusBarNotification sbn) {
        try {
            JSONObject ev = event(sbn);
            if (ev == null) {
                return;
            }
            synchronized (recent) {
                while (recent.size() >= RECENT) {
                    recent.removeFirst();
                }
                recent.addLast(ev);
            }
            PhoneEvents.notification(this, ev);
        } catch (RuntimeException | JSONException e) {
            // One odd notification must not end the listener.
        }
    }

    private JSONObject event(StatusBarNotification sbn) throws JSONException {
        String pkg = sbn.getPackageName();
        Notification n = sbn.getNotification();
        if (pkg == null || pkg.equals(getPackageName()) || n == null || sbn.isOngoing()
                || (n.flags & Notification.FLAG_GROUP_SUMMARY) != 0) {
            return null;
        }
        Bundle x = n.extras;
        String title = text(x, Notification.EXTRA_TITLE);
        String body = text(x, Notification.EXTRA_BIG_TEXT);
        if (body.isEmpty()) {
            body = text(x, Notification.EXTRA_TEXT);
        }
        if (title.isEmpty() && body.isEmpty()) {
            return null;
        }
        int sig = (title + "\u0001" + body).hashCode();
        synchronized (lastSeen) {
            Integer before = lastSeen.put(sbn.getKey(), sig);
            if (before != null && before == sig) {
                return null;
            }
        }
        return new JSONObject()
                .put("device", "phone")
                .put("kind", "notification")
                .put("source", pkg)
                .put("app", label(pkg))
                .put("title", clip(title, 1000))
                .put("text", clip(body, 4000))
                .put("at", sbn.getPostTime());
    }

    private static String text(Bundle x, String key) {
        if (x == null) {
            return "";
        }
        CharSequence s = x.getCharSequence(key);
        return s == null ? "" : s.toString().trim();
    }

    private static String clip(String s, int max) {
        return s.length() <= max ? s : s.substring(0, max);
    }

    private String label(String pkg) {
        try {
            PackageManager pm = getPackageManager();
            ApplicationInfo ai = pm.getApplicationInfo(pkg, 0);
            return pm.getApplicationLabel(ai).toString();
        } catch (PackageManager.NameNotFoundException | RuntimeException e) {
            return pkg;
        }
    }
}
