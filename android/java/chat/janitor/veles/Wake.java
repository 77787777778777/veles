package chat.janitor.veles;

import android.app.AlarmManager;
import android.app.PendingIntent;
import android.content.Context;
import android.content.Intent;
import android.os.Build;

import org.json.JSONArray;
import org.json.JSONObject;

import java.io.File;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.util.ArrayList;
import java.util.List;
import java.util.Locale;

/**
 * What a stopped engine is started for (PLAN-sessions-and-phone.md P4;
 * crates/veles-core/src/device_events.rs `write_wake`). The gateway stops
 * five minutes after its last use and leaves {@code wake.json} in its home:
 * the apps whose notifications a trigger listens to, and when the next cron
 * job is due. Nothing else starts it again but the person opening the app:
 * a notification no trigger wants must not cost the battery its idle stop
 * saves.
 */
final class Wake {
    /** The next cron job, epoch seconds; 0 = none. */
    final long at;
    /** Package globs ({@code *} and {@code ?}); {@code *} = any. */
    final List<String> notifications;
    /** The next pool heartbeat, epoch seconds; 0 = not in a pool. */
    final long heartbeatAt;

    private Wake(long at, List<String> notifications, long heartbeatAt) {
        this.at = at;
        this.notifications = notifications;
        this.heartbeatAt = heartbeatAt;
    }

    static File file(Context c) {
        return new File(Engine.home(c), "wake.json");
    }

    /** The plan the engine left; an empty one when there is none. */
    static Wake read(Context c) {
        List<String> globs = new ArrayList<>();
        long at = 0;
        long heartbeatAt = 0;
        try {
            JSONObject o = new JSONObject(new String(
                    Files.readAllBytes(file(c).toPath()), StandardCharsets.UTF_8));
            at = o.optLong("at", 0);
            heartbeatAt = o.optLong("heartbeat_at", 0);
            JSONArray a = o.optJSONArray("notifications");
            for (int i = 0; a != null && i < a.length(); i++) {
                String g = a.optString(i, "").trim();
                if (!g.isEmpty()) {
                    globs.add(g);
                }
            }
        } catch (Exception e) {
            // No plan yet (the engine never stopped for idleness), or a
            // broken one: nothing wakes it.
        }
        return new Wake(at, globs, heartbeatAt);
    }

    /** Does a trigger listen to notifications from {@code pkg}? */
    boolean wantsNotification(String pkg) {
        for (String g : notifications) {
            if (glob(g, pkg)) {
                return true;
            }
        }
        return false;
    }

    /** Case-insensitive {@code *}/{@code ?} glob, as the triggers' own. */
    static boolean glob(String pattern, String text) {
        return globAt(pattern.toLowerCase(Locale.ROOT), 0, text.toLowerCase(Locale.ROOT), 0);
    }

    private static boolean globAt(String p, int i, String t, int j) {
        while (i < p.length()) {
            char c = p.charAt(i);
            if (c == '*') {
                for (int k = j; k <= t.length(); k++) {
                    if (globAt(p, i + 1, t, k)) {
                        return true;
                    }
                }
                return false;
            }
            if (j >= t.length() || (c != '?' && c != t.charAt(j))) {
                return false;
            }
            i++;
            j++;
        }
        return j == t.length();
    }

    private static PendingIntent alarm(Context c) {
        return PendingIntent.getBroadcast(c, 0,
                new Intent(c, WakeReceiver.class).setAction(WakeReceiver.ACTION_ALARM),
                PendingIntent.FLAG_IMMUTABLE | PendingIntent.FLAG_UPDATE_CURRENT);
    }

    /**
     * Set the alarm for the next cron job (and the next pool heartbeat)
     * in the plan, or clear it. After the engine stopped for idleness,
     * and after the phone restarted (alarms do not survive that).
     *
     * A plan that names the time an alarm already woke the engine for is
     * not armed again: the job did not run, and an alarm for it would
     * start the engine every five minutes for nothing.
     */
    static void arm(Context c) {
        AlarmManager am = c.getSystemService(AlarmManager.class);
        if (am == null) {
            return;
        }
        Wake w = read(c);
        PendingIntent pi = alarm(c);
        // The earliest of cron and heartbeat is what the alarm fires for.
        long next = 0;
        if (w.at > 0 && w.at != Prefs.lastAlarm(c)) {
            next = w.at;
        }
        if (w.heartbeatAt > 0 && w.heartbeatAt != Prefs.lastAlarm(c)) {
            if (next <= 0 || w.heartbeatAt < next) {
                next = w.heartbeatAt;
            }
        }
        if (next <= 0 || !Prefs.enabled(c)) {
            am.cancel(pi);
            Prefs.setArmedAlarm(c, 0);
            return;
        }
        // Not sooner than a minute from now: a job that is due already
        // runs then, not in a loop of starts.
        long when = Math.max(next * 1000, System.currentTimeMillis() + 60_000);
        if (Build.VERSION.SDK_INT < 31 || am.canScheduleExactAlarms()) {
            am.setExactAndAllowWhileIdle(AlarmManager.RTC_WAKEUP, when, pi);
        } else {
            // Without "Alarms & reminders" Android picks the moment
            // within its own window: a cron job may start a few minutes late.
            am.setAndAllowWhileIdle(AlarmManager.RTC_WAKEUP, when, pi);
        }
        Prefs.setArmedAlarm(c, next);
        Engine.append(Engine.log(c), "veles-android: alarm set for " + next
                + (w.at > 0 && next == w.at ? " (cron)" : " (heartbeat)"));
    }
}
