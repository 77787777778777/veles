# android-install.sh — sign the APK with this machine's key and adb install it.
#
#   nix run .#android-install                 # the debug APK, to the one phone adb sees
#   nix run .#android-install -- --release    # the release variant
#   nix run .#android-install -- -s SERIAL    # pick a device
#   nix run .#android-install-userland        # the userland APK, same key
#                                             # (docs/phone.md "The userland package")
#
# The flake (apps.android-install) runs this with adb, keytool and the
# SDK's apksigner pinned, and names the APK it built in VELES_APK_DIR.
#
# THE KEY. Android only installs an update signed with the same key as
# what is installed, and a new key means uninstalling first, which
# deletes the app's home (config, state.db). So:
#   ${XDG_CONFIG_HOME:-~/.config}/veles-android/release.p12   (0600)
#     when it exists: the key the published releases are signed with
#     (alias `release`, password in release.pass beside it). A build
#     from here then upgrades a release install in place, and back.
#   ${XDG_CONFIG_HOME:-~/.config}/veles-android/sideload.p12  (0600)
#     otherwise: this machine's own key, made on first use. Keep it.
#     The file mode is what protects it; its password is fixed, as
#     Android's own debug key's is. Its certificate names no host: the
#     subject is public in every APK it signs.
#
# Getting a config onto the phone (debug APK only — run-as needs it):
#   adb shell run-as chat.janitor.veles mkdir -p files/home
#   adb shell run-as chat.janitor.veles sh -c 'cat > files/home/config.toml' < config.toml
# and the desktop API, which listens on the phone's 127.0.0.1:8642:
#   adb forward tcp:18642 tcp:8642
#   adb shell run-as chat.janitor.veles cat files/home/.env   # its bearer token

set -euo pipefail

: "${VELES_APK_DIR:?run this through nix run .#android-install}"
: "${VELES_BUILD_TOOLS:?run this through nix run .#android-install}"

# The userland APK (apps.android-install-userland) names its own package
# and file stem; both default to the gateway's.
package=${VELES_APK_PACKAGE:-chat.janitor.veles}
stem=${VELES_APK_STEM:-veles}
variant=debug
serial=()
while [ $# -gt 0 ]; do
    case $1 in
        --release) variant=release ;;
        -s) serial=(-s "$2"); shift ;;
        -h|--help) echo "usage: nix run .#android-install -- [--release] [-s SERIAL]"; exit 0 ;;
        *) echo "android-install: unknown argument $1" >&2; exit 2 ;;
    esac
    shift
done

keydir=${XDG_CONFIG_HOME:-$HOME/.config}/veles-android
if [ -f "$keydir/release.p12" ] && [ -f "$keydir/release.pass" ]; then
    key=$keydir/release.p12
    alias=release
    ks_pass=file:$keydir/release.pass
else
    key=$keydir/sideload.p12
    alias=sideload
    ks_pass=pass:veles-sideload
    if [ ! -f "$key" ]; then
        mkdir -p "$keydir"
        chmod 700 "$keydir"
        (umask 077 && keytool -genkeypair -keystore "$key" -storetype PKCS12 \
            -storepass veles-sideload -keypass veles-sideload -alias sideload -keyalg RSA \
            -keysize 3072 -validity 10000 -dname "CN=veles sideload" >/dev/null)
        echo "android-install: made the sideload key $key — keep it (scripts/android-install.sh says why)"
    fi
fi
echo "android-install: signing with $key"

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
apk=$work/$stem-$variant.apk
"$VELES_BUILD_TOOLS/apksigner" sign --ks "$key" --ks-pass "$ks_pass" \
    --ks-key-alias "$alias" --out "$apk" "$VELES_APK_DIR/$stem-$variant-unsigned.apk"
"$VELES_BUILD_TOOLS/apksigner" verify "$apk"

devices=$(adb "${serial[@]}" devices | awk 'NR > 1 && $2 == "device"' | wc -l)
if [ "$devices" -eq 0 ]; then
    echo "android-install: adb sees no device. Enable USB debugging (or wireless" >&2
    echo "debugging: adb pair, then adb connect) and accept this computer's key." >&2
    echo "The signed APK is not kept; rerun when the phone is there." >&2
    exit 1
fi
if [ "$devices" -gt 1 ] && [ ${#serial[@]} -eq 0 ]; then
    echo "android-install: several devices; pick one with -s:" >&2
    adb devices >&2
    exit 1
fi

# -r keeps the app's data. The install starts no engine (nothing does
# at boot or update any more); opening MainActivity below leaves the
# controls on screen, and Start there — or opening the desktop —
# starts it.
adb "${serial[@]}" install -r "$apk"
adb "${serial[@]}" shell am start -n "$package/.MainActivity" >/dev/null
echo "android-install: installed the $variant APK on $(adb "${serial[@]}" get-serialno)"
