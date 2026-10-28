package chat.janitor.veles;

import android.content.ComponentName;
import android.content.Context;
import android.content.Intent;
import android.content.ServiceConnection;
import android.net.LocalServerSocket;
import android.net.LocalSocket;
import android.net.LocalSocketAddress;
import android.os.IBinder;
import android.os.Parcel;
import android.os.ParcelFileDescriptor;
import android.os.RemoteException;
import android.os.SystemClock;

import org.json.JSONObject;

import java.io.FileDescriptor;
import java.io.IOException;
import java.io.OutputStream;
import java.nio.ByteBuffer;
import java.nio.charset.StandardCharsets;

/**
 * The gateway's end of the userland transport (PLAN-phone-userland.md T3
 * §4). This app's SELinux domain may not connect to the userland app's
 * sockets (T3 §2 R1b), but Binder may carry a descriptor between the two:
 * this binds the userland's UserlandService (behind its signature
 * permission; binding starts it) and serves @veles-userland-local to this
 * app's own processes. Each ulsh that connects there (ULSH_RELAY) gets a
 * session the service made: the service's openSession returns one end of
 * a socketpair whose other end the broker holds, and it rides to ulsh on
 * a Relay frame (crates/veles-userland proto.rs: {@code "opened"} with the
 * descriptor, or {@code {"refused": why}}). ulsh speaks the broker's
 * protocol on it from then on; nothing passes through here.
 *
 * One per process: Userland.shell starts it, GatewayService.onDestroy
 * stops it.
 */
final class UserlandRelay {
    /** proto::RELAY_SOCKET. */
    static final String SOCKET = "veles-userland-local";
    /** The userland's Sessions binder (android/userland Sessions.java). */
    private static final String DESCRIPTOR = "chat.janitor.veles.userland.Sessions";
    private static final int OPEN_SESSION = IBinder.FIRST_CALL_TRANSACTION;
    /** How long a session waits for the binding (the first one starts the userland). */
    private static final long BIND_WAIT_MS = 15_000;

    private static UserlandRelay current;

    private final Context app;
    private final Object lock = new Object();
    /** Guarded by lock: the userland's binder while bound. */
    private IBinder sessions;
    private volatile boolean closed;
    private LocalServerSocket server;

    private final ServiceConnection connection = new ServiceConnection() {
        @Override
        public void onServiceConnected(ComponentName name, IBinder b) {
            synchronized (lock) {
                sessions = b;
                lock.notifyAll();
            }
            Engine.append(Engine.log(app), "veles-android: bound the userland package");
        }

        @Override
        public void onServiceDisconnected(ComponentName name) {
            // Its process died; Android binds it again (BIND_AUTO_CREATE).
            synchronized (lock) {
                sessions = null;
            }
            Engine.append(Engine.log(app), "veles-android: the userland package went away");
        }

        @Override
        public void onBindingDied(ComponentName name) {
            // Updated or force-stopped: this binding is over, a new one is not.
            synchronized (lock) {
                sessions = null;
            }
            if (!closed) {
                app.unbindService(this);
                bind();
            }
        }
    };

    private UserlandRelay(Context app) {
        this.app = app;
    }

    /** The relay, bound and serving; the same one until {@link #stop}. */
    static synchronized UserlandRelay start(Context c) throws IOException {
        if (current == null) {
            UserlandRelay r = new UserlandRelay(c.getApplicationContext());
            r.open();
            current = r;
        }
        return current;
    }

    static synchronized void stop() {
        if (current != null) {
            current.close();
            current = null;
        }
    }

    private void open() throws IOException {
        server = new LocalServerSocket(SOCKET);
        try {
            if (!bind()) {
                throw new IOException("Android would not bind " + Userland.PACKAGE);
            }
        } catch (IOException | RuntimeException e) {
            server.close();
            throw e;
        }
        Thread t = new Thread(this::accept, "veles-userland-relay");
        t.setDaemon(true);
        t.start();
    }

    private boolean bind() {
        Intent i = new Intent().setComponent(
                new ComponentName(Userland.PACKAGE, Userland.PACKAGE + ".UserlandService"));
        return app.bindService(i, connection, Context.BIND_AUTO_CREATE);
    }

    private void close() {
        closed = true;
        synchronized (lock) {
            sessions = null;
            lock.notifyAll();
        }
        try {
            app.unbindService(connection);
        } catch (IllegalArgumentException ignored) {
            // Not bound (any more).
        }
        // A connect wakes the accept loop, which then sees closed.
        try (LocalSocket poke = new LocalSocket()) {
            poke.connect(new LocalSocketAddress(SOCKET, LocalSocketAddress.Namespace.ABSTRACT));
        } catch (IOException ignored) {
            // Already gone.
        }
        try {
            server.close();
        } catch (IOException ignored) {
            // Closing.
        }
    }

    private void accept() {
        while (!closed) {
            LocalSocket s;
            try {
                s = server.accept();
            } catch (IOException e) {
                if (closed) {
                    return;
                }
                Engine.append(Engine.log(app), "veles-android: userland relay: " + e);
                SystemClock.sleep(100);
                continue;
            }
            if (closed) {
                closeQuietly(s);
                return;
            }
            new Thread(() -> serve(s), "veles-userland-session").start();
        }
    }

    /** One ulsh: a session, or why not. Only this app's own processes are answered. */
    private void serve(LocalSocket s) {
        try {
            if (s.getPeerCredentials().getUid() != android.os.Process.myUid()) {
                return;
            }
            OutputStream out = s.getOutputStream();
            ParcelFileDescriptor session;
            try {
                session = openSession();
            } catch (IOException e) {
                out.write(frame("{\"refused\":" + JSONObject.quote(String.valueOf(e.getMessage())) + "}"));
                return;
            }
            try {
                s.setFileDescriptorsForSend(new FileDescriptor[] {session.getFileDescriptor()});
                out.write(frame("\"opened\""));
            } finally {
                // ulsh has its own copy now; the broker must see ulsh's
                // hang-up as the session's end, so none stays here.
                session.close();
            }
        } catch (IOException e) {
            Engine.append(Engine.log(app), "veles-android: userland relay: " + e);
        } finally {
            closeQuietly(s);
        }
    }

    /** The userland service's openSession (android/userland Sessions.java). */
    private ParcelFileDescriptor openSession() throws IOException {
        IBinder b;
        synchronized (lock) {
            long until = SystemClock.elapsedRealtime() + BIND_WAIT_MS;
            long left;
            while (sessions == null && !closed
                    && (left = until - SystemClock.elapsedRealtime()) > 0) {
                try {
                    lock.wait(left);
                } catch (InterruptedException e) {
                    Thread.currentThread().interrupt();
                    break;
                }
            }
            b = sessions;
        }
        if (b == null) {
            throw new IOException("the userland package is not bound (installed, signed alike,"
                    + " and allowed to run?)");
        }
        Parcel data = Parcel.obtain();
        Parcel reply = Parcel.obtain();
        try {
            data.writeInterfaceToken(DESCRIPTOR);
            if (!b.transact(OPEN_SESSION, data, reply, 0)) {
                throw new IOException("the userland service has no openSession (an older Veles userland?)");
            }
            if (reply.readInt() != 0) {
                throw new IOException("the userland service refused: " + reply.readString());
            }
            ParcelFileDescriptor pfd = ParcelFileDescriptor.CREATOR.createFromParcel(reply);
            if (pfd == null) {
                throw new IOException("the userland service's reply carried no descriptor");
            }
            return pfd;
        } catch (RemoteException | RuntimeException e) {
            // A reply whose descriptor the system would not hand over
            // fails the whole transaction: this is where it shows.
            throw new IOException("openSession: " + e, e);
        } finally {
            data.recycle();
            reply.recycle();
        }
    }

    /** One frame of the broker's wire: a 4-byte big-endian length, then the JSON. */
    private static byte[] frame(String json) {
        byte[] body = json.getBytes(StandardCharsets.UTF_8);
        return ByteBuffer.allocate(4 + body.length).putInt(body.length).put(body).array();
    }

    private static void closeQuietly(LocalSocket s) {
        try {
            s.close();
        } catch (IOException ignored) {
            // Closing.
        }
    }
}
