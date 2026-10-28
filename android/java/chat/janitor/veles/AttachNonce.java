package chat.janitor.veles;

import java.util.UUID;

/**
 * The one-time word an "attach" intent must carry to be believed.
 *
 * DesktopActivity is exported (it is the launcher entry), so ANY app
 * can startActivity it with an "attach" extra, and the desktop's
 * `borg onintent` would attach whatever paths it names — including
 * files/home/.env. AttachActivity is the desktop's own file picker
 * (exported=false) and shares the :desktop process with it, so a word
 * both can see in this process proves the intent came from the picker:
 * AttachActivity mints one per launch and sends it as the
 * "attach_nonce" extra; the desktop reads this same field back (borg
 * queryfields reads a public static non-final field) and attaches only
 * when the extra matches, once (lib/android.tcl remembers the word it
 * spent).
 *
 * The field's NAME is the key `borg queryfields` returns it under, and
 * lib/android.tcl reads exactly that key: rename one, rename both
 * (androidtest checks the two agree).
 *
 * The picker does not clear the word after sending it: the intent
 * reaches Tcl later (onNewIntent, then Tcl's event loop), and a word
 * cleared in between matched nothing — every attach was refused.
 *
 * Public is what java.lang.Class.getFields() requires; the field is
 * reachable only inside this app's own process (Android's UID sandbox),
 * and its secrecy is the random UUID, not the modifier.
 */
public final class AttachNonce {
    /** The word the latest picker's intent must repeat; "" before any. */
    public static String nonce = "";

    private AttachNonce() {
    }

    /** A fresh word for one picker launch. */
    public static String mint() {
        nonce = UUID.randomUUID().toString();
        return nonce;
    }

    /** A picker that sends nothing: no intent's word can match. */
    public static void clear() {
        nonce = "";
    }
}
