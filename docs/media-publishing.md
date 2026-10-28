# Publishing media: the contract, and the half the engine will not do

A reply can carry a file. The convention is a line of its own in the model's
output:

```
MEDIA:/absolute/path/to/file.png
```

What happens next depends on the surface, and on who is reading:

| surface   | delivery                                                            |
|-----------|---------------------------------------------------------------------|
| desktop   | rendered inline in the transcript (it is a local file the owner owns) |
| TUI       | a path note — the terminal is not a place to draw a PNG               |
| telegram  | uploaded to the chat, by kind (photo/audio/voice/video/document)      |
| IRC       | **copied into a served directory and linked**, or a note if there is none |

IRC is the one that needs infrastructure, because IRC has no upload. This
document is about that half: what the engine guarantees, what it refuses to
decide for you, and the script that builds the rest.

## What the engine does

**It decides who may have which file** (`media::deliverable`). A path is
deliverable when it is inside the engine's media directory — `<data_dir>/media`,
where `generate_image`, `text_to_speech` and the connectors' own downloads
write — or inside the sender's own sandbox workspace. Owner surfaces (TUI,
REPL, desktop, CLI) and `Cap::All` principals are unrestricted. Everyone else,
**admins included**, gets a note in place of the file:

```
📎 media withheld (outside the media directory)
```

No path in the note. The path and the principal go to the journal as a
`[acl] withheld …` warning, because "a model tried to leave the media root" is
worth knowing and a public channel is not where to say it. The same rule runs
inbound: a below-owner sender's `MEDIA:` line is defanged before anything
resolves it, so the file is never opened and the marker cannot be used to probe
which host paths exist.

**It writes files safely** — for IRC, into `media_public_dir`:

- the name is a nanosecond stamp plus the original file name reduced to
  `[A-Za-z0-9.-]` (`_` becomes `-`, since `_` is *italic* to the markdown
  converter on the way to the wire and a link with eaten characters is a link
  that 404s), capped at 80 characters, retried against a collision;
- the copy is `chmod 0644` explicitly, because a service's umask is not the
  operator's umask and a file the web server cannot read is a 403 with no
  explanation anywhere the operator would look;
- files over 64 MiB are refused before they are copied;
- nothing is deleted unless `media_public_max_age_days` is set (see
  **Expiry** below) — and then only the engine's own flat files.

**It never serves anything.** No listener, no route, no certificate. The two
keys are:

```toml
[platforms.irc]
media_public_dir = "/path/to/a/directory/something/serves"
media_public_url = "https://media.example.org"
```

with `IRC_MEDIA_PUBLIC_DIR` / `IRC_MEDIA_PUBLIC_URL` as the `.env` twins (the
env wins, as everywhere else in this project). Both unset — the default — means
IRC delivers no media at all and says so:

```
📎 media not delivered on IRC (no public directory configured)
```

which is strictly better than the old behaviour, which was printing
`MEDIA:/home/you/.local/share/veles/media/generated/1234-chart.png` into a
public channel. A half-configuration (one key of the two) also delivers
nothing, and warns on connect rather than quietly filling a directory nobody
serves. When both are set, the connect log says where files go:

```
irc/bot: media publishes into /srv/www/veles-media and links from https://media.example.org
```

That line is the answer to "did this ever get configured".

## scripts/media-publish.sh

The serving half, parameterised. It knows nothing about any particular machine,
VPS, DNS provider or CA — every default is a flag or an env var, and
`--dry-run` prints the whole plan without touching anything.

```sh
# serve from a directory on an ssh host, syncing there on change
scripts/media-publish.sh remote \
  --host media.example.org \
  --ssh deploy@files.example.org \
  --remote-dir /var/www/veles-media

# serve from this machine's own nginx
scripts/media-publish.sh local --host media.example.org

# mount the remote directory instead of syncing (writes are uploads)
scripts/media-publish.sh mount --host media.example.org --ssh deploy@files.example.org

# the directory is already served; just write the two keys
scripts/media-publish.sh configure --host media.example.org

# prove the chain works: publish a probe file, fetch it over the public URL
scripts/media-publish.sh check --host media.example.org
```

What each mode does:

- **`remote`** — creates the served directory on the ssh host, writes an nginx
  vhost, obtains a certificate, then installs two systemd **user** units on the
  gateway's machine: a `.path` unit that rsyncs the published directory on
  change, and a `.timer` that does it anyway every `--interval` seconds. The
  timer is not belt-and-braces theatre: an inotify unit that missed a write is
  a link that 404s, and "missed" happens (a directory replaced rather than
  written into, a filesystem that does not report the event, a masked unit).
- **`mount`** — same vhost, but an sshfs mount instead of a sync, so a write
  *is* an upload and there is no window where the link exists and the file does
  not. Needs `sshfs` and FUSE locally, and it is the more fragile of the two:
  a dropped ssh session is a directory that stops being writable until the
  mount is re-established.
- **`local`** — one machine does both halves.
- **`configure`** / **`check`** / **`remove`** — the keys, the end-to-end
  probe, and the undo (which asks before deleting anything on a remote host).

The vhost it writes is the two-block shape static sites should have: port 80
serves the ACME challenge and redirects, port 443 serves the directory with
`autoindex off` stated out loud, `X-Content-Type-Options: nosniff`, a
dotfile-denying location, and `Cache-Control: immutable` (safe because the
names are unique per publish). The certificate is obtained against a
**port-80-only** vhost first and the full one is written over it afterwards,
because nginx refuses to reload while an enabled vhost names a certificate that
does not exist — and a failed reload is not a failed site, it is every site on
the box.

`--tls` picks the validator: `certbot` (HTTP-01 against `--webroot`, the
default), `certbot-dns01` (below), `existing` (the certificate is already
there), or `none` (plain HTTP, one block).

### When HTTP-01 will not complete: `--tls certbot-dns01`

A hostname created minutes ago behind a CDN can fail ACME like this:

```
Detail: During secondary validation: DNS problem: networking error looking up A
```

while the name resolves perfectly from every public resolver you can reach, and
the challenge file is fetchable from the internet. The *primary* validation
passed; the CA's other vantage points could not resolve a name that is new to
them. This is a wait, not a misconfiguration — and the expensive part is that
each attempt is a **failed authorization**, and five of those for one identifier
inside an hour locks the identifier out for the rest of the hour. So: do not
retry in a loop, and if you have already burned the budget, switch validators
rather than waiting it out.

```sh
scripts/media-publish.sh remote --host media.example.org --ssh deploy@files.example.org \
  --tls certbot-dns01 \
  --dns-publish-cmd 'my-dns-provider-cli set-txt "$ACME_RECORD" "$ACME_TOKEN"'
```

The DNS credentials stay on the machine running the script: certbot runs on the
serving host with an auth hook that only *waits* for the TXT record to be
visible from there, while `--dns-publish-cmd` publishes it from here (it is run
with `ACME_DOMAIN`, `ACME_RECORD`, `ACME_TOKEN` and `ACME_ACTION=publish` then
`cleanup` in its environment). Without `--dns-publish-cmd` the token is printed
and you publish it by hand.

Two things this path does deliberately, both learned the hard way:

- the hook lives in a root-owned directory under `/usr/local/lib`, **never
  `/tmp`**. certbot runs hooks as root; a root-executed script in a
  world-writable directory is a privilege escalation with a timer on it, and
  the timer is the next renewal.
- afterwards it re-issues once over HTTP-01 with `--force-renewal`, which
  rewrites the renewal configuration to the webroot. A `--manual` certificate
  renews by running the hooks again — so unless you flip it, renewal depends
  forever on whatever published the TXT the first time, and fails quietly 60
  days later. If HTTP-01 still does not work, the script says so instead of
  pretending.

**DNS is deliberately not its job.** Providers differ, credentials differ, and
a script that can create DNS records is a script that can take over a domain.
It checks that the hostname resolves, and if it does not, it prints what to
create and stops.

The two keys land in the `.env` beside `config.toml` (append-only, existing
lines for the same key replaced in place, old file kept as `.bak`, mode 600)
rather than being edited into a hand-maintained TOML file, because the engine's
own `veles config fill` treats that file the same way and the `IRC_*` vars beat
the TOML block regardless.

## Expiry

Nothing deletes a published file unless something is told to. A channel that
asks for a lot of charts fills a web root forever, at URLs that are unguessable
but not forgotten, world readable by design. The engine now has the knob —
**`IRC_MEDIA_PUBLIC_MAX_AGE_DAYS`** (`platforms.irc.media_public_max_age_days`):
the gateway sweeps the published directory on connect and hourly, deleting flat
files older than the age (`0`, the default, means never). It prunes the LOCAL
directory only, so the sync has to follow or the served copies outlive their
links:

```sh
scripts/media-publish.sh remote --host media.example.org --ssh deploy@files.example.org \
  --tls existing --max-age 7 --delete
```

`--max-age 7` writes the key; `--delete` rewrites the installed sync unit with
`rsync --delete`. Without `--delete` the script says so instead of pretending:
expiry that only reaches one side of the sync is a promise the URLs break. The
sweep is deliberately flat and does not follow symlinks — see the unit tests in
`veles-irc` for exactly what it will and will not touch.

If you would rather not have the engine delete anything, the cron form still
works:

```sh
# nightly, on the serving host: drop anything older than a week
0 4 * * * find /var/www/veles-media -type f -mtime +7 -delete
```

## Troubleshooting

- **`404`** — the vhost is not enabled, or `root` points somewhere else.
  `sudo nginx -T | grep -A5 <site-name>` on the serving host.
- **`403`** — permissions, not routing. The web server has to be able to read
  the file *and* traverse every directory above it. The engine chmods its own
  copies to 0644; a directory created by hand at 0700 will still 403.
- **the link 404s for a few seconds after the reply** — that is the sync
  window in `remote` mode. `mount` mode does not have one; `--interval` bounds
  it either way.
- **`journalctl --user -u <site>-sync.service`** — the rsync's own errors. A
  host-key prompt in a non-interactive unit is the usual one: the sync runs as
  a user service, so the ssh key must work without a passphrase or with an
  agent the user manager can see.
- **ACME: `During secondary validation: DNS problem: networking error looking
  up A`** — see [`--tls certbot-dns01`](#when-http-01-will-not-complete---tls-certbot-dns01).
  The short version: wait, do not retry in a loop, and switch validators rather
  than spending the hour's authorization budget.
- **a machine fetching the link is blocked but a browser is not** — a WAF in
  front of the origin (Cloudflare's Browser Integrity Check plus a default
  security level will silently refuse server-to-server requests to a new
  subdomain). IRC clients are usually the browser case; link-preview bots are
  not.

## Why this is a script and not a plugin

A veles plugin runs inside the gateway, and this work happens outside it: a
directory, a web server, a certificate, a DNS record, a sync unit — all of
which have to exist before the gateway can be told where to put files, and all
of which outlive a restart of it. The engine's half is already first-class
config; the rest is deployment, and deployment belongs in a script you can read
end to end and run with `--dry-run`.

## Appendix: one real installation

The first run of this, on the machine veles is developed on, as an example of
the whole shape — an IRC bot on a desktop, files served by a VPS:

```sh
# 1. DNS (by hand, at the provider): A  veles-media.example.org → the VPS, proxied
# 2. everything else
scripts/media-publish.sh remote \
  --host veles-media.example.org \
  --ssh deploy@files.example.org \
  --remote-dir /var/www/veles-media
```

which created `/var/www/veles-media` owned by the ssh user at 755, wrote
`/etc/nginx/sites-available/veles-media` and symlinked it, obtained a
certificate (HTTP-01 failed on secondary validation for the first hour because
the record was minutes old, so it was re-run with `--tls certbot-dns01` and the
TXT published from the laptop, then flipped back to webroot for renewal),
installed three systemd user units, appended the two keys to
`~/.config/veles/.env`, and finished by publishing a probe file and fetching it
back over HTTPS:

```
▸ publishing a probe file and fetching it back over https://veles-media.example.org
  OK: https://veles-media.example.org/veles-media-probe-3503444.txt fetched
```

The units:

```
~/.config/systemd/user/veles-media-sync.service   # rsync -a --mkpath <local>/ <ssh>:<remote>/
~/.config/systemd/user/veles-media-sync.path      # PathChanged=<local> → the service
~/.config/systemd/user/veles-media-sync.timer     # and every 60s anyway
```

A file dropped into the published directory was fetchable over HTTPS eight
seconds later with nothing run by hand, which is the number that matters: the
model posts the link in the same message as the file, so a human clicking it a
few seconds later has to find it there.

The gateway says what it will do, on connect. On THIS box the running
line, with the multi-network config read from `[platforms.irc]` in
`~/.config/veles/config.toml`, is:

```
irc/bot: media publishes into /home/user/.local/share/veles/media/public and links from https://veles-media.7778777.online
```

That configuration is live: the media-sync units
(`~/.config/systemd/user/veles-media-sync.{service,path,timer}`, from
`scripts/media-publish.sh remote`) are ACTIVE user units on this
machine, and expiry is armed (`media_public_max_age_days = "7"`). The
general posture above still holds - both keys unset (the default)
means IRC delivers no media at all and says so; this box is the
configured case.

Still owed by the operator: if the serving
host is behind a CDN with a browser-integrity check, an exception for
server-to-server fetches of those URLs.
