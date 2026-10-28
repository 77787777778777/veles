package chat.janitor.veles.userland;

import android.content.Context;
import android.net.ConnectivityManager;
import android.net.LinkProperties;
import android.net.Network;
import android.system.ErrnoException;
import android.system.Os;

import java.io.BufferedInputStream;
import java.io.EOFException;
import java.io.File;
import java.io.FileInputStream;
import java.io.FileOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.net.InetAddress;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.List;

/**
 * files/rootfs: the guest, unpacked from assets/rootfs.tar on first run.
 *
 * ONCE, not per install: after the first start the rootfs is the
 * person's (what `apk add` put there), and an update of this app must
 * not wipe it. A stamp at its top says it is complete; a run that died
 * half way leaves files/rootfs.partial, which the next one deletes.
 *
 * The tar is the build's (flake.nix, userlandRootfs): plain ustar of
 * regular files, directories, symlinks and hard links, nothing else.
 */
final class Rootfs {
    private Rootfs() {}

    static final String STAMP = ".veles-rootfs";

    static File dir(Context c) {
        return new File(c.getFilesDir(), "rootfs");
    }

    /** Bound at /shared in the guest. */
    static File shared(Context c) {
        return new File(c.getFilesDir(), "shared");
    }

    /** proot's PROOT_TMP_DIR. */
    static File tmp(Context c) {
        return new File(c.getCacheDir(), "proot");
    }

    /**
     * The broker's --sandboxes: veles' named sandboxes (`[sandbox] backend
     * = "userland"`), each a guest root of its own that the broker makes
     * from a copy of files/rootfs the first time the name is used.
     */
    static File sandboxes(Context c) {
        return new File(c.getFilesDir(), "sandboxes");
    }

    /**
     * Unpacks the rootfs if it is not there yet, then (every start) the
     * guest's resolv.conf from the active network's DNS servers. Returns
     * what it did, for the log.
     */
    static String ensure(Context c) throws IOException {
        File root = dir(c);
        String did;
        if (new File(root, STAMP).isFile()) {
            did = "rootfs: present";
        } else {
            File partial = new File(c.getFilesDir(), "rootfs.partial");
            deleteTree(partial);
            deleteTree(root);
            long t0 = System.nanoTime();
            long[] counts;
            try (InputStream in = new BufferedInputStream(c.getAssets().open("rootfs.tar"), 1 << 16)) {
                counts = untar(in, partial);
            }
            mkdirs(new File(partial, "tmp"));
            chmod(new File(partial, "tmp"), 01777);
            write(new File(partial, STAMP), "alpine-minirootfs from assets/rootfs.tar\n");
            if (!partial.renameTo(root)) {
                throw new IOException("cannot rename " + partial + " to " + root);
            }
            long ms = (System.nanoTime() - t0) / 1_000_000;
            // The spike's measurement (PLAN-phone-userland.md T3, gate T3-R1).
            did = "rootfs: extracted " + counts[0] + " entries, " + (counts[1] >> 10)
                    + " KiB in " + ms + " ms";
        }
        mkdirs(shared(c));
        mkdirs(tmp(c));
        mkdirs(sandboxes(c));
        mkdirs(new File(root, "shared"));
        writeResolvConf(c, new File(root, "etc/resolv.conf"));
        return did;
    }

    /** The active network's DNS servers, else two public ones. */
    private static void writeResolvConf(Context c, File f) throws IOException {
        StringBuilder sb = new StringBuilder("# written by veles userland at each start\n");
        int n = 0;
        try {
            ConnectivityManager cm = c.getSystemService(ConnectivityManager.class);
            Network net = cm == null ? null : cm.getActiveNetwork();
            LinkProperties lp = net == null ? null : cm.getLinkProperties(net);
            if (lp != null) {
                for (InetAddress a : lp.getDnsServers()) {
                    String h = a.getHostAddress();
                    // musl takes no %scope on a nameserver line.
                    if (h != null && h.indexOf('%') < 0) {
                        sb.append("nameserver ").append(h).append('\n');
                        n++;
                    }
                }
            }
        } catch (RuntimeException e) {
            // No network service answer: the fallback below.
        }
        if (n == 0) {
            sb.append("nameserver 1.1.1.1\nnameserver 9.9.9.9\n");
        }
        f.delete(); // Alpine may ship it as a symlink.
        write(f, sb.toString());
    }

    /** Unpacks {@code in} under {@code root}; returns {entries, bytes}. */
    static long[] untar(InputStream in, File root) throws IOException {
        mkdirs(root);
        String top = root.getCanonicalPath() + File.separator;
        byte[] h = new byte[512];
        byte[] buf = new byte[1 << 16];
        List<File> dirs = new ArrayList<>();
        List<Integer> dirModes = new ArrayList<>();
        long entries = 0, bytes = 0;
        while (true) {
            readFully(in, h, 512);
            if (allZero(h)) {
                break;
            }
            String name = str(h, 0, 100);
            String prefix = str(h, 345, 155);
            if (!prefix.isEmpty()) {
                name = prefix + "/" + name;
            }
            int mode = (int) octal(h, 100, 8) & 07777;
            long size = octal(h, 124, 12);
            char type = (char) h[156];
            String link = str(h, 157, 100);
            while (name.startsWith("./")) {
                name = name.substring(2);
            }
            if (name.equals(".") || name.isEmpty()) {
                skip(in, size);
                continue;
            }
            File out = new File(root, name);
            String canon = new File(out.getParentFile().getCanonicalPath(), out.getName()).getPath();
            if (name.startsWith("/") || name.contains("../") || name.equals("..")
                    || !canon.startsWith(top)) {
                throw new IOException("rootfs.tar entry outside the rootfs: " + name);
            }
            mkdirs(out.getParentFile());
            entries++;
            switch (type) {
                case '0':
                case '\0': {
                    try (OutputStream o = new FileOutputStream(out)) {
                        long left = size;
                        while (left > 0) {
                            int n = in.read(buf, 0, (int) Math.min(buf.length, left));
                            if (n < 0) {
                                throw new EOFException("rootfs.tar ends inside " + name);
                            }
                            o.write(buf, 0, n);
                            left -= n;
                        }
                    }
                    bytes += size;
                    skip(in, pad(size));
                    chmod(out, mode & 0777);
                    break;
                }
                case '5':
                    mkdirs(out);
                    dirs.add(out);
                    dirModes.add(mode & 0777);
                    skip(in, size + pad(size));
                    break;
                case '2':
                    out.delete();
                    try {
                        Os.symlink(link, out.getPath());
                    } catch (ErrnoException e) {
                        throw new IOException("symlink " + name + " -> " + link + ": " + e);
                    }
                    break;
                case '1': {
                    File target = new File(root, link.startsWith("./") ? link.substring(2) : link);
                    out.delete();
                    try {
                        Os.link(target.getPath(), out.getPath());
                    } catch (ErrnoException e) {
                        // An app's storage may refuse hard links: a copy.
                        copy(target, out);
                        chmod(out, mode & 0777);
                    }
                    break;
                }
                default:
                    // Devices and fifos: the guest gets the host's /dev.
                    skip(in, size + pad(size));
                    entries--;
                    break;
            }
        }
        // Directories last, so a read-only one was writable while filled.
        for (int i = dirs.size() - 1; i >= 0; i--) {
            chmod(dirs.get(i), dirModes.get(i) | 0700);
        }
        return new long[] {entries, bytes};
    }

    private static long pad(long size) {
        return (512 - (size % 512)) % 512;
    }

    private static boolean allZero(byte[] b) {
        for (byte x : b) {
            if (x != 0) {
                return false;
            }
        }
        return true;
    }

    private static String str(byte[] h, int off, int len) {
        int end = off;
        while (end < off + len && h[end] != 0) {
            end++;
        }
        return new String(h, off, end - off, StandardCharsets.UTF_8);
    }

    private static long octal(byte[] h, int off, int len) throws IOException {
        long v = 0;
        for (int i = off; i < off + len; i++) {
            byte b = h[i];
            if (b == 0 || b == ' ') {
                continue;
            }
            if (b < '0' || b > '7') {
                throw new IOException("rootfs.tar: bad number in a header");
            }
            v = v * 8 + (b - '0');
        }
        return v;
    }

    private static void readFully(InputStream in, byte[] b, int len) throws IOException {
        int off = 0;
        while (off < len) {
            int n = in.read(b, off, len - off);
            if (n < 0) {
                throw new EOFException("rootfs.tar ends inside a header");
            }
            off += n;
        }
    }

    private static void skip(InputStream in, long n) throws IOException {
        while (n > 0) {
            long s = in.skip(n);
            if (s <= 0) {
                if (in.read() < 0) {
                    throw new EOFException("rootfs.tar is cut short");
                }
                s = 1;
            }
            n -= s;
        }
    }

    private static void copy(File from, File to) throws IOException {
        byte[] buf = new byte[1 << 16];
        try (InputStream i = new FileInputStream(from); OutputStream o = new FileOutputStream(to)) {
            for (int n; (n = i.read(buf)) > 0; ) {
                o.write(buf, 0, n);
            }
        }
    }

    private static void chmod(File f, int mode) throws IOException {
        try {
            Os.chmod(f.getPath(), mode);
        } catch (ErrnoException e) {
            throw new IOException("chmod " + f + ": " + e);
        }
    }

    static void write(File f, String s) throws IOException {
        try (OutputStream o = new FileOutputStream(f)) {
            o.write(s.getBytes(StandardCharsets.UTF_8));
        }
    }

    static void mkdirs(File d) throws IOException {
        if (!d.isDirectory() && !d.mkdirs()) {
            throw new IOException("cannot create " + d);
        }
    }

    static void deleteTree(File f) {
        // Not through a symlink: the rootfs is full of absolute ones.
        if (f.isDirectory() && !isSymlink(f)) {
            File[] kids = f.listFiles();
            if (kids != null) {
                for (File k : kids) {
                    deleteTree(k);
                }
            }
        }
        f.delete();
    }

    private static boolean isSymlink(File f) {
        return java.nio.file.Files.isSymbolicLink(f.toPath());
    }
}
