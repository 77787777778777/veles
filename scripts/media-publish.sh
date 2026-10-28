#!/usr/bin/env bash
# media-publish.sh — give a chat surface somewhere to put files, and a URL to
# say where they went.
#
# WHY THIS EXISTS
#
# The engine's half of media delivery is two config keys and a contract:
#
#   platforms.irc.media_public_dir   a directory the gateway may COPY into
#   platforms.irc.media_public_url   the URL that directory is served at
#
# and the contract is that the gateway writes files and SOMETHING ELSE serves
# them. It will not start a web server, will not obtain a certificate, and will
# not guess your DNS — those are deployment decisions, and a bot that answers
# in a public channel handing out links to a directory nobody serves is worse
# than one that says "media not delivered". So an installation that wants links
# instead of notes has to build the serving half, and building it by hand means
# re-deriving an nginx vhost, an ACME challenge order that does not break every
# other site on the box, a sync unit, and the two keys — every time, on every
# machine.
#
# This script is that half, parameterised. Nothing in it knows which machine,
# which VPS, which DNS provider or which CA you use; every default below is
# overridable by a flag or an env var, and `--dry-run` prints the whole plan
# without touching anything.
#
# WHAT IT DOES, per mode
#
#   remote     (default) serve from a directory on an SSH host: create it,
#              write the vhost, obtain the cert, then install a systemd USER
#              path+service pair on THIS machine that rsyncs the published
#              directory over on change (with a timer fallback, because an
#              inotify unit that missed a write is a link that 404s).
#   mount      same vhost, but instead of syncing, sshfs-mount the remote
#              directory at the local publish path, so a write IS an upload.
#              Needs sshfs and FUSE locally; survives less well than rsync.
#   local      serve from a directory on THIS machine with a local nginx.
#   configure  only write the two keys — the directory is already served.
#   check      verify the chain end to end: publish a probe file, wait for it
#              to arrive, fetch it over the public URL, clean up.
#   remove     undo what this script installed (units, vhost, keys). Asks
#              before deleting anything on a remote host.
#
# WHAT THE VHOST REFUSES TO DO
#
# The served directory holds files a CHAT USER can influence the name of: the
# engine's marker gate lets a below-owner sender name a file in their own
# sandbox workspace, and users can write there. So the vhost serves media as
# media and everything else as an octet-stream download with a null CSP —
# otherwise a published `evil.html` is attacker-authored content hosted on
# your domain, and `nosniff` is no help against a file that genuinely is HTML.
# The adapter renames non-media to `.bin` before publishing; this is the same
# answer at the other end, because the two ship independently.
#
# DNS is deliberately NOT its job: providers differ, credentials differ, and a
# script that creates DNS records is a script that can take over a domain. It
# checks that the hostname resolves and prints the exact record to create if it
# does not.
#
# Usage:  scripts/media-publish.sh [mode] [options]
#         scripts/media-publish.sh --help
set -euo pipefail

# ── knobs ────────────────────────────────────────────────────────────────
MODE="${MODE:-remote}"
HOST="${MEDIA_HOST:-}"
LOCAL_DIR="${MEDIA_LOCAL_DIR:-}"
SSH_TARGET="${MEDIA_SSH:-}"
REMOTE_DIR="${MEDIA_REMOTE_DIR:-}"
WEBROOT="${MEDIA_WEBROOT:-/var/www/html/letsencrypt}"
TLS="${MEDIA_TLS:-certbot}"            # certbot | certbot-dns01 | existing | none
MAX_AGE_DAYS="${MEDIA_MAX_AGE_DAYS:-}"  # days a published link lives (empty = never)
DELETE="${MEDIA_DELETE:-0}"             # 1 = the sync passes --delete (expiry follows)
DNS_PUBLISH_CMD="${MEDIA_DNS_PUBLISH_CMD:-}"   # how a TXT record gets published (dns01)
ACME_EMAIL="${MEDIA_ACME_EMAIL:-}"
SCHEME="${MEDIA_SCHEME:-https}"
SYNC_INTERVAL="${MEDIA_SYNC_INTERVAL:-60}"
CERT_RETRIES="${MEDIA_CERT_RETRIES:-20}"
CERT_RETRY_DELAY="${MEDIA_CERT_RETRY_DELAY:-60}"
ENV_FILE="${MEDIA_ENV_FILE:-}"
SUDO="${MEDIA_SUDO:-sudo}"
SSH_OPTS="${MEDIA_SSH_OPTS:--o BatchMode=yes}"
DRY_RUN="${MEDIA_DRY_RUN:-0}"
ASSUME_YES="${MEDIA_ASSUME_YES:-0}"
SITE_NAME="${MEDIA_SITE_NAME:-veles-media}"
# Whether the files are served by THIS machine (mode local) or by an ssh host.
# Set by mode resolution, read by run_served.
SERVE_HERE=0

say()  { printf '▸ %s\n' "$*"; }
note() { printf '  %s\n' "$*"; }
die()  { printf 'media-publish: %s\n' "$*" >&2; exit 1; }

usage() {
  # The preamble is the help text: from the first comment line to the
  # "Usage:" line, found rather than hardcoded — a line range is a
  # number that goes stale the next time the header grows a paragraph.
  sed -n "2,$(grep -n '^# Usage:' "$0" | head -1 | cut -d: -f1)p" "$0" | sed 's/^# \{0,1\}//'
  cat <<'EOF'

Options (env var in parens):
  --host NAME           public hostname that will serve the files (MEDIA_HOST)
  --local-dir PATH      where the gateway publishes (MEDIA_LOCAL_DIR)
                        default: <veles data dir>/media/public
  --ssh TARGET          ssh target for remote/mount mode (MEDIA_SSH)
  --remote-dir PATH     served directory on that host (MEDIA_REMOTE_DIR)
                        default: /var/www/<host>
  --webroot PATH        ACME webroot on the serving host (MEDIA_WEBROOT)
  --tls KIND            certbot | certbot-dns01 | existing | none (MEDIA_TLS)
  --dns-publish-cmd CMD run LOCALLY to publish the ACME TXT record (dns01),
                        with ACME_DOMAIN, ACME_RECORD, ACME_TOKEN and
                        ACME_ACTION=publish|cleanup in its environment
                        (MEDIA_DNS_PUBLISH_CMD). Without it the token is
                        printed and you publish it yourself.
  --email ADDR          ACME account email, if the host has no account yet
  --scheme KIND         https | http (MEDIA_SCHEME)
  --interval SECS       rsync fallback timer for remote mode (MEDIA_SYNC_INTERVAL)
  --cert-retries N      ACME attempts before giving up (MEDIA_CERT_RETRIES).
                        Keep this low: five FAILED authorizations for one name
                        inside an hour locks it out for the rest of the hour.
  --env-file PATH       where to write the keys (MEDIA_ENV_FILE)
                        default: the .env beside veles's config.toml
  --max-age DAYS        also write IRC_MEDIA_PUBLIC_MAX_AGE_DAYS: the gateway
                        sweeps the published directory hourly and deletes
                        files older than this (MEDIA_MAX_AGE_DAYS)
  --delete              the sync passes --delete, so the SERVED copies die
                        with the local sweep. Without it, expiry is local
                        only and the served files outlive their links.
                        Re-run the script to change an installed sync. (MEDIA_DELETE)
  --sudo CMD            how to become root on the serving host (MEDIA_SUDO)
  --site-name NAME      vhost/unit name (MEDIA_SITE_NAME)
  --dry-run             print every command, change nothing
  --yes                 do not ask before removing remote files
  -h, --help            this text
EOF
}

while [ $# -gt 0 ]; do
  case "$1" in
    remote|mount|local|configure|check|remove) MODE="$1" ;;
    --host)         HOST="$2"; shift ;;
    --local-dir)    LOCAL_DIR="$2"; shift ;;
    --ssh)          SSH_TARGET="$2"; shift ;;
    --remote-dir)   REMOTE_DIR="$2"; shift ;;
    --webroot)      WEBROOT="$2"; shift ;;
    --tls)          TLS="$2"; shift ;;
    --dns-publish-cmd) DNS_PUBLISH_CMD="$2"; shift ;;
    --email)        ACME_EMAIL="$2"; shift ;;
    --scheme)       SCHEME="$2"; shift ;;
    --interval)     SYNC_INTERVAL="$2"; shift ;;
    --cert-retries) CERT_RETRIES="$2"; shift ;;
    --env-file)     ENV_FILE="$2"; shift ;;
    --sudo)         SUDO="$2"; shift ;;
    --site-name)    SITE_NAME="$2"; shift ;;
    --max-age)      MAX_AGE_DAYS="$2"; shift ;;
    --delete)       DELETE=1 ;;
    --dry-run)      DRY_RUN=1 ;;
    --yes)          ASSUME_YES=1 ;;
    -h|--help)      usage; exit 0 ;;
    *)              usage; die "unknown argument: $1" ;;
  esac
  shift
done

# ── the directories, resolved the way the engine resolves them ────────────
# VELES_HOME forces everything into one tree (that is what the OCI image
# and the test suites rely on), so honouring it here is what makes the script
# point a dev tree at a dev directory instead of the live one.
veles_data_dir() {
  if [ -n "${VELES_HOME:-}" ]; then printf '%s' "$VELES_HOME"
  elif [ -n "${XDG_DATA_HOME:-}" ]; then printf '%s/veles' "$XDG_DATA_HOME"
  else printf '%s/.local/share/veles' "$HOME"; fi
}
veles_config_dir() {
  if [ -n "${VELES_HOME:-}" ]; then printf '%s' "$VELES_HOME"
  elif [ -n "${XDG_CONFIG_HOME:-}" ]; then printf '%s/veles' "$XDG_CONFIG_HOME"
  else printf '%s/.config/veles' "$HOME"; fi
}

[ -n "$LOCAL_DIR" ] || LOCAL_DIR="$(veles_data_dir)/media/public"
[ -n "$ENV_FILE" ]  || ENV_FILE="$(veles_config_dir)/.env"
if [ -n "$MAX_AGE_DAYS" ]; then
  case "$MAX_AGE_DAYS" in
    ''|*[!0-9]*) die "--max-age wants a number of days (got: $MAX_AGE_DAYS)" ;;
  esac
fi
case "$MODE" in
  remote|mount)
    [ -n "$SSH_TARGET" ] || die "$MODE mode needs --ssh user@host"
    [ -n "$REMOTE_DIR" ] || REMOTE_DIR="/var/www/${HOST:-$SITE_NAME}"
    ;;
  local)
    REMOTE_DIR="$LOCAL_DIR"
    ;;
esac
BASE_URL="${SCHEME}://${HOST}"
if [ "$TLS" = none ]; then SCHEME="http"; BASE_URL="http://${HOST}"; fi

run() {
  if [ "$DRY_RUN" = 1 ]; then printf '  DRY %s\n' "$*"; else
    printf '  $ %s\n' "$*" >&2; "$@"
  fi
}
# One shell, on whichever host serves the files. Quoted once, here, so the
# remote command is a single argument and nothing in it is expanded twice.
run_served() {
  local script="$1"
  if [ "$SERVE_HERE" = 1 ]; then
    if [ "$DRY_RUN" = 1 ]; then printf '  DRY local: %s\n' "$script"
    else bash -c "$script"; fi
  else
    if [ "$DRY_RUN" = 1 ]; then printf '  DRY %s: %s\n' "$SSH_TARGET" "$script"
    else ssh $SSH_OPTS "$SSH_TARGET" "$script"
    fi
  fi
}
need() { command -v "$1" >/dev/null 2>&1 || die "$1 is required for this mode and is not in PATH"; }

# ── the vhost ─────────────────────────────────────────────────────────────
# Two blocks, the shape every static vhost should have: 80 does the ACME
# challenge and redirects, 443 serves the directory. `autoindex off` is
# explicit because the default is off but a directory of other people's
# pictures is exactly the place to say so out loud, and the dotfile location
# denies what a `..`-ish or stray-upload name should never reach.
vhost_static_block() {
  cat <<EOF
    root $REMOTE_DIR;
    autoindex off;

    add_header X-Content-Type-Options nosniff always;
    # Names are unique per publish (a nanosecond stamp plus a sanitized
    # original name), so a hit here is a hit forever.
    add_header Cache-Control "public, max-age=31536000, immutable" always;

    location ~ /\. { deny all; }

    # Media is served as media. Everything else is served as a DOWNLOAD.
    #
    # Why: the engine's marker gate lets a below-owner sender name a file
    # in their own sandbox workspace, and a user can write into that
    # workspace — so the bot will legitimately publish a file whose NAME
    # the sender chose. \`nosniff\` does not help against a file that
    # really is text/html: it would be attacker-authored content hosted
    # under this domain. The adapter renames anything outside its own
    # allowlist to \`.bin\` (see \`safe_published_name\`), and this block is
    # the second half of the same answer — the two ship independently,
    # so an installation running an older engine is still covered here.
    location ~* \\.(png|jpe?g|gif|webp|bmp|avif|ico|mp4|webm|mov|mkv|mp3|wav|ogg|oga|opus|flac|m4a|aac|pdf)\$ {
        try_files \$uri =404;
    }

    location / {
        # An octet-stream with a filename disposition and no scripting
        # privileges of any kind. A browser saves it; it never runs it.
        default_type application/octet-stream;
        types { }
        add_header Content-Disposition "attachment" always;
        add_header Content-Security-Policy "default-src 'none'; sandbox" always;
        # Re-stated, not forgotten: nginx's add_header is inherited only
        # while a level declares NONE of its own, so the two above would
        # silently drop the server block's headers from this location.
        add_header X-Content-Type-Options nosniff always;
        add_header Cache-Control "public, max-age=31536000, immutable" always;
        try_files \$uri =404;
    }
EOF
}
vhost_interim() {
  cat <<EOF
server {
    listen 80;
    server_name $HOST;

    location / {
      return         301 https://\$server_name\$request_uri;
    }

    include snippets/letsencrypt.conf;
}
EOF
}
vhost_full() {
  {
    cat <<EOF
server {
    listen 80;
    server_name $HOST;

    include snippets/letsencrypt.conf;

    location / {
      return         301 https://\$server_name\$request_uri;
    }
}
server {
    server_name $HOST;

    listen 443 ssl http2;
    listen [::]:443 ssl http2;
    ssl_session_timeout 1d;
    ssl_session_cache shared:MozSSL:10m;
    ssl_session_tickets off;

    ssl_trusted_certificate   /etc/letsencrypt/live/$HOST/chain.pem;
    ssl_certificate           /etc/letsencrypt/live/$HOST/fullchain.pem;
    ssl_certificate_key       /etc/letsencrypt/live/$HOST/privkey.pem;

    ssl_protocols TLSv1.2 TLSv1.3;
    ssl_prefer_server_ciphers off;
    ssl_stapling on;
    ssl_stapling_verify on;

    include snippets/letsencrypt.conf;

$(vhost_static_block)
}
EOF
  }
}
vhost_plain() {   # --tls none: one block, no certificate to fail on
  cat <<EOF
server {
    listen 80;
    server_name $HOST;

$(vhost_static_block)
}
EOF
}

write_vhost() {
  local content="$1"
  run_served "
    set -e
    $SUDO mkdir -p '$REMOTE_DIR'
    # Owned by whoever syncs into it (the ssh user), readable by the web
    # server: 755 on the directory and the gateway's own 0644 on files.
    $SUDO chown \"\$(id -un):\" '$REMOTE_DIR'
    $SUDO chmod 755 '$REMOTE_DIR'
    $SUDO tee '/etc/nginx/sites-available/$SITE_NAME' >/dev/null <<'VHOST'
$content
VHOST
    $SUDO ln -sf '/etc/nginx/sites-available/$SITE_NAME' '/etc/nginx/sites-enabled/$SITE_NAME'
    $SUDO nginx -t && $SUDO systemctl reload nginx
  "
}

# An nginx refuses to reload while an enabled vhost names a certificate that
# does not exist, and a failed reload is not a failed site — it is every site
# on the box. So the cert is obtained against a port-80-only vhost first, and
# the full one is written over it afterwards.
#
# Two validators, because the obvious one has a failure mode that costs an hour:
#
#   certbot          HTTP-01 against the webroot. Right answer normally.
#   certbot-dns01    DNS-01, for when HTTP-01 cannot complete. It happens to
#                    brand-new hostnames behind a CDN: the PRIMARY validation
#                    passes and the CA's other vantage points answer "networking
#                    error looking up A" for a name that resolves fine from
#                    every resolver you can reach. Each failed attempt is a
#                    failed AUTHORIZATION, and five of those for one identifier
#                    inside an hour locks the identifier out for the rest of it
#                    — so this path paces itself and does not retry in a loop.
obtain_cert() {
  case "$TLS" in
    certbot) obtain_cert_http01 ;;
    certbot-dns01) obtain_cert_dns01 ;;
    *) note "tls=$TLS: not obtaining a certificate" ;;
  esac
}

obtain_cert_http01() {
  local email_flag=""
  [ -n "$ACME_EMAIL" ] && email_flag="--agree-tos -m $ACME_EMAIL"
  say "obtaining a certificate for $HOST (ACME HTTP-01, webroot $WEBROOT)"
  note "if this fails with 'during secondary validation: DNS problem' on a"
  note "hostname you created minutes ago, that is the CA's resolvers, not"
  note "your config. Pace the retries (--cert-retries) or use --tls certbot-dns01."
  run_served "
    set -e
    for i in \$(seq 1 $CERT_RETRIES); do
      if $SUDO certbot certonly --webroot -w $WEBROOT -d $HOST \
           --non-interactive --keep-until-expiring $email_flag; then
        exit 0
      fi
      echo \"  attempt \$i/$CERT_RETRIES failed; sleeping ${CERT_RETRY_DELAY}s\"
      sleep $CERT_RETRY_DELAY
    done
    exit 1
  "
}

# DNS-01 without putting DNS credentials on the serving host: certbot runs there
# with an auth hook that only WAITS for the TXT record to be visible, while the
# record itself is published from HERE by --dns-publish-cmd, which is run with
# ACME_DOMAIN / ACME_TOKEN / ACME_ACTION in its environment. The hook lives in a
# root-owned directory, never /tmp: certbot runs it as root, and a root-executed
# script in a world-writable directory is a privilege escalation with a timer on
# it (the next renewal).
obtain_cert_dns01() {
  local hook_dir="/usr/local/lib/$SITE_NAME"
  local token_file="/var/run/$SITE_NAME-acme-token"
  say "obtaining a certificate for $HOST (ACME DNS-01)"
  if [ "$DRY_RUN" = 1 ]; then
    note "would install root-owned hooks in $hook_dir on the serving host,"
    note "run certbot --manual --preferred-challenges dns there, publish the"
    note "TXT from here${DNS_PUBLISH_CMD:+ with --dns-publish-cmd}, then flip renewal to the webroot"
    return 0
  fi
  if [ -z "$DNS_PUBLISH_CMD" ]; then
    note "no --dns-publish-cmd: the token will be printed and you will have to"
    note "publish the TXT record yourself, then press Enter here."
  fi
  run_served "
    set -e
    $SUDO mkdir -p '$hook_dir'
    $SUDO chown root: '$hook_dir' && $SUDO chmod 755 '$hook_dir'
    $SUDO tee '$hook_dir/acme-auth-hook.sh' >/dev/null <<'AUTHHOOK'
#!/bin/sh
# Publish nothing; wait for the operator's machine to publish the TXT.
name=\"_acme-challenge.\$CERTBOT_DOMAIN\"
printf '%s %s\n' \"\$name\" \"\$CERTBOT_VALIDATION\" > $token_file
chmod 644 $token_file
i=0
while [ \"\$i\" -lt 120 ]; do
  val=\$(dig +short TXT \"\$name\" @1.1.1.1 2>/dev/null | tr -d '\"' | head -1)
  [ \"\$val\" = \"\$CERTBOT_VALIDATION\" ] && exit 0
  i=\$((i + 1))
  sleep 5
done
exit 1
AUTHHOOK
    printf '#!/bin/sh\nexit 0\n' | $SUDO tee '$hook_dir/acme-cleanup-hook.sh' >/dev/null
    $SUDO chmod 755 '$hook_dir/acme-auth-hook.sh' '$hook_dir/acme-cleanup-hook.sh'
    $SUDO rm -f '$token_file'
    $SUDO certbot certonly --manual --preferred-challenges dns \
      --manual-auth-hook '$hook_dir/acme-auth-hook.sh' \
      --manual-cleanup-hook '$hook_dir/acme-cleanup-hook.sh' \
      -d $HOST --non-interactive --agree-tos
  " &
  local pid=$!
  # Wait for the hook to write the token, then publish it from here.
  local token="" tries=0
  while [ -z "$token" ] && [ "$tries" -lt 60 ]; do
    sleep 3; tries=$((tries + 1))
    token="$(run_served_quiet "cat $token_file 2>/dev/null" || true)"
  done
  [ -n "$token" ] || { wait "$pid" || true; die "certbot never asked for a TXT record"; }
  local name="${token%% *}" value="${token##* }"
  note "TXT $name = $value"
  if [ -n "$DNS_PUBLISH_CMD" ]; then
    say "publishing the TXT record"
    ACME_DOMAIN="$HOST" ACME_RECORD="$name" ACME_TOKEN="$value" ACME_ACTION=publish \
      bash -c "$DNS_PUBLISH_CMD"
    ACME_DOMAIN="$HOST" ACME_RECORD="$name" ACME_TOKEN="$value" ACME_ACTION=cleanup \
      bash -c "$DNS_PUBLISH_CMD" || note "cleanup step of --dns-publish-cmd failed; delete $name by hand"
  else
    printf '  publish that TXT record, then press Enter: '
    read -r _
  fi
  wait "$pid" || die "DNS-01 validation failed"
  say "certificate obtained"
  # A --manual cert renews by running the hooks again, which means renewal
  # depends on whatever published the TXT the first time. If HTTP-01 works —
  # and now that the name has existed for a while it usually does — flip the
  # renewal config to the webroot so renewal is unattended.
  note "flipping renewal to HTTP-01 so it does not depend on the hook"
  if run_served "
      $SUDO certbot certonly --webroot -w $WEBROOT -d $HOST --cert-name $HOST \
        --force-renewal --non-interactive --agree-tos >/dev/null 2>&1"; then
    note "renewal now uses the webroot ($WEBROOT)"
  else
    note "HTTP-01 still fails for this name: renewal will run the hook, so"
    note "whatever publishes the TXT has to be reachable at renewal time too."
  fi
  run_served "$SUDO rm -f '$token_file'" || true
}

# Like run_served, but capturing stdout instead of showing it.
run_served_quiet() {
  if [ "$SERVE_HERE" = 1 ]; then bash -c "$1"; else ssh $SSH_OPTS "$SSH_TARGET" "$1"; fi
}

# Write the vhost and get the certificate, in the only order that cannot take
# the box's other sites down with it: an interim port-80 vhost first (nginx can
# reload while the certificate does not exist yet), then the certificate, then
# the full two-block vhost written over the interim one. `existing` skips
# straight to the full vhost; `none` writes the single-block HTTP one.
serve_and_certify() {
  case "$TLS" in
    certbot|certbot-dns01)
      write_vhost "$(vhost_interim)"
      obtain_cert
      write_vhost "$(vhost_full)"
      ;;
    existing) write_vhost "$(vhost_full)" ;;
    none) write_vhost "$(vhost_plain)" ;;
    *) die "--tls must be one of: certbot, certbot-dns01, existing, none" ;;
  esac
}

dns_check() {
  [ -n "$HOST" ] || die "--host is required (the URL the gateway will hand out)"
  if getent hosts "$HOST" >/dev/null 2>&1; then
    note "$HOST resolves: $(getent hosts "$HOST" | head -1 | awk '{print $1}')"
  else
    note "$HOST does NOT resolve yet. Create the record first, e.g. an A"
    note "record for $HOST pointing at this serving host's public address,"
    note "then re-run. (This script never touches DNS.)"
    [ "$DRY_RUN" = 1 ] || die "$HOST does not resolve"
  fi
}

# ── the local half: how published files get to the served directory ───────
UNIT_DIR="$HOME/.config/systemd/user"
SYNC_SERVICE="$SITE_NAME-sync.service"
SYNC_PATH="$SITE_NAME-sync.path"
SYNC_TIMER="$SITE_NAME-sync.timer"
MOUNT_SERVICE="$SITE_NAME-mount.service"

install_sync_units() {
  need rsync; need ssh; need systemctl
  say "installing the sync units in $UNIT_DIR"
  if [ "$DRY_RUN" = 1 ]; then
    note "would write $SYNC_SERVICE, $SYNC_PATH, $SYNC_TIMER"
    note "rsync $LOCAL_DIR/ -> $SSH_TARGET:$REMOTE_DIR/ on change and every ${SYNC_INTERVAL}s"
    return 0
  fi
  mkdir -p "$UNIT_DIR" "$LOCAL_DIR"
  # ExecStart has to be an absolute path, and rsync's is not the same path on
  # every distro, so resolve it here rather than guessing in the unit.
  local rsync_bin delete_flag=""
  rsync_bin="$(command -v rsync)"
  # --delete is OFF by default and exists for one reason: the engine's
  # expiry sweep (IRC_MEDIA_PUBLIC_MAX_AGE_DAYS) deletes only the LOCAL
  # copies, so without --delete the served ones outlive their links
  # forever. With it, an emptied or pruned local directory empties the
  # served one too — which is exactly the semantics expiry wants, and
  # exactly why it is not the default.
  [ "$DELETE" = 1 ] && delete_flag="--delete"
  cat > "$UNIT_DIR/$SYNC_SERVICE" <<EOF
[Unit]
Description=rsync veles published media to $HOST
Wants=network-online.target

[Service]
Type=oneshot
Nice=10
# The trailing slashes matter: the first means "the contents of", the second
# means "into". Without them the second run nests a directory inside itself.
# No --inplace: a reader must never see a half-written file.
ExecStart=$rsync_bin -a $delete_flag --mkpath --timeout=60 $LOCAL_DIR/ $SSH_TARGET:$REMOTE_DIR/
EOF
  cat > "$UNIT_DIR/$SYNC_PATH" <<EOF
[Unit]
Description=watch the veles published-media directory

[Path]
PathChanged=$LOCAL_DIR
Unit=$SYNC_SERVICE

[Install]
WantedBy=default.target
EOF
  cat > "$UNIT_DIR/$SYNC_TIMER" <<EOF
[Unit]
Description=periodic fallback rsync of veles published media

[Timer]
OnBootSec=2min
OnUnitActiveSec=${SYNC_INTERVAL}s
AccuracySec=5s
Unit=$SYNC_SERVICE

[Install]
WantedBy=timers.target
EOF
  systemctl --user daemon-reload
  # The path unit is what makes a link work within a second of the reply; the
  # timer is the honesty about inotify not being a guarantee (a write that
  # lands while the unit is masked, a directory replaced rather than written
  # into, a missed event on a filesystem that does not report one).
  systemctl --user enable --now "$SYNC_PATH" "$SYNC_TIMER"
  note "enabled $SYNC_PATH and $SYNC_TIMER"
}

install_mount_unit() {
  need sshfs; need systemctl
  say "installing the sshfs mount unit ($MOUNT_SERVICE)"
  local sshfs_bin
  sshfs_bin="$(command -v sshfs)"
  local fusermount_bin
  fusermount_bin="$(command -v fusermount3 || command -v fusermount)"
  if [ "$DRY_RUN" = 1 ]; then
    note "would write $UNIT_DIR/$MOUNT_SERVICE mounting $SSH_TARGET:$REMOTE_DIR at $LOCAL_DIR"
    return 0
  fi
  mkdir -p "$UNIT_DIR" "$LOCAL_DIR"
  cat > "$UNIT_DIR/$MOUNT_SERVICE" <<EOF
[Unit]
Description=mount the veles media directory from $HOST
After=network-online.target
Wants=network-online.target

[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart=$sshfs_bin $SSH_TARGET:$REMOTE_DIR $LOCAL_DIR -o reconnect,ServerAliveInterval=15,ServerAliveCountMax=3,uid=%U
ExecStop=$fusermount_bin -u $LOCAL_DIR

[Install]
WantedBy=default.target
EOF
  systemctl --user daemon-reload
  systemctl --user enable --now "$MOUNT_SERVICE"
  note "mounted; a write into $LOCAL_DIR is now a write on $HOST"
}

remove_units() {
  for u in "$SYNC_PATH" "$SYNC_TIMER" "$SYNC_SERVICE" "$MOUNT_SERVICE"; do
    if [ -f "$UNIT_DIR/$u" ]; then
      say "removing $u"
      if [ "$DRY_RUN" != 1 ]; then
        systemctl --user disable --now "$u" 2>/dev/null || true
        rm -f "$UNIT_DIR/$u"
        systemctl --user daemon-reload
      fi
    fi
  done
}

# ── the two keys ──────────────────────────────────────────────────────────
# Written to the .env rather than into config.toml: the engine's own `config
# fill` treats that file as append-only and the IRC_* vars beat the TOML block,
# so appending here is the same shape the engine would have written, and no
# comment or credential in a hand-maintained config.toml is at risk. Existing
# lines for the same key are replaced in place, and the old file is kept.
write_keys() {
  say "writing the media keys to $ENV_FILE"
  local lines=("IRC_MEDIA_PUBLIC_DIR=$LOCAL_DIR" "IRC_MEDIA_PUBLIC_URL=$BASE_URL")
  if [ -n "$MAX_AGE_DAYS" ]; then
    lines+=("IRC_MEDIA_PUBLIC_MAX_AGE_DAYS=$MAX_AGE_DAYS")
  fi
  for l in "${lines[@]}"; do note "$l"; done
  if [ "$DRY_RUN" = 1 ]; then return 0; fi
  mkdir -p "$(dirname "$ENV_FILE")"
  [ -f "$ENV_FILE" ] && cp -p "$ENV_FILE" "$ENV_FILE.bak"
  local tmp
  tmp="$(mktemp)"
  [ -f "$ENV_FILE" ] && grep -vE '^IRC_MEDIA_PUBLIC_(DIR|URL|MAX_AGE_DAYS)=' "$ENV_FILE" > "$tmp" || true
  {
    cat "$tmp"
    [ -s "$tmp" ] && [ -n "$(tail -c1 "$tmp")" ] && printf '\n'
    printf '# Where replies put files a chat surface can link to (scripts/media-publish.sh)\n'
    printf '%s\n' "${lines[@]}"
  } > "$ENV_FILE"
  rm -f "$tmp"
  chmod 600 "$ENV_FILE"
  if [ -n "$MAX_AGE_DAYS" ]; then
    note "links expire after ${MAX_AGE_DAYS}d: the gateway sweeps the local directory hourly"
    [ "$DELETE" = 1 ] || note "but the SERVED copies will outlive them — re-run with --delete to make the sync follow"
  fi
  note "restart the gateway to pick them up; it logs where it publishes on connect"
}

remove_keys() {
  [ -f "$ENV_FILE" ] || return 0
  say "removing the media keys from $ENV_FILE (backup kept as .bak)"
  if [ "$DRY_RUN" = 1 ]; then return 0; fi
  cp -p "$ENV_FILE" "$ENV_FILE.bak"
  local tmp; tmp="$(mktemp)"
  grep -vE '^IRC_MEDIA_PUBLIC_(DIR|URL|MAX_AGE_DAYS)=|^# Where replies put files' "$ENV_FILE" > "$tmp" || true
  mv "$tmp" "$ENV_FILE"; chmod 600 "$ENV_FILE"
}

remove_vhost() {
  say "removing the vhost and the served directory on the serving host"
  if [ "$ASSUME_YES" != 1 ] && [ "$DRY_RUN" != 1 ]; then
    printf '  this deletes %s and everything published into it. continue? [y/N] ' "$REMOTE_DIR"
    read -r ans
    [ "$ans" = y ] || [ "$ans" = Y ] || { note "left in place"; return 0; }
  fi
  run_served "
    set -e
    $SUDO rm -f /etc/nginx/sites-enabled/$SITE_NAME /etc/nginx/sites-available/$SITE_NAME
    $SUDO nginx -t && $SUDO systemctl reload nginx
    $SUDO rm -rf $REMOTE_DIR
  "
}

# ── verification ──────────────────────────────────────────────────────────
# The only check that matters: a file written where the gateway writes it,
# fetched from the URL the gateway will hand out. Anything short of this
# verifies the config and not the chain.
verify() {
  local probe="veles-media-probe-$$.txt"
  say "publishing a probe file and fetching it back over $BASE_URL"
  mkdir -p "$LOCAL_DIR"
  printf 'veles media-publish probe %s\n' "$(date -u +%FT%TZ)" > "$LOCAL_DIR/$probe"
  local url="${BASE_URL%/}/$probe"
  local ok=0
  for _attempt in $(seq 1 30); do
    if [ "$MODE" = remote ]; then
      systemctl --user start "$SYNC_SERVICE" 2>/dev/null || true
    fi
    if curl -fsS -m 10 "$url" >/dev/null 2>&1; then ok=1; break; fi
    sleep 2
  done
  if [ "$ok" = 1 ]; then
    note "OK: $url fetched"
  else
    note "FAILED to fetch $url after 60s."
    note "  · does $HOST resolve to the serving host? (--scheme http if there is no cert)"
    note "  · is the vhost enabled and nginx reloaded? ($SUDO nginx -T | grep -A3 $SITE_NAME)"
    note "  · can the web server READ the file? a 403 is a umask, not a routing problem"
    note "  · remote mode: did the sync run? journalctl --user -u $SYNC_SERVICE"
  fi
  rm -f "$LOCAL_DIR/$probe"
  if [ "$MODE" = remote ]; then
    systemctl --user start "$SYNC_SERVICE" 2>/dev/null || true
  fi
  run_served "rm -f $REMOTE_DIR/$probe" || true
  [ "$ok" = 1 ] || die "the chain does not work yet; the keys were still written, so unset them or fix the serving half"
}

# ── modes ─────────────────────────────────────────────────────────────────
case "$MODE" in
  configure)
    write_keys
    ;;
  check)
    verify
    ;;
  remove)
    remove_units
    remove_keys
    if [ -n "$SSH_TARGET" ]; then
      [ -n "$REMOTE_DIR" ] || REMOTE_DIR="/var/www/${HOST:-$SITE_NAME}"
      remove_vhost
    elif [ -n "$HOST" ]; then
      SERVE_HERE=1
      REMOTE_DIR="${REMOTE_DIR:-$LOCAL_DIR}"
      remove_vhost
    else
      note "no --ssh and no --host, so only the local units and keys were touched"
    fi
    ;;
  local)
    SERVE_HERE=1
    command -v nginx >/dev/null 2>&1 ||
      note "nginx is not in PATH here — on a distro that manages it as a service that is fine, but the reload below will fail if it is really absent"
    dns_check
    say "serving $LOCAL_DIR on this machine as $HOST"
    serve_and_certify
    write_keys
    verify
    ;;
  remote|mount)
    need ssh
    dns_check
    say "serving $REMOTE_DIR on $SSH_TARGET as $HOST"
    serve_and_certify
    if [ "$MODE" = mount ]; then install_mount_unit; else install_sync_units; fi
    write_keys
    verify
    ;;
  *)
    usage; die "unknown mode: $MODE"
    ;;
esac

say "done"
note "the engine copies a reply's media into $LOCAL_DIR and links ${BASE_URL%/}/<name>."
if [ -n "$MAX_AGE_DAYS" ]; then
  if [ "$DELETE" = 1 ]; then
    note "links expire after $MAX_AGE_DAYS days: the gateway sweeps the local dir and the sync carries the deletion to the served copies."
  else
    note "links expire after $MAX_AGE_DAYS days LOCALLY — but the SERVED copies outlive the sweep without --delete."
  fi
else
  note "nothing expires those copies: they accumulate until something removes them (see --max-age)."
fi
