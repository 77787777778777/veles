package chat.janitor.veles.userland;

import android.app.Notification;
import android.app.NotificationChannel;
import android.app.NotificationManager;
import android.app.PendingIntent;
import android.app.Service;
import android.content.Context;
import android.content.Intent;
import android.content.pm.PackageManager;
import android.os.Handler;
import android.os.IBinder;
import android.os.Looper;
import android.os.PowerManager;
import android.os.SystemClock;

import java.io.File;
import java.io.FileOutputStream;
import java.io.IOException;
import java.io.OutputStream;
import java.io.RandomAccessFile;
import java.nio.charset.StandardCharsets;
import java.util.concurrent.TimeUnit;

/**
 * Serves the gateway: unpacks the rootfs on first run (Rootfs), then runs
 * the exec broker, lib/(abi)/libveles_userland.so (crates/veles-userland
 * says what it does with a session). The gateway binds this service,
 * which starts it, and asks its Binder (Sessions) for each session: the
 * two apps may not connect to each other's sockets (PLAN-phone-userland.md
 * T3 §2 R1b), so Sessions makes a socketpair and hands the broker one end
 * on @veles-userland-handoff. The broker's own @veles-userland stays, for
 * a system that lets the gateway connect.
 *
 * A foreground service with a partial wake lock, like the gateway's own
 * GatewayService: the broker's children are what the agent's terminal is
 * running, and they must not freeze when the screen goes off. A broker
 * that exits is restarted (1 s, doubling to a minute, reset by five
 * minutes of uptime). Stop ends it.
 *
 * It serves exactly one uid: chat.janitor.veles's, and only when that
 * package is signed with this one's key, so a look-alike package of the
 * same name cannot drive the distro.
 */
public final class UserlandService extends Service {
    static final String GATEWAY = "chat.janitor.veles";
    static final String SOCKET = "veles-userland";
    static final String ACTION_STOP = "chat.janitor.veles.userland.action.STOP";
    private static final String CHANNEL = "userland";
    private static final int NOTIFICATION = 1;
    private static final long LOG_MAX = 1L << 20;

    /** What the activity shows; one process. */
    static volatile String state = "";

    private final Handler main = new Handler(Looper.getMainLooper());
    private final Sessions sessions = new Sessions(this);
    private PowerManager.WakeLock wake;
    private Thread supervisor;
    private volatile boolean stopping;
    private volatile Process broker;

    /** A start from the gateway's bind, which a person's Stop outranks. */
    private static final String EXTRA_BIND = "chat.janitor.veles.userland.extra.BIND";

    /** The person's Start (the activity): also lifts their Stop. */
    static void start(Context c) {
        setStopped(c, false);
        c.startForegroundService(new Intent(c, UserlandService.class));
    }

    static void stop(Context c) {
        c.startService(new Intent(c, UserlandService.class).setAction(ACTION_STOP));
    }

    /**
     * The person pressed Stop (here or on the notification) and has not
     * started it since: the gateway's bind does not start it again, and
     * its sessions are refused, so the gateway's shell goes back to
     * Android's at its next start (docs/phone.md "The userland package").
     */
    static boolean stoppedByPerson(Context c) {
        return c.getSharedPreferences("userland", MODE_PRIVATE).getBoolean("stopped", false);
    }

    private static void setStopped(Context c, boolean on) {
        c.getSharedPreferences("userland", MODE_PRIVATE).edit().putBoolean("stopped", on).apply();
    }

    static File log(Context c) {
        return new File(c.getFilesDir(), "userland.log");
    }

    /**
     * The gateway's bind (UserlandRelay.java). Binding creates the service
     * but does not start it, so it starts itself here: the broker runs only
     * under the started, foreground service. (targetSdk 28: Android's
     * limits on starting a foreground service from the background apply
     * from target 31 on.) Not after the person's Stop.
     */
    @Override
    public IBinder onBind(Intent intent) {
        if (!stoppedByPerson(this) && (supervisor == null || !supervisor.isAlive())) {
            startForegroundService(new Intent(this, UserlandService.class).putExtra(EXTRA_BIND, true));
        }
        return sessions;
    }

    /** A Stop is under way or stands, or the service is being destroyed. */
    boolean isStopping() {
        return stopping || stoppedByPerson(this);
    }

    @Override
    public void onCreate() {
        super.onCreate();
        state = getString(R.string.state_stopped);
        NotificationChannel ch = new NotificationChannel(
                CHANNEL, getString(R.string.channel_userland), NotificationManager.IMPORTANCE_LOW);
        ch.setShowBadge(false);
        getSystemService(NotificationManager.class).createNotificationChannel(ch);
    }

    @Override
    public int onStartCommand(Intent intent, int flags, int startId) {
        boolean fromBind = intent != null && intent.getBooleanExtra(EXTRA_BIND, false);
        if (intent != null && ACTION_STOP.equals(intent.getAction())) {
            setStopped(this, true);
            stopping = true;
            Process p = broker;
            if (p != null) {
                p.destroy();
            }
            stopForeground(STOP_FOREGROUND_REMOVE);
            stopSelf();
            return START_NOT_STICKY;
        }
        startForeground(NOTIFICATION, notification(state));
        if (fromBind && stoppedByPerson(this)) {
            // A Stop landed between the bind and this start: it wins.
            stopForeground(STOP_FOREGROUND_REMOVE);
            stopSelf();
            return START_NOT_STICKY;
        }
        if (wake == null) {
            PowerManager pm = getSystemService(PowerManager.class);
            wake = pm.newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "veles:userland");
            wake.acquire();
        }
        if (supervisor == null || !supervisor.isAlive()) {
            stopping = false;
            supervisor = new Thread(this::supervise, "veles-userland");
            supervisor.start();
        }
        return START_STICKY;
    }

    @Override
    public void onDestroy() {
        stopping = true;
        Process p = broker;
        if (p != null) {
            p.destroy();
        }
        Thread s = supervisor;
        if (s != null) {
            s.interrupt();
        }
        if (wake != null && wake.isHeld()) {
            wake.release();
        }
        state = getString(R.string.state_stopped);
        super.onDestroy();
    }

    /**
     * The gateway's uid, or -1 (and why, in the log) when it is not
     * installed or not signed by this package's key.
     */
    int gatewayUid() {
        PackageManager pm = getPackageManager();
        try {
            int uid = pm.getApplicationInfo(GATEWAY, 0).uid;
            if (pm.checkSignatures(GATEWAY, getPackageName()) != PackageManager.SIGNATURE_MATCH) {
                append(log(this), "userland: " + GATEWAY + " is signed by another key; not serving it");
                return -1;
            }
            return uid;
        } catch (PackageManager.NameNotFoundException e) {
            append(log(this), "userland: " + GATEWAY + " is not installed; nothing to serve");
            return -1;
        }
    }

    private void supervise() {
        File log = log(this);
        rotate(log);
        show(getString(R.string.state_preparing));
        try {
            append(log, Rootfs.ensure(this));
        } catch (IOException e) {
            append(log, "userland: cannot prepare the rootfs: " + e);
            show(getString(R.string.state_failed, e.getMessage()));
            return;
        }
        int uid = gatewayUid();
        if (uid < 0) {
            show(getString(R.string.state_failed, GATEWAY));
            return;
        }
        String lib = getApplicationInfo().nativeLibraryDir;
        long backoff = 1000;
        while (!stopping) {
            long started = SystemClock.elapsedRealtime();
            int code;
            try {
                ProcessBuilder pb = new ProcessBuilder(
                        lib + "/libveles_userland.so",
                        "--socket", SOCKET,
                        "--handoff", Sessions.HANDOFF,
                        "--allow-uid", Integer.toString(uid),
                        "--proot", lib + "/libproot.so",
                        "--loader", lib + "/libproot_loader.so",
                        "--rootfs", Rootfs.dir(this).getPath(),
                        "--tmp", Rootfs.tmp(this).getPath(),
                        "--shared", Rootfs.shared(this).getPath(),
                        "--sandboxes", Rootfs.sandboxes(this).getPath());
                pb.redirectErrorStream(true);
                pb.redirectOutput(ProcessBuilder.Redirect.appendTo(log));
                pb.redirectInput(ProcessBuilder.Redirect.from(new File("/dev/null")));
                broker = pb.start();
                show(getString(R.string.state_running));
                code = broker.waitFor();
            } catch (IOException e) {
                append(log, "userland: cannot start the broker: " + e);
                code = -1;
            } catch (InterruptedException e) {
                break;
            } finally {
                broker = null;
            }
            if (stopping) {
                break;
            }
            if (SystemClock.elapsedRealtime() - started > TimeUnit.MINUTES.toMillis(5)) {
                backoff = 1000;
            }
            append(log, "userland: the broker exited with " + code + "; again in "
                    + backoff / 1000 + " s");
            show(getString(R.string.state_failed, "exit " + code));
            try {
                Thread.sleep(backoff);
            } catch (InterruptedException e) {
                break;
            }
            backoff = Math.min(backoff * 2, TimeUnit.MINUTES.toMillis(1));
        }
    }

    private void show(String text) {
        state = text;
        main.post(() -> {
            if (!stopping) {
                getSystemService(NotificationManager.class).notify(NOTIFICATION, notification(text));
            }
        });
    }

    private Notification notification(String text) {
        PendingIntent open = PendingIntent.getActivity(this, 0,
                new Intent(this, MainActivity.class), PendingIntent.FLAG_IMMUTABLE);
        PendingIntent stop = PendingIntent.getService(this, 1,
                new Intent(this, UserlandService.class).setAction(ACTION_STOP),
                PendingIntent.FLAG_IMMUTABLE);
        return new Notification.Builder(this, CHANNEL)
                .setSmallIcon(android.R.drawable.stat_notify_sync_noanim)
                .setContentTitle(getString(R.string.app_name))
                .setContentText(text)
                .setContentIntent(open)
                .setOngoing(true)
                .setOnlyAlertOnce(true)
                .addAction(new Notification.Action.Builder(null, getString(R.string.action_stop), stop)
                        .build())
                .build();
    }

    static void append(File f, String line) {
        try (OutputStream o = new FileOutputStream(f, true)) {
            o.write((line + "\n").getBytes(StandardCharsets.UTF_8));
        } catch (IOException ignored) {
            // The log is for the person reading it.
        }
    }

    private static void rotate(File f) {
        if (f.length() > LOG_MAX) {
            File old = new File(f.getPath() + ".1");
            old.delete();
            f.renameTo(old);
        }
    }

    /** The last {@code max} bytes of the log. */
    static String tail(File f, int max) {
        try (RandomAccessFile r = new RandomAccessFile(f, "r")) {
            long len = r.length();
            long from = Math.max(0, len - max);
            byte[] b = new byte[(int) (len - from)];
            r.seek(from);
            r.readFully(b);
            return new String(b, StandardCharsets.UTF_8);
        } catch (IOException e) {
            return "";
        }
    }
}
