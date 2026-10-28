package chat.janitor.veles;

import android.content.Context;
import android.content.SharedPreferences;

/**
 * Whether the person wants the gateway running: Start sets it, Stop clears
 * it. And the cron alarm's bookkeeping (Wake).
 */
final class Prefs {
    private Prefs() {}

    private static SharedPreferences prefs(Context c) {
        return c.getSharedPreferences("gateway", Context.MODE_PRIVATE);
    }

    static boolean enabled(Context c) {
        return prefs(c).getBoolean("enabled", false);
    }

    static void setEnabled(Context c, boolean on) {
        prefs(c).edit().putBoolean("enabled", on).apply();
    }

    /** The cron time the alarm is set for, epoch seconds; 0 = none. */
    static long armedAlarm(Context c) {
        return prefs(c).getLong("alarm_armed", 0);
    }

    static void setArmedAlarm(Context c, long at) {
        prefs(c).edit().putLong("alarm_armed", at).apply();
    }

    /** The cron time the last alarm woke the engine for. */
    static long lastAlarm(Context c) {
        return prefs(c).getLong("alarm_fired", 0);
    }

    static void setLastAlarm(Context c, long at) {
        prefs(c).edit().putLong("alarm_fired", at).apply();
    }

    /**
     * Run the gateway's shells in the userland package's distro when it
     * answers (Userland.java). On unless turned off; no switch on screen
     * yet (PLAN-phone-userland.md T3: the toggle's UI comes with its
     * translations).
     */
    static boolean useUserland(Context c) {
        return prefs(c).getBoolean("use_userland", true);
    }

    /** When the person was last told the engine could not start; ms. */
    static long blockedNoticeAt(Context c) {
        return prefs(c).getLong("blocked_notice", 0);
    }

    static void setBlockedNoticeAt(Context c, long ms) {
        prefs(c).edit().putLong("blocked_notice", ms).apply();
    }
}
