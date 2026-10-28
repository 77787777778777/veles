package chat.janitor.veles;

import android.app.Notification;
import android.app.NotificationChannel;
import android.app.NotificationManager;
import android.app.PendingIntent;
import android.app.Service;
import android.content.BroadcastReceiver;
import android.content.Context;
import android.content.Intent;
import android.content.IntentFilter;
import android.content.pm.ServiceInfo;
import android.graphics.drawable.Icon;
import android.os.Build;
import android.os.Handler;
import android.os.IBinder;
import android.os.Looper;
import android.os.SystemClock;

import org.json.JSONException;
import org.json.JSONObject;

import java.io.File;
import java.io.IOException;
import java.util.concurrent.TimeUnit;

/**
 * Runs `veles gateway run --exit-when-idle 300`: a foreground service,
 * so Android shows it and does not reap it as idle. The phone's
 * gateway is for the person holding it — chat networks and cron run
 * on the VPS — so five minutes after anyone last used it the engine
 * stops on its own (exit 75), which ends the service too: no timers,
 * no sockets, no radio. Opening the app starts it again.
 *
 * An exit that is not the idle stop is a crash, restarted the way a
 * systemd unit with Restart=always would (1 s, doubling to a minute,
 * reset by five minutes of uptime). Stop is the other way it ends on
 * purpose.
 *
 * Opening the app starts it, and since P4 (PLAN-sessions-and-phone.md)
 * three things more, all through {@link #wake}: a notification a trigger
 * listens to (PhoneEvents), the next cron job's alarm (Wake, WakeReceiver)
 * and, when the person chose it, the phone starting. While it runs it also
 * offers the agent the phone's background actions (ServiceDevice) and
 * tells the gateway when the phone is plugged in or unplugged.
 */
public final class GatewayService extends Service {
    static final String ACTION_STOP = "chat.janitor.veles.action.STOP";
    private static final String CHANNEL = "gateway";
    private static final int NOTIFICATION = 1;
    /** "Could not start in the background" (wake). */
    private static final int NOTIFICATION_BLOCKED = 3;
    private static final long LOG_MAX = 4L << 20;
    /** The engine stopped because nobody used it (crates/veles-core/src/idle.rs). */
    private static final int EXIT_IDLE = 75;

    /** What the activity shows. One process, so a static is enough. */
    static final int STOPPED = 0, STARTING = 1, RUNNING = 2, RESTARTING = 3;

    static volatile int state = STOPPED;
    static volatile int lastExit;
    static volatile int retryIn;

    private final Handler main = new Handler(Looper.getMainLooper());
    /**
     * The supervisor's sleep between retries waits on this, and a start
     * wakes it. Nothing interrupts the supervisor but a stop: a start
     * that did (every open of the app starts the service) broke it out
     * of waiting for a running engine, which it then left unsupervised.
     */
    private final Object wake = new Object();
    /** Main thread only: the supervisor thread, while it runs. */
    private Thread supervisor;
    /** Main thread only: the latest start, for stopSelfResult. */
    private int lastStartId;
    /** Guarded by wake: the supervisor sleeps between retries. */
    private boolean sleeping;
    /** Guarded by wake: a start came during that sleep; retry at once. */
    private boolean startNow;
    /** Guarded by wake: the supervisor has left its loop and starts no engine. */
    private boolean supervisorDone;
    private volatile boolean stopping;
    private volatile Process child;
    /** Asks and finished turns, as notifications (Notifier). */
    private Notifier notifier;
    /** The phone's background actions for the agent (ServiceDevice). */
    private ServiceDevice device;
    /** Plugged in or unplugged: a device event while the engine runs. */
    private final BroadcastReceiver power = new BroadcastReceiver() {
        @Override
        public void onReceive(Context c, Intent i) {
            String state = Intent.ACTION_POWER_CONNECTED.equals(i.getAction())
                    ? "connected" : "disconnected";
            try {
                PhoneEvents.whileRunning(c, new JSONObject()
                        .put("device", "phone").put("kind", "power").put("source", state)
                        .put("at", System.currentTimeMillis()));
            } catch (JSONException ignored) {
                // Not with these values.
            }
        }
    };
    private boolean powerRegistered;

    static void start(Context c) {
        Prefs.setEnabled(c, true);
        c.startForegroundService(new Intent(c, GatewayService.class));
    }

    static void stop(Context c) {
        Prefs.setEnabled(c, false);
        c.startService(new Intent(c, GatewayService.class).setAction(ACTION_STOP));
    }

    /**
     * Start the engine for {@code why}, from the background: unless the
     * person stopped it. False when it will not start. Android lets a
     * background app start its service only when it may run in the
     * background (the Phone page's "Running in the background"); refused,
     * the person is told once an hour, and the gateway's log says why.
     */
    static boolean wake(Context c, String why) {
        if (!Prefs.enabled(c)) {
            return false;
        }
        if (state == RUNNING || state == STARTING) {
            return true;
        }
        try {
            c.startForegroundService(new Intent(c, GatewayService.class));
            Engine.append(Engine.log(c), "veles-android: starting for " + why);
            return true;
        } catch (RuntimeException e) {
            // ForegroundServiceStartNotAllowedException (Android 12 on).
            Engine.append(Engine.log(c), "veles-android: Android would not start the engine for "
                    + why + ": " + e);
            blocked(c);
            return false;
        }
    }

    private static void blocked(Context c) {
        long now = System.currentTimeMillis();
        if (now - Prefs.blockedNoticeAt(c) < TimeUnit.HOURS.toMillis(1)) {
            return;
        }
        Prefs.setBlockedNoticeAt(c, now);
        NotificationManager nm = c.getSystemService(NotificationManager.class);
        if (nm == null) {
            return;
        }
        nm.createNotificationChannel(new NotificationChannel(
                CHANNEL, c.getString(R.string.channel_gateway), NotificationManager.IMPORTANCE_LOW));
        PendingIntent open = PendingIntent.getActivity(c, 2,
                new Intent(c, DesktopActivity.class).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK),
                PendingIntent.FLAG_IMMUTABLE);
        String text = c.getString(R.string.notify_wake_blocked);
        nm.notify(NOTIFICATION_BLOCKED, new Notification.Builder(c, CHANNEL)
                .setSmallIcon(R.drawable.ic_notification)
                .setContentTitle(c.getString(R.string.app_name))
                .setContentText(text)
                .setStyle(new Notification.BigTextStyle().bigText(text))
                .setContentIntent(open)
                .setAutoCancel(true)
                .build());
    }

    @Override
    public IBinder onBind(Intent intent) {
        return null;
    }

    @Override
    public void onCreate() {
        super.onCreate();
        NotificationChannel ch = new NotificationChannel(
                CHANNEL, getString(R.string.channel_gateway), NotificationManager.IMPORTANCE_LOW);
        ch.setShowBadge(false);
        getSystemService(NotificationManager.class).createNotificationChannel(ch);
        IntentFilter f = new IntentFilter(Intent.ACTION_POWER_CONNECTED);
        f.addAction(Intent.ACTION_POWER_DISCONNECTED);
        if (Build.VERSION.SDK_INT >= 33) {
            registerReceiver(power, f, Context.RECEIVER_NOT_EXPORTED);
        } else {
            registerReceiver(power, f);
        }
        powerRegistered = true;
    }

    @Override
    public int onStartCommand(Intent intent, int flags, int startId) {
        lastStartId = startId;
        if (intent != null && ACTION_STOP.equals(intent.getAction())) {
            Prefs.setEnabled(this, false);
            shutdown();
            return START_NOT_STICKY;
        }
        // A null intent is Android restarting a service it killed: the
        // gateway was running, so it runs again.
        Prefs.setEnabled(this, true);
        // Every open of the app lands here. A running engine needs
        // nothing; one the supervisor waits to restart starts now; one
        // that stopped (idle) gets a new supervisor — also when the old
        // one has only just left its loop and has not yet stopped the
        // service, whose stop then stands down (supervise's end).
        boolean spawn;
        synchronized (wake) {
            if (supervisor != null && supervisor.isAlive() && !supervisorDone) {
                if (sleeping) {
                    startNow = true;
                    wake.notifyAll();
                }
                spawn = false;
            } else {
                // A Stop under way ends the service; this start is lost
                // with it, as before (Start again, or reopen the app).
                spawn = !stopping;
                if (spawn) {
                    supervisorDone = false;
                    startNow = false;
                    state = STARTING;
                }
            }
        }
        Notification n = notification(describe(this));
        if (Build.VERSION.SDK_INT >= 34) {
            startForeground(NOTIFICATION, n, ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE);
        } else {
            startForeground(NOTIFICATION, n);
        }
        if (spawn) {
            supervisor = new Thread(this::supervise, "veles-supervisor");
            supervisor.start();
        }
        if (notifier == null) {
            notifier = new Notifier(this);
        }
        notifier.start();
        if (device == null) {
            device = new ServiceDevice(this);
        }
        device.start();
        return START_STICKY;
    }

    @Override
    public void onDestroy() {
        stopping = true;
        if (notifier != null) {
            notifier.stop();
        }
        if (device != null) {
            device.stop();
        }
        if (powerRegistered) {
            powerRegistered = false;
            unregisterReceiver(power);
        }
        Process p = child;
        if (p != null) {
            p.destroy();
        }
        Thread s = supervisor;
        if (s != null) {
            s.interrupt();
        }
        // The userland binding and its relay socket (Userland.shell made
        // them); running sessions keep their own sockets.
        UserlandRelay.stop();
        state = STOPPED;
        super.onDestroy();
    }

    private void supervise() {
        final Thread me = Thread.currentThread();
        File log = Engine.log(this);
        long backoff = 1000;
        while (!stopping) {
            long started = SystemClock.elapsedRealtime();
            int code;
            try {
                Engine.unpack(this);
                Engine.rotate(log, LOG_MAX);
                ProcessBuilder pb = Engine.gateway(this);
                pb.redirectErrorStream(true);
                pb.redirectOutput(ProcessBuilder.Redirect.appendTo(log));
                pb.redirectInput(ProcessBuilder.Redirect.from(new File("/dev/null")));
                child = pb.start();
                state = RUNNING;
                show();
                code = waitFor(child, log);
            } catch (IOException e) {
                Engine.append(log, "veles-android: cannot start the gateway: " + e);
                code = -1;
            } catch (InterruptedException e) {
                // A stop: shutdown() and onDestroy() set stopping, then
                // end the engine themselves.
                break;
            } finally {
                child = null;
            }
            if (stopping) {
                break;
            }
            if (code == EXIT_IDLE) {
                // Nobody has used the gateway for five minutes: it stops,
                // and the service with it. The toggle stays on (Prefs),
                // so opening the app starts it again — and so do the
                // next cron job's alarm and a notification a trigger
                // listens to (wake.json, which the engine just wrote).
                Engine.append(log, "veles-android: idle, stopping");
                Wake.arm(this);
                break;
            }
            if (SystemClock.elapsedRealtime() - started > TimeUnit.MINUTES.toMillis(5)) {
                backoff = 1000;
            }
            lastExit = code;
            retryIn = (int) (backoff / 1000);
            state = RESTARTING;
            Engine.append(log, "veles-android: the gateway exited with " + code
                    + "; restarting in " + retryIn + " s");
            show();
            boolean now;
            synchronized (wake) {
                sleeping = true;
                long until = SystemClock.elapsedRealtime() + backoff;
                try {
                    long left;
                    while (!startNow && !stopping
                            && (left = until - SystemClock.elapsedRealtime()) > 0) {
                        wake.wait(left);
                    }
                } catch (InterruptedException e) {
                    // A stop (stopping is set); the loop ends below.
                } finally {
                    sleeping = false;
                }
                now = startNow;
                startNow = false;
            }
            backoff = now ? 1000 : Math.min(backoff * 2, TimeUnit.MINUTES.toMillis(1));
        }
        synchronized (wake) {
            supervisorDone = true;
            state = STOPPED;
        }
        Engine.append(log, "veles-android: supervisor done (stopping=" + stopping + ")");
        if (!stopping) {
            // Idle: no child, no timers, no sockets. The notification
            // (a foreground service's) goes with the service — unless a
            // start came meanwhile: it runs a new supervisor (stand
            // down), or is still on its way (stopSelfResult keeps the
            // service for it).
            main.post(() -> {
                if (supervisor != me || stopping) {
                    return;
                }
                supervisor = null;
                stopForeground(STOP_FOREGROUND_REMOVE);
                stopSelfResult(lastStartId);
            });
        }
    }

    /**
     * Waits for the engine to exit. Only a stop interrupts this thread,
     * and it sets stopping first; any other interrupt waits on, so the
     * engine is never left without its supervisor.
     */
    private int waitFor(Process p, File log) throws InterruptedException {
        while (true) {
            try {
                return p.waitFor();
            } catch (InterruptedException e) {
                if (stopping) {
                    throw e;
                }
                Engine.append(log, "veles-android: supervisor interrupted while the engine runs; waiting on");
            }
        }
    }

    /** SIGTERM, which the gateway shuts down on; SIGKILL after 10 s. */
    private void shutdown() {
        stopping = true;
        if (notifier != null) {
            notifier.stop();
        }
        if (device != null) {
            device.stop();
        }
        // Stopped by the person: no alarm starts it again (Wake.arm reads
        // the toggle, which Stop has cleared).
        Wake.arm(this);
        Thread s = supervisor;
        new Thread(() -> {
            Process p = child;
            if (p != null) {
                p.destroy();
                try {
                    if (!p.waitFor(10, TimeUnit.SECONDS)) {
                        p.destroyForcibly();
                    }
                } catch (InterruptedException ignored) {
                    p.destroyForcibly();
                }
            }
            if (s != null) {
                s.interrupt();
            }
            main.post(() -> {
                state = STOPPED;
                stopForeground(STOP_FOREGROUND_REMOVE);
                stopSelf();
            });
        }, "veles-stop").start();
    }

    /** The state in words, for the notification and the activity. */
    static String describe(Context c) {
        switch (state) {
            case STARTING:
                return c.getString(R.string.state_starting);
            case RUNNING:
                return c.getString(R.string.state_running);
            case RESTARTING:
                return c.getString(R.string.state_restarting, lastExit, retryIn);
            default:
                return c.getString(R.string.state_stopped);
        }
    }

    /**
     * Updates the notification from the main thread, where shutdown's
     * stopForeground also runs: posted from the supervisor after it, the
     * update would bring back a notification nothing removes.
     */
    private void show() {
        main.post(() -> {
            if (!stopping) {
                getSystemService(NotificationManager.class)
                        .notify(NOTIFICATION, notification(describe(this)));
            }
        });
    }

    private Notification notification(String text) {
        PendingIntent open = PendingIntent.getActivity(this, 0,
                new Intent(this, MainActivity.class), PendingIntent.FLAG_IMMUTABLE);
        PendingIntent stop = PendingIntent.getService(this, 1,
                new Intent(this, GatewayService.class).setAction(ACTION_STOP),
                PendingIntent.FLAG_IMMUTABLE);
        return new Notification.Builder(this, CHANNEL)
                .setSmallIcon(R.drawable.ic_notification)
                .setContentTitle(getString(R.string.app_name))
                .setContentText(text)
                .setContentIntent(open)
                .setOngoing(true)
                .setOnlyAlertOnce(true)
                .addAction(new Notification.Action.Builder(
                        Icon.createWithResource(this, R.drawable.ic_notification),
                        getString(R.string.action_stop), stop).build())
                .build();
    }
}
