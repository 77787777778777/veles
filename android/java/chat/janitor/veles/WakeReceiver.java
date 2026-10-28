package chat.janitor.veles;

import android.content.BroadcastReceiver;
import android.content.Context;
import android.content.Intent;

import java.io.File;

/**
 * Starts the engine when nothing else can (PLAN-sessions-and-phone.md A1,
 * round P4).
 *
 * <ul>
 * <li>The cron alarm (Wake.arm): a job is due while the engine is stopped.
 * <li>The phone started: alarms do not survive a restart, so the plan's
 * alarm is set again; and when the person chose "Start with the phone" on
 * the Phone page (the {@code boot-start} file), the engine starts.
 * </ul>
 *
 * Neither starts it when the person stopped it (Stop in the gateway's
 * screen or its notification): GatewayService.wake asks Prefs first.
 */
public final class WakeReceiver extends BroadcastReceiver {
    static final String ACTION_ALARM = "chat.janitor.veles.action.WAKE";

    /** The Phone page's "Start with the phone" (lib/phone.tcl writes it). */
    static File bootFlag(Context c) {
        return new File(c.getFilesDir(), "boot-start");
    }

    @Override
    public void onReceive(Context c, Intent i) {
        String action = i.getAction();
        if (ACTION_ALARM.equals(action)) {
            Prefs.setLastAlarm(c, Prefs.armedAlarm(c));
            GatewayService.wake(c, "a cron job is due");
        } else if (Intent.ACTION_BOOT_COMPLETED.equals(action)) {
            if (bootFlag(c).exists()) {
                GatewayService.wake(c, "the phone started");
            } else {
                Wake.arm(c);
            }
        }
    }
}
