package chat.janitor.veles;

import android.content.Context;
import android.content.pm.PackageManager;

import java.io.File;
import java.io.FileInputStream;
import java.io.FileOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.io.RandomAccessFile;
import java.nio.charset.StandardCharsets;
import java.util.Locale;
import java.util.Map;
import java.util.TimeZone;
import java.util.zip.ZipEntry;
import java.util.zip.ZipInputStream;

/**
 * Where the engine and its files live on the phone, and the environment
 * `veles gateway run` gets.
 *
 * The APK carries the engine as lib/(abi)/libveles.so: Android runs
 * executables only from the app's native library directory, and only
 * files named lib*.so are extracted there. Tcl's script library and the
 * engine's catalogs are plain files, so they travel as assets/runtime.zip
 * and are unpacked into app storage.
 */
final class Engine {
    private Engine() {}

    /** VELES_HOME: config and state, in app-private storage. */
    static File home(Context c) {
        return new File(c.getFilesDir(), "home");
    }

    /** Tcl's script library and the catalogs, unpacked from the APK. */
    static File runtime(Context c) {
        return new File(c.getFilesDir(), "runtime");
    }

    /** The engine's stdout and stderr. */
    static File log(Context c) {
        return new File(c.getFilesDir(), "gateway.log");
    }

    static String nativeDir(Context c) {
        return c.getApplicationInfo().nativeLibraryDir;
    }

    /**
     * Unpacks assets/runtime.zip once per install: the stamp is the
     * install time, so a reinstall of the same version unpacks again.
     */
    static void unpack(Context c) throws IOException {
        File dir = runtime(c);
        File stamp = new File(dir, ".installed");
        String want;
        try {
            want = Long.toString(c.getPackageManager()
                    .getPackageInfo(c.getPackageName(), 0).lastUpdateTime);
        } catch (PackageManager.NameNotFoundException e) {
            throw new IOException(e);
        }
        if (want.equals(read(stamp))) {
            return;
        }
        deleteTree(dir);
        byte[] buf = new byte[1 << 16];
        String root = dir.getCanonicalPath() + File.separator;
        try (ZipInputStream zip = new ZipInputStream(c.getAssets().open("runtime.zip"))) {
            for (ZipEntry e; (e = zip.getNextEntry()) != null; ) {
                File out = new File(dir, e.getName());
                if (!out.getCanonicalPath().startsWith(root)) {
                    throw new IOException("runtime.zip entry outside its directory: " + e.getName());
                }
                if (e.isDirectory()) {
                    mkdirs(out);
                    continue;
                }
                mkdirs(out.getParentFile());
                try (OutputStream o = new FileOutputStream(out)) {
                    for (int n; (n = zip.read(buf)) > 0; ) {
                        o.write(buf, 0, n);
                    }
                }
            }
        }
        write(stamp, want);
    }

    /**
     * How long the engine runs unused before it stops: five minutes, or
     * the number in files/idle-secs (10 s to a day). The emulator smoke
     * sets it short to watch the stop and what starts the engine again
     * (scripts/android-emulator-smoke.sh); a person may set it longer.
     */
    static int idleSecs(Context c) {
        String s = read(new File(c.getFilesDir(), "idle-secs")).trim();
        try {
            int n = Integer.parseInt(s);
            if (n >= 10 && n <= 86_400) {
                return n;
            }
        } catch (NumberFormatException e) {
            // Absent or not a number: the default.
        }
        return 300;
    }

    /** The command and environment of `veles gateway run`. */
    static ProcessBuilder gateway(Context c) throws IOException {
        String lib = nativeDir(c);
        File home = home(c);
        mkdirs(home);
        // --exit-when-idle: the phone's gateway stops five minutes
        // after anyone last used it (GatewayService, which that stop
        // ends too; opening the app starts it again).
        ProcessBuilder pb = new ProcessBuilder(
                lib + "/libveles.so", "gateway", "run", "--exit-when-idle",
                Integer.toString(idleSecs(c)));
        pb.directory(home);
        Map<String, String> env = pb.environment();
        env.put("HOME", home.getPath());
        env.put("VELES_HOME", home.getPath());
        env.put("TMPDIR", c.getCacheDir().getPath());
        // files/bin holds the APK's static tools (TOOLS: bash, git, rg,
        // jq, curl, sqlite3, ssh, scp) and busybox's applets as symlinks
        // into the native library directory (binDir): terminal commands
        // then have them on PATH the way a laptop does. Android lets no
        // app execute a file in its own storage, but a link is only a
        // name: the exec lands on the lib*.so, which the app may run like
        // the engine (measured from inside the app, android/README.md).
        // The engine's own search does not go through PATH: VELES_RG below.
        //
        // files/bin FIRST: toybox in /system/bin has its own ls, ps, sed,
        // grep, find … with other flags, and ahead on PATH it shadowed
        // every busybox applet the agent writes for (HANDOFF pitfall 77).
        // What only toybox has (getprop, am, pm, logcat, cmd, settings)
        // still resolves from /system/bin.
        String bin = binDir(c);
        env.put("PATH", bin + ":/system/bin");
        // The shell every spawn runs (crates/veles-core/src/shell.rs:
        // the terminal, background shells, monitors, `.md` commands, the
        // sandbox's memory wrapper): bash when the APK carries it, else
        // busybox's ash, else the platform default (mksh). By its files/bin
        // name, so the platform line says `shell: bash`, not `libbash`.
        String shell = shellFor(c, bin);
        if (shell != null) {
            env.put("VELES_SHELL", shell);
            env.put("SHELL", shell);
        }
        // libveles.so's runpath finds libtcl8.6.so beside it; this is the
        // same directory, for libtclsh.so and anything else it runs.
        env.put("LD_LIBRARY_PATH", lib);
        env.put("TCL_LIBRARY", new File(runtime(c), "tcl8.6").getPath());
        env.put("VELES_TCLSH", lib + "/libtclsh.so");
        // ripgrep for search_files: Android extracts only lib*.so from
        // the APK and there is no `rg` on a phone's PATH, so the engine
        // is told where its own copy lives (the VELES_TCLSH pattern).
        env.put("VELES_RG", lib + "/librg.so");
        // git (libgit.so, libgit_remote_http.so): its helpers are found
        // through GIT_EXEC_PATH, the links linkGit made.
        File gitCore = new File(c.getFilesDir(), "libexec/git-core");
        boolean curl = new File(bin, "curl").exists();
        // git and curl are static against musl, which cannot resolve
        // names on Android ("Could not resolve host", the real phone,
        // 2026-10-12): they go out through the engine's loopback CONNECT
        // proxy (crates/veles-core/src/connect_proxy.rs), with a token
        // minted for this start. git's as configuration in the
        // environment, curl's in its curlrc (below), so every git and
        // curl the engine or the agent runs has it, and nothing else is
        // proxied (the engine's own HTTP resolves through bionic).
        String proxyUrl = null;
        if (gitCore.isDirectory() || curl) {
            byte[] raw = new byte[16];
            new java.security.SecureRandom().nextBytes(raw);
            StringBuilder token = new StringBuilder();
            for (byte b : raw) {
                token.append(String.format(Locale.ROOT, "%02x", b));
            }
            String proxy = "127.0.0.1:9877";
            env.put("VELES_CONNECT_PROXY", proxy + " " + token);
            proxyUrl = "http://veles:" + token + "@" + proxy;
        }
        if (gitCore.isDirectory()) {
            env.put("GIT_EXEC_PATH", gitCore.getPath());
            env.put("GIT_CONFIG_NOSYSTEM", "1");
            gitConfig(env, "http.proxy", proxyUrl);
            // No templates ship: an empty folder, so `git init` and
            // `git clone` do not warn on stderr (which Tcl's exec takes
            // for a failure).
            File templates = new File(c.getFilesDir(), "libexec/git-templates");
            if (templates.isDirectory() || templates.mkdirs()) {
                gitConfig(env, "init.templateDir", templates.getPath());
            }
        }
        env.put("VELES_LOCALES_DIR", new File(runtime(c), "locales").getPath());
        // The bundled Tcl plugins (the Anthropic Messages driver the setup
        // wizard's Anthropic choice names): plain files in runtime.zip,
        // unpacked beside the catalogs. The engine's own lookup beside the
        // binary finds nothing here — the binary is a lib*.so in the native
        // library directory — so it is told (plugins::BUNDLED_ENV).
        env.put("VELES_BUNDLED_PLUGINS", new File(runtime(c), "plugins").getPath());
        // The in-process modules (irc, slopdrop, telegram, lang-js): the APK
        // carries each as lib/(abi)/libveles_module_<name>.so, and the
        // module host wants <root>/<name>/plugin.toml naming the file —
        // written here per start, pointing at the native directory's
        // copy (its path moves with every reinstall).
        env.put("VELES_MODULE_PATH", modulesDir(c));
        env.put("VELES_LOCALE", Locale.getDefault().toLanguageTag());
        env.put("LANG", "C.UTF-8");
        env.put("TZ", TimeZone.getDefault().getID());
        // The log is a file the activity shows as text: no ANSI colors.
        env.put("NO_COLOR", "1");
        // The system's CA store (the updatable one first, Android 14 on);
        // rustls's native-certs loader reads the hashed files there.
        String caFile = null;
        for (String certs : new String[] {
                "/apex/com.android.conscrypt/cacerts", "/system/etc/security/cacerts"}) {
            if (new File(certs).isDirectory()) {
                env.put("SSL_CERT_DIR", certs);
                // git's OpenSSL looks issuers up in a folder by a newer
                // name hash than Android's files carry ("unable to get
                // local issuer certificate", the real phone, 2026-10-12):
                // it gets the same certificates as one file.
                String bundle = caBundle(c, new File(certs));
                if (bundle != null && env.containsKey("GIT_CONFIG_COUNT")) {
                    gitConfig(env, "http.sslCAInfo", bundle);
                }
                caFile = bundle;
                break;
            }
        }
        if (curl && proxyUrl != null) {
            String curlDir = curlHome(c, proxyUrl, caFile);
            if (curlDir != null) {
                env.put("CURL_HOME", curlDir);
            }
        }
        // The userland package's distro, when it answers (Userland.java,
        // PLAN-phone-userland.md T3). Last, so ulsh wins over the APK's
        // own shell, which stays the fallback.
        Userland.shell(c, lib, env);
        return pb;
    }

    /**
     * files/etc/curl/.curlrc, rewritten per start: the curl CLI
     * (libcurl_cli.so) goes out through the engine's CONNECT proxy for
     * every URL (`proxytunnel`, so plain http too) except loopback, and
     * checks certificates against the one-file bundle git uses. It
     * carries this start's proxy token, so it is private. The directory
     * on success, null when it could not be written.
     */
    private static String curlHome(Context c, String proxyUrl, String caFile) {
        File dir = new File(c.getFilesDir(), "etc/curl");
        StringBuilder rc = new StringBuilder();
        rc.append("proxy = \"").append(proxyUrl).append("\"\n");
        rc.append("proxytunnel\n");
        rc.append("noproxy = \"localhost,127.0.0.1,::1\"\n");
        if (caFile != null) {
            rc.append("cacert = \"").append(caFile).append("\"\n");
        }
        try {
            mkdirs(dir);
            File f = new File(dir, ".curlrc");
            write(f, rc.toString());
            f.setReadable(false, false);
            f.setReadable(true, true);
            return dir.getPath();
        } catch (IOException e) {
            return null;
        }
    }

    /**
     * The shell for VELES_SHELL and SHELL: files/bin/bash when the APK
     * carries bash (arm64), else busybox's files/bin/ash, else null (the
     * engine's platform default, mksh). The files/bin link when it was
     * made, the library itself when it was not.
     */
    private static String shellFor(Context c, String bin) {
        String[][] ladder = {{"bash", "libbash.so"}, {"ash", "libbusybox.so"}};
        for (String[] s : ladder) {
            String target = nativeDir(c) + "/" + s[1];
            if (!new File(target).canExecute()) {
                continue;
            }
            File named = new File(bin, s[0]);
            if (named.exists()) {
                return named.getPath();
            }
            if (s[0].equals("bash")) {
                return target;
            }
        }
        return null;
    }

    /**
     * files/modules, one directory per module the APK carries, each with
     * a plugin.toml whose `entry` is the ABSOLUTE path of the module's
     * .so in the native library directory. Rewritten per start (a
     * reinstall moves that directory); a module the APK no longer
     * carries loses its directory. Idempotent.
     */
    static String modulesDir(Context c) {
        File root = new File(c.getFilesDir(), "modules");
        try {
            mkdirs(root);
            File lib = new File(nativeDir(c));
            String[] names = lib.list();
            java.util.Set<String> present = new java.util.HashSet<>();
            if (names != null) {
                for (String f : names) {
                    if (!f.startsWith("libveles_module_") || !f.endsWith(".so")) {
                        continue;
                    }
                    String name = f.substring("libveles_module_".length(), f.length() - 3);
                    // A crate's dylib spells the dashes of its name as underscores;
                    // the module's own manifest keeps them (lang-js, R9), and the
                    // host adopts a directory only under the manifest's name.
                    name = name.replace('_', '-');
                    present.add(name);
                    File dir = new File(root, name);
                    mkdirs(dir);
                    String entry = new File(lib, f).getPath();
                    write(new File(dir, "plugin.toml"),
                            "name = \"" + name + "\"\nkind = \"module\"\nentry = \"" + entry + "\"\n");
                }
            }
            File[] stale = root.listFiles();
            if (stale != null) {
                for (File d : stale) {
                    if (d.isDirectory() && !present.contains(d.getName())) {
                        deleteTree(d);
                    }
                }
            }
        } catch (IOException ignored) {
            // No modules then: the gateway runs without them and says so.
        }
        return root.getPath();
    }

    /**
     * The APK's static tools, by the name each gets in files/bin and the
     * lib*.so it is (flake.nix: velesRgAndroid, velesGitAndroid and the
     * toolbox after it). Android extracts only lib-prefixed .so files, so
     * every program ships under a library's name. rg is on every ABI; the
     * rest are arm64-only and simply absent on the emulator.
     */
    static final String[][] TOOLS = {
        {"rg", "librg.so"},
        {"git", "libgit.so"},
        {"bash", "libbash.so"},
        {"jq", "libjq.so"},
        {"curl", "libcurl_cli.so"},
        {"sqlite3", "libsqlite3_cli.so"},
        {"ssh", "libssh.so"},
        {"scp", "libscp.so"},
    };

    /** files/bin, with busybox's applets and every TOOLS entry linked. Idempotent. */
    static String binDir(Context c) {
        File bin = new File(c.getFilesDir(), "bin");
        try {
            mkdirs(bin);
        } catch (IOException ignored) {
            // The links are a convenience for terminal commands; the
            // engine itself has VELES_RG and VELES_TCLSH. No reason to
            // fail the boot.
        }
        linkBusybox(c, bin);
        linkTools(c, bin);
        linkGitCore(c);
        return bin.getPath();
    }

    /**
     * Each TOOLS entry the APK carries, linked by name into files/bin,
     * made or corrected (a reinstall moves the native directory). A link
     * whose library this APK no longer carries is removed.
     */
    private static void linkTools(Context c, File bin) {
        for (String[] t : TOOLS) {
            String target = nativeDir(c) + "/" + t[1];
            File named = new File(bin, t[0]);
            try {
                if (new File(target).canExecute()) {
                    link(named, target);
                } else if (java.nio.file.Files.isSymbolicLink(named.toPath())
                        && java.nio.file.Files.readSymbolicLink(named.toPath()).toString()
                                .endsWith("/" + t[1])) {
                    java.nio.file.Files.delete(named.toPath());
                }
            } catch (IOException ignored) {
                // A convenience; the next start tries again.
            }
        }
    }

    /** Whether {@code name} is a TOOLS entry this APK carries (busybox leaves it alone). */
    private static boolean shippedTool(Context c, String name) {
        for (String[] t : TOOLS) {
            if (t[0].equals(name) && new File(nativeDir(c) + "/" + t[1]).canExecute()) {
                return true;
            }
        }
        return false;
    }

    /** One more git setting through the environment (GIT_CONFIG_COUNT/KEY_n/VALUE_n). */
    private static void gitConfig(Map<String, String> env, String key, String value) {
        int n = 0;
        try {
            n = Integer.parseInt(env.getOrDefault("GIT_CONFIG_COUNT", "0"));
        } catch (NumberFormatException ignored) {
            // Not ours: start over.
        }
        env.put("GIT_CONFIG_KEY_" + n, key);
        env.put("GIT_CONFIG_VALUE_" + n, value);
        env.put("GIT_CONFIG_COUNT", Integer.toString(n + 1));
    }

    /** The CA files of {@code dir} as one PEM file in app storage; null on failure. */
    private static String caBundle(Context c, File dir) {
        File[] certs = dir.listFiles();
        if (certs == null || certs.length == 0) {
            return null;
        }
        java.util.Arrays.sort(certs);
        File out = new File(c.getFilesDir(), "ca-bundle.pem");
        File tmp = new File(c.getFilesDir(), "ca-bundle.pem.tmp");
        try (OutputStream o = new FileOutputStream(tmp)) {
            for (File f : certs) {
                if (f.isFile()) {
                    o.write(java.nio.file.Files.readAllBytes(f.toPath()));
                    o.write('\n');
                }
            }
        } catch (IOException e) {
            return null;
        }
        return tmp.renameTo(out) ? out.getPath() : null;
    }

    /** {@code link} → {@code target}, made or corrected. */
    private static void link(File link, String target) throws IOException {
        java.nio.file.Path p = link.toPath();
        if (java.nio.file.Files.isSymbolicLink(p)) {
            if (java.nio.file.Files.readSymbolicLink(p).toString().equals(target)) {
                return;
            }
            java.nio.file.Files.delete(p);
        } else if (java.nio.file.Files.exists(p)) {
            return;
        }
        java.nio.file.Files.createSymbolicLink(p, java.nio.file.Paths.get(target));
    }

    /**
     * git's own helpers (PLAN-sessions-and-phone.md A3: plugin rollback,
     * git installs, the Review pane; `git` itself is a TOOLS entry): in
     * files/libexec/git-core the programs git runs by name — the HTTPS
     * helper, and its own upload-pack and receive-pack (a local clone),
     * which are git itself under another name.
     */
    private static void linkGitCore(Context c) {
        String git = nativeDir(c) + "/libgit.so";
        String http = nativeDir(c) + "/libgit_remote_http.so";
        if (!new File(git).canExecute()) {
            return;
        }
        try {
            File core = new File(c.getFilesDir(), "libexec/git-core");
            mkdirs(core);
            for (String name : new String[] {"git", "git-upload-pack", "git-receive-pack",
                    "git-upload-archive"}) {
                link(new File(core, name), git);
            }
            if (new File(http).canExecute()) {
                for (String name : new String[] {"git-remote-http", "git-remote-https",
                        "git-remote-ftp", "git-remote-ftps"}) {
                    link(new File(core, name), http);
                }
            }
        } catch (IOException ignored) {
            // A convenience; the next start tries again.
        }
    }

    /**
     * busybox's applets beside the TOOLS (PLAN-sessions-and-phone.md A3,
     * the better shell): awk, vi, less, wget and the rest that Android's
     * toybox lacks. files/bin is FIRST on PATH (gateway()), so an applet
     * wins over its toybox twin, whose flags differ (HANDOFF pitfall 77);
     * a command only toybox has still resolves from /system/bin. A name a
     * shipped TOOLS entry has (bash, if busybox ever lists it) is left to
     * that tool. The applets come from `libbusybox.so --list`, run once
     * per install (a stamp names the library linked).
     */
    private static void linkBusybox(Context c, File bin) {
        String target = nativeDir(c) + "/libbusybox.so";
        if (!new File(target).canExecute()) {
            return;
        }
        File stamp = new File(bin, ".busybox");
        try {
            if (new String(java.nio.file.Files.readAllBytes(stamp.toPath()),
                    StandardCharsets.UTF_8).trim().equals(target)) {
                return;
            }
        } catch (IOException ignored) {
            // No stamp yet.
        }
        try {
            Process p = new ProcessBuilder(target, "--list").redirectErrorStream(true).start();
            java.util.List<String> applets = new java.util.ArrayList<>();
            try (java.io.BufferedReader r = new java.io.BufferedReader(
                    new java.io.InputStreamReader(p.getInputStream(), StandardCharsets.UTF_8))) {
                for (String l; (l = r.readLine()) != null; ) {
                    if (l.matches("[a-z0-9][a-z0-9_.\\[\\-]*")) {
                        applets.add(l);
                    }
                }
            }
            int code = p.waitFor();
            // An app's own processes run under Android's seccomp filter,
            // which run-as does not apply: a static binary it refuses
            // dies there and nowhere else. Say what came back.
            append(log(c), "veles-android: busybox --list gave " + applets.size()
                    + " applets (exit " + code + ")");
            applets.add("busybox");
            for (String name : applets) {
                if (shippedTool(c, name)) {
                    continue;
                }
                java.nio.file.Path link = new File(bin, name).toPath();
                if (java.nio.file.Files.isSymbolicLink(link)) {
                    java.nio.file.Files.delete(link);
                } else if (java.nio.file.Files.exists(link)) {
                    continue;
                }
                java.nio.file.Files.createSymbolicLink(link, java.nio.file.Paths.get(target));
            }
            write(stamp, target);
        } catch (IOException | InterruptedException ignored) {
            // A convenience; the next start tries again.
        }
    }

    /** The last {@code max} bytes of the log, from a line start. */
    static String tail(File f, int max) {
        try (RandomAccessFile r = new RandomAccessFile(f, "r")) {
            long len = r.length();
            long from = Math.max(0, len - max);
            byte[] b = new byte[(int) (len - from)];
            r.seek(from);
            r.readFully(b);
            String s = new String(b, StandardCharsets.UTF_8);
            if (from > 0) {
                int nl = s.indexOf('\n');
                s = nl >= 0 ? s.substring(nl + 1) : s;
            }
            return s;
        } catch (IOException e) {
            return "";
        }
    }

    /** Keeps one previous log, so a restart loop cannot fill the disk. */
    static void rotate(File f, long max) {
        if (f.length() > max) {
            File old = new File(f.getPath() + ".1");
            old.delete();
            f.renameTo(old);
        }
    }

    static void append(File f, String line) {
        try (OutputStream o = new FileOutputStream(f, true)) {
            o.write((line + "\n").getBytes(StandardCharsets.UTF_8));
        } catch (IOException ignored) {
            // The log is for the person reading it; nothing to do.
        }
    }

    /**
     * DESKTOP_BEARER_TOKEN from the gateway's .env, as android/desktop.tcl
     * reads it: what the service's own clients (Notifier, PhoneEvents,
     * ServiceDevice) send to the gateway on this phone. Empty until the
     * gateway has written it.
     */
    static String bearer(Context c) {
        File env = new File(home(c), ".env");
        try {
            for (String line : java.nio.file.Files.readAllLines(env.toPath(), StandardCharsets.UTF_8)) {
                String l = line.trim();
                if (l.startsWith("DESKTOP_BEARER_TOKEN=")) {
                    String v = l.substring("DESKTOP_BEARER_TOKEN=".length()).trim();
                    if (v.length() >= 2 && (v.charAt(0) == '"' || v.charAt(0) == '\'')
                            && v.charAt(v.length() - 1) == v.charAt(0)) {
                        v = v.substring(1, v.length() - 1);
                    }
                    return v;
                }
            }
        } catch (IOException e) {
            return "";
        }
        return "";
    }

    private static String read(File f) {
        try (InputStream in = new FileInputStream(f)) {
            byte[] b = new byte[64];
            int n = in.read(b);
            return n > 0 ? new String(b, 0, n, StandardCharsets.UTF_8) : "";
        } catch (IOException e) {
            return "";
        }
    }

    private static void write(File f, String s) throws IOException {
        try (OutputStream o = new FileOutputStream(f)) {
            o.write(s.getBytes(StandardCharsets.UTF_8));
        }
    }

    private static void mkdirs(File d) throws IOException {
        if (!d.isDirectory() && !d.mkdirs()) {
            throw new IOException("cannot create " + d);
        }
    }

    private static void deleteTree(File f) {
        File[] kids = f.listFiles();
        if (kids != null) {
            for (File k : kids) {
                deleteTree(k);
            }
        }
        f.delete();
    }
}
