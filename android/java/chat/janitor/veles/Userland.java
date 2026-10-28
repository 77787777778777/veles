package chat.janitor.veles;

import android.content.Context;
import android.content.pm.PackageManager;
import android.os.SystemClock;

import java.io.File;
import java.io.IOException;
import java.io.InputStream;
import java.nio.charset.StandardCharsets;
import java.util.Map;
import java.util.concurrent.TimeUnit;

/**
 * The userland package, chat.janitor.veles.userland (PLAN-phone-userland.md T3):
 * an Alpine distro in an app of its own, at the targetSdk that may still
 * execute its own files. When it is installed, signed with this app's
 * key, and a probe command runs there, the gateway's shell is
 * lib/(abi)/libulsh.so, which runs every script in that distro
 * (crates/veles-userland); otherwise nothing here changes the shell.
 *
 * ulsh reaches the distro through UserlandRelay: this app may not connect
 * to the userland app's socket (their SELinux domains, T3 §2 R1b), so each
 * ulsh asks the relay, in this process, for a session the userland service
 * hands out over Binder.
 */
final class Userland {
    private Userland() {}

    static final String PACKAGE = "chat.janitor.veles.userland";
    /** The probe's limit: the first one may wait for the rootfs to unpack. */
    private static final long PROBE_SECS = 60;

    /**
     * VELES_SHELL (and SHELL) → ulsh, with the relay's name and the
     * broker's uid for ulsh to check, when Prefs allows it and a probe
     * runs. Called last when the environment is built, so it wins over the
     * bash fallback.
     */
    static void shell(Context c, String lib, Map<String, String> env) {
        String ulsh = lib + "/libulsh.so";
        if (!Prefs.useUserland(c) || !new File(ulsh).canExecute()) {
            return;
        }
        int uid = brokerUid(c);
        if (uid < 0) {
            return;
        }
        File log = Engine.log(c);
        try {
            UserlandRelay.start(c);
        } catch (IOException | RuntimeException e) {
            // SecurityException: no OPEN_SESSION permission, which Android
            // grants only when the userland was installed before veles.
            Engine.append(log, "veles-android: cannot bind the userland package: " + e
                    + " (install Veles userland, then veles again); the shell stays the APK's");
            return;
        }
        long t0 = SystemClock.elapsedRealtime();
        String failed = probe(ulsh, uid);
        long ms = SystemClock.elapsedRealtime() - t0;
        if (failed != null) {
            Engine.append(log, "veles-android: the userland package does not run commands: "
                    + failed + " (" + ms + " ms); the shell stays the APK's");
            return;
        }
        env.put("VELES_SHELL", ulsh);
        env.put("SHELL", ulsh);
        env.put("ULSH_RELAY", UserlandRelay.SOCKET);
        env.put("ULSH_BROKER_UID", Integer.toString(uid));
        Engine.append(log, "veles-android: the shell is the userland's (ulsh through @"
                + UserlandRelay.SOCKET + ", broker uid " + uid + "; probe " + ms + " ms)");
    }

    /** The userland app's uid, or -1 when absent or signed by another key. */
    static int brokerUid(Context c) {
        PackageManager pm = c.getPackageManager();
        try {
            int uid = pm.getApplicationInfo(PACKAGE, 0).uid;
            return pm.checkSignatures(PACKAGE, c.getPackageName()) == PackageManager.SIGNATURE_MATCH
                    ? uid : -1;
        } catch (PackageManager.NameNotFoundException e) {
            return -1;
        }
    }

    /**
     * `ulsh -c 'exit 0'` the way the engine will run every shell: a child
     * of this app, on pipes, its session from the relay. null when it
     * exits 0, else what went wrong in ulsh's own words (where the
     * system's policy refused something, this is where it says so).
     */
    private static String probe(String ulsh, int uid) {
        ProcessBuilder pb = new ProcessBuilder(ulsh, "-c", "exit 0");
        pb.environment().put("ULSH_RELAY", UserlandRelay.SOCKET);
        pb.environment().put("ULSH_BROKER_UID", Integer.toString(uid));
        pb.redirectErrorStream(true);
        Process p;
        try {
            p = pb.start();
        } catch (IOException e) {
            return "cannot start ulsh: " + e;
        }
        try {
            p.getOutputStream().close();
            if (!p.waitFor(PROBE_SECS, TimeUnit.SECONDS)) {
                p.destroy();
                return "no answer in " + PROBE_SECS + " s";
            }
            int code = p.exitValue();
            return code == 0 ? null : "exit " + code + ": " + drain(p.getInputStream()).trim();
        } catch (IOException e) {
            return "ulsh: " + e;
        } catch (InterruptedException e) {
            p.destroy();
            Thread.currentThread().interrupt();
            return "interrupted";
        }
    }

    /** What a finished process left in its pipe, up to 4 KiB. */
    private static String drain(InputStream in) throws IOException {
        byte[] b = new byte[4096];
        int n = 0;
        for (int r; n < b.length && (r = in.read(b, n, b.length - n)) > 0; ) {
            n += r;
        }
        return new String(b, 0, n, StandardCharsets.UTF_8);
    }
}
