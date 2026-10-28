package chat.janitor.veles.userland;

import android.content.pm.PackageManager;
import android.net.LocalSocket;
import android.net.LocalSocketAddress;
import android.os.Binder;
import android.os.IBinder;
import android.os.Parcel;
import android.os.ParcelFileDescriptor;
import android.os.Parcelable;
import android.os.RemoteException;
import android.os.SystemClock;

import java.io.FileDescriptor;
import java.io.IOException;
import java.nio.ByteBuffer;
import java.nio.charset.StandardCharsets;

/**
 * The gateway's way in (PLAN-phone-userland.md T3 §4): what
 * UserlandService returns from onBind. The two apps' SELinux domains may
 * not connect to each other's sockets (T3 §2 R1b), but Binder may carry a
 * descriptor from one to the other, so a session is a socketpair made
 * here: one end goes to the broker on its handoff socket, the other back
 * to the caller, which talks the broker's protocol on it
 * (crates/veles-userland, proto.rs draws the hops).
 *
 * Hand-written, not AIDL (the build has no aidl step); the gateway's
 * UserlandRelay.java makes the same transaction:
 *
 * <pre>
 *   code   FIRST_CALL_TRANSACTION (openSession), interface DESCRIPTOR, no arguments
 *   reply  int 0, then the session's socket (ParcelFileDescriptor.writeToParcel)
 *          int 1, then a String: why not
 * </pre>
 *
 * Only the gateway gets one: the bind needs the signature permission, and
 * each call is checked again here (the permission, and the calling uid
 * against chat.janitor.veles's, signed with this package's key).
 */
final class Sessions extends Binder {
    static final String DESCRIPTOR = "chat.janitor.veles.userland.Sessions";
    static final int OPEN_SESSION = IBinder.FIRST_CALL_TRANSACTION;
    static final String PERMISSION = "chat.janitor.veles.userland.permission.OPEN_SESSION";
    /** The broker's handoff socket (proto::HANDOFF_SOCKET). */
    static final String HANDOFF = "veles-userland-handoff";
    /** How long a call waits for a broker that is still starting. */
    private static final long BROKER_WAIT_MS = 15_000;

    private final UserlandService service;

    Sessions(UserlandService service) {
        this.service = service;
    }

    @Override
    protected boolean onTransact(int code, Parcel data, Parcel reply, int flags)
            throws RemoteException {
        if (code != OPEN_SESSION) {
            return super.onTransact(code, data, reply, flags);
        }
        data.enforceInterface(DESCRIPTOR);
        ParcelFileDescriptor session;
        try {
            session = open(Binder.getCallingUid());
        } catch (IOException | RuntimeException e) {
            UserlandService.append(UserlandService.log(service), "userland: no session: " + e.getMessage());
            reply.writeInt(1);
            reply.writeString(String.valueOf(e.getMessage()));
            return true;
        }
        reply.writeInt(0);
        // Closed here once the reply is sent: the caller has its own copy.
        session.writeToParcel(reply, Parcelable.PARCELABLE_WRITE_RETURN_VALUE);
        return true;
    }

    private ParcelFileDescriptor open(int caller) throws IOException {
        if (service.checkCallingPermission(PERMISSION) != PackageManager.PERMISSION_GRANTED) {
            throw new IOException("uid " + caller + " does not hold " + PERMISSION);
        }
        int gateway = service.gatewayUid();
        if (caller != gateway) {
            throw new IOException("uid " + caller + " is not " + UserlandService.GATEWAY
                    + "'s (" + gateway + ")");
        }
        if (service.isStopping()) {
            throw new IOException("Veles userland is stopped (open it and press Start)");
        }
        ParcelFileDescriptor[] pair = ParcelFileDescriptor.createSocketPair();
        try {
            handOff(pair[1], caller);
        } catch (IOException e) {
            pair[0].close();
            throw e;
        } finally {
            // In flight to the broker, or refused: either way not ours.
            pair[1].close();
        }
        return pair[0];
    }

    /**
     * The broker's end, to the broker: a Handoff frame naming the caller,
     * the descriptor riding on it. Waits for a broker that is still
     * starting (the bind that started the service is usually what this
     * call follows).
     */
    private void handOff(ParcelFileDescriptor end, int caller) throws IOException {
        LocalSocketAddress addr =
                new LocalSocketAddress(HANDOFF, LocalSocketAddress.Namespace.ABSTRACT);
        long until = SystemClock.elapsedRealtime() + BROKER_WAIT_MS;
        LocalSocket s = new LocalSocket();
        try {
            for (;;) {
                try {
                    s.connect(addr);
                    break;
                } catch (IOException e) {
                    s.close();
                    if (service.isStopping() || SystemClock.elapsedRealtime() > until) {
                        throw new IOException("the broker does not take sessions on @" + HANDOFF
                                + " (" + e.getMessage() + ")");
                    }
                    SystemClock.sleep(200);
                    s = new LocalSocket();
                }
            }
            int holder = s.getPeerCredentials().getUid();
            if (holder != android.os.Process.myUid()) {
                throw new IOException("@" + HANDOFF + " is held by uid " + holder
                        + ", not this app's broker");
            }
            s.setFileDescriptorsForSend(new FileDescriptor[] {end.getFileDescriptor()});
            s.getOutputStream().write(frame("{\"caller_uid\":" + caller + "}"));
        } finally {
            s.close();
        }
    }

    /** One frame of the broker's wire: a 4-byte big-endian length, then the JSON. */
    static byte[] frame(String json) {
        byte[] body = json.getBytes(StandardCharsets.UTF_8);
        return ByteBuffer.allocate(4 + body.length).putInt(body.length).put(body).array();
    }
}
