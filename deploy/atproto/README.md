# atproto island: veles.7778777.online — AS DEPLOYED 2026-10-05

This is a record of the live deployment, not a forward recipe. The bring-up
was driven interactively; this captures what exists and why, so it can be
rebuilt or audited. The code is `crates/veles-atproto/` (commits `942781a`,
`fbb971b`, and the door round).

## What is live

- **PDS host:** `veles.7778777.online`, service DID `did:web:veles.7778777.online`.
- **Handle domain:** `veles.7778777.online`; wildcard handles `*.veles.7778777.online`.
- **Bot account (the model drives it):** `jackma.veles.7778777.online` =
  `did:plc:xrhhnoqaa7nth7coplnfy4b7`, registered in the PLC directory,
  relay-announced to `bsky.network` (`getHostStatus`: active, accountCount 1).
- **Test account (the agent posts from it for live tests, 2026-10-11):**
  `tester.veles.7778777.online` = `did:plc:5gpaqik336k7xsb6duz2x6k2`,
  self-labelled `bot`. In `[acl]` it is `role = "user"`,
  `ticket_role = "owner"` — anything above user waits on a ticket the
  operator signs (docs/acl.md) — and `[platforms.atproto] co_operators`
  admits it at the door. Its password stays on the laptop
  (`~/.local/state/veles-vps-stage/bsky-tester-create.json`).
- **Object store:** Cloudflare R2 bucket `veles-atproto`, **EU jurisdiction**
  (endpoint `https://88fb6a02508370f9bc1351ef4b5641c9.eu.r2.cloudflarestorage.com`).
  The bucket is the ONLY durable state.
- **Invite-gated:** `inviteCodeRequired: true`. `blobUploadLimit` 100 MB.

## The door (USER 2026-10-05: "mutuals get User, everyone else NO access")

Two layers, fail-closed end to end:

1. **Adapter-side** (`[platforms.atproto] strangers_policy = "ignore"`, the
   default): the identity resolver answers who a DID is to the operator.
   Only the **operator**, their **mutuals** (both directions) and the
   **co-operators** (`co_operators`, accounts the operator runs) produce a
   turn. A co-operator's follows make no one a mutual: only the operator's
   graph is asked. A one-way follower, someone the operator follows, and strangers are
   dropped BEFORE a turn/reply/token — one INFO line, same posture as
   `[acl.strangers] = "ignore"`. A resolver error fails closed to Stranger.
2. **Gateway backstop** (`[acl.strangers] atproto = "ignore"`): even if the
   adapter gate were bypassed, a sender with no ACL rule is dropped at the
   gateway door.

**Roles.** Operator (`atproto:did:plc:jg6o3…`) is pinned `owner` in
`[[acl.users]]`. A **mutual** gets `user` as a REAL runtime ACL rule: the
adapter writes a TTL'd grant (`pairing::grant_scoped`, 24h) for
`atproto:<did>`, re-asserted on every admitted turn — an active mutual never
lapses; an unfollow decays within a day. Trusted comes later by explicit
`/acl grant atproto:did:plc:…`.

To open the world to a sandboxed guest turn later (the deferred security
test): set `strangers_policy = "guest"` and remove the `[acl.strangers]`
backstop. Guests are forced into a Tor-egress user-env sandbox at guest
ceilings.

## DNS (grey-cloud, both → 203.0.113.10)

- `A veles.7778777.online`
- `A *.veles.7778777.online`

Cloudflare proxy is OFF (it caps uploads at 100 MB and kills the firehose
websocket).

## TLS

Wildcard cert `veles.7778777.online` + `*.veles.7778777.online` at
`/etc/letsencrypt/live/veles.7778777.online/`, expires 2027-01-03. Issued by
certbot **DNS-01** (`python3-certbot-dns-cloudflare`) with credentials file
`/etc/letsencrypt/dns/cloudflare-7778777.ini` (mode 600 root). Since
2026-10-06 it holds a DNS-edit-only token for this zone (the laptop's
`~/cloudflare_dns`); before, it held the full account token from
`~/cloudflare_`, which USER rotated. `certbot renew --dry-run --cert-name
veles.7778777.online` passed with it on 2026-10-06. Auto-renew reuses the
file. The other vhosts use HTTP-01 (`snippets/letsencrypt.conf`); the
wildcard needed DNS-01.

## nginx front (`/etc/nginx/sites-available/veles`, source `deploy/atproto/nginx-veles.conf`)

Terminates TLS, proxies to `127.0.0.1:2583`. **Blocks** the operator surfaces
(`/admin`, `/xrpc/vlpds.admin.*`, `/metrics`, `/internal/*` → 404). The
firehose path (`/xrpc/com.atproto.sync.subscribeRepos`) gets websocket upgrade
headers, no buffering, long timeouts. `VLPDS_TRUSTED_PROXIES=127.0.0.1` so
rate limits see the real client IP from `X-Forwarded-For`.

## vlpds container (quadlet `deploy/atproto/vlpds.container` →
`/srv/pool/veles/.config/containers/systemd/vlpds.container`, user `veles`)

- Image **pinned** to `ghcr.io/jazware/vlpds:sha-2aaf27c` (= main @
  `sha256:6905f2d0…`). vlpds has NO semver tags, only `main` + `sha-*`.
- Loopback ports only: `127.0.0.1:2583` (app), `127.0.0.1:9583` (metrics).
- Secrets `/srv/pool/veles/vlpds/secrets/`, mode 0400, mounted read-only at
  `/run/vlpds`. **Ownership gotcha (rootless podman):** the container runs as
  uid 10001 (vlpds); mounted secrets must be owned by HOST uid **306608**
  (= subuid-base 296608 + 10001 − 1; host-uid-0→container-0 shifts the map by
  one) so they read as uid 10001 inside. veles subuid = `296608:65536`.
- `StopTimeout=90` (vlpds SIGTERMs gracefully; never `podman kill`; never two
  with the same `VLPDS_NODE_ID=single`).
- `VLPDS_APPVIEW=https://api.bsky.app,did:web:api.bsky.app` (2026-10-06):
  without it every `app.bsky.*` call answered 400 "No service configured"
  — the notification backstop had never worked. The adapter also sends
  `atproto-proxy: did:web:api.bsky.app#bsky_appview` on those calls.

### Secrets (back the KEK + PLC key up OFFLINE — they are NOT in the bucket)

`jwt-secret`, `admin-token`, `internal-token`, `kek`, `s3-access-key`,
`s3-secret-key` (64-hex or the R2 creds, no trailing newline), and
`plc-rotation.key` (the `vw1.`-wrapped PLC rotation key). The PLC rotation
key's public half: `did:key:zQ3shR3sEVwF8EjBtgnQtBdLCvmXmYYMUpmqa2KA3CC1wabe4`,
wrapped under KEK fingerprint `Lb42b2ad1f4718d3d`.

Backed up 2026-10-06 to the laptop: `~/backups/vlpds/vlpds-2026-10-06.tar.gz`
(0600, in a 0700 dir; the whole `vlpds/` dir plus both quadlets — its tar
stream's sha256 matched the VPS's). Keep a copy off the laptop too.

### R2 credentials

Data-plane (`access_key_id` + `secret_access_key`) live in the laptop's
`~/cloudflare_r2`, IP-restricted to the VPS — usable only from there. The
bucket was created with the zone token (`~/cloudflare_`), which also has R2
edit on the account; the EU jurisdiction needs the `cf-r2-jurisdiction: eu`
HEADER on the create call (a body field is silently ignored and yields
`default`).

## The gateway's atproto adapter

- `.env` (`/srv/pool/veles/data/.env`): `ATPROTO_BOT_HANDLE`,
  `ATPROTO_APP_PASSWORD` (the bot's account password from `account create`),
  `ATPROTO_OPERATOR_DID`.
- `config.toml`: `[platforms.atproto]` (pds_url `http://127.0.0.1:2583`,
  operator_did, jetstream_enabled, notif_poll_secs 30, strangers_policy
  ignore) + the owner identity + the `[acl.strangers]` backstop.
- Wired into `veles-cli` behind the singleton fence (`singleton:atproto`
  arbiter lease), own OS thread + runtime, bus only.
- Ingestion: Jetstream v2 hint lane (broad app-notif set) + the authoritative
  notification poll on our own PDS (`listNotifications`+`updateSeen`).
  Exactly-once is the gateway's `dedup_key`.
- Since 2026-10-10 the VPS's lane reads the local jetstream-relay
  (`~/jetstream-relay_flake`), which also serves courier: one upstream
  stream for both. It is a per-node value, so it lives in the VPS's
  `[node.overrides]`, not in the shared `[platforms.atproto]`:
  `"platforms.atproto.jetstream_url" = "ws://jetstream-relay:8644/xrpc/network.bsky.jetstream.subscribeEvents"`.
  The plugin reads its config at gateway start, so a change needs a
  gateway restart (a `plugins reload` keeps the old value). The bot's
  old fan-out (`fanout_bind`, `ATPROTO_FANOUT_*`) is gone.
- `appview.query` (a repo service): one read-only `app.bsky.*.get*` query
  as the bot, through our PDS. The trigger engine's `bluesky` action uses
  it to preview bsky.app and impro.social links in IRC, so posts limited
  to signed-in users read too; without it slopdrop's linkresolver answers.
- `repo.get_record` (a repo service): one public record by AT URI, raw,
  from the AppView's record store with no viewer, so no block applies;
  `found: false` when the record is gone. The previews use it for what a
  thread view hides behind a block (the post, its parent, a quote) and to
  tell a blocked post that was deleted from one that still exists.
- Jetstream v2 is `wss://jetstream.us-east.bsky.network/xrpc/network.bsky.jetstream.subscribeEvents`
  (`collections=`, `seq` cursor, no key for the live tail, about 36 h of
  cursor replay). Until 2026-10-06 the lane spoke legacy v1
  (`jetstream2.us-east…/subscribe`: the "2" is a host number). The cursor
  lives in `data/atproto-jetstream-v2.cursor`; the v1 file
  (`atproto-jetstream.cursor`, a microsecond timestamp) is left for a
  rollback, and seeded the v2 one on its first start.

## /tcl on Bluesky: the local output check, approve and delete

A public `/tcl` reply is decided by a LOCAL check of its exact output, run
on the VPS CPU — no third-party endpoint. The DeepSeek proc rating is gone.

- The scorer is the `veles-classifier` sidecar (quadlet beside the
  gateway): Detoxify `unitary/unbiased-toxic-roberta` int8-ONNX on one
  vCPU, `http://veles-classifier:8800`, NO published port (gateway-only).
  Model mounted RO from `/srv/pool/veles/classifier/model`; source +
  Containerfile + install-model.sh in `deploy/vps/classifier/`.
- The decision (`slopdrop_check::decide_public`), first rule wins:
  1. **Blocked.** The output or the proc is blocked: withheld, with no
     hosted copy and no question.
  2. **Allowlisted.** The owner approved this exact text: inline.
  3. **Bad list.** The line names a bad-list proc, or reaches one through
     stored bodies (`ProcIndex::bad_reach`): withheld + link. A wrapped
     call (`return [badproc]`) counts. The output check cannot stand in for
     the bad list: 28 of 71 bad-list proc outputs pass it. No proc index
     means fail closed.
  4. **The output check passes:** inline. It runs on the exact text the
     post carries (mIRC/ANSI codes stripped, the head of a long result):
     deterministic gates (art/shape, links/people, the folded denylist),
     then the scorer.
     - Thresholds `[slopdrop] output_classifier_tox`/`_id`: deployed 0.30
       / 0.25. 0.05 came from the 10-06 calibration and was relaxed on
       10-07.
     - A scorer answer counts only as a 2xx with all 16 heads and
       `truncated:false`.
     - No endpoint answering means withheld "classifier unavailable",
       failing CLOSED in about 0.3 s.
  5. **Anything else:** withheld + link.
  - The gateway logs one line per reply: the outcome, the reason, the
    top head and the ms. Never the text.
- **The ledger: every public reply, and delete.**
  - Every public `/tcl` reply is recorded in `slopdrop/withheld.json`
    for 7 days, as `withheld`, `inline` or `blocked note` (since 10-07;
    before, inline replies were not recorded and the bot could not take
    them down). The same output of the same kind is one entry, and it
    keeps every post, record and link.
  - Only a withheld reply is asked about on Telegram: approve, delete,
    keep, or keep and stop asking.
  - The owner acts on the node running this adapter (Telegram reaches it):
    - `/slopdrop withheld`: the held-back replies.
    - `/slopdrop posts`: every reply, each with a link to its post.
    - `/slopdrop approve|delete <post>`, where `<post>` is the entry id, the
      post's link in any client, its at-uri or rkey, the `/t/` short link
      or its id, the viewer link, or the request the reply answered.
    - `/slopdrop check <text>`: what the check says of a text.
  - Approve reposts inline, allowlists the text and lifts a block. The
    entry turns `inline` and names the repost, so it can still be deleted.
  - Delete takes every copy down and blocks the output (and the proc, for
    a single call). A second approve of the same reply is refused.
- **Quotes and markers (2026-10-12).** Anyone but the configured owner
  quotes only inside the thread or their own posts, once per post and
  three times per author a day (`<state>/quotes.json`). The adapter reads
  a quote's target as the bot and drops it, with a note, when quoting is
  off, a block stands either way, a moderation service's label is on the
  post or its author, or the author gave it an adult self-label (`porn`,
  `sexual`, `nudity`, `graphic-media`, `gore`). The author's other
  self-labels, such as `bot`, are no reason. A shaped `/tcl`
  reply never carries an attachment marker; the details are in
  `docs/slopdrop-bridge.md`, "Quotes" and "Attachment markers".
- **Text art as pictures (2026-10-12).** The slopdrop module draws a
  reply's IRC art as a PNG and sends it in the reply spec (`kind = "art"`,
  base64, with its size and alt text), with the self-labels it decided
  (`kind = "label"`). The adapter takes only PNGs under 1,000,000 bytes,
  at most four, and only the four self-label values. It uploads each
  as `image/png` as it was drawn (never re-encoded: a JPEG smears every
  glyph), sets `aspectRatio` on it and on every other image it posts,
  and puts the labels on every post of the reply as `selfLabels`. The
  details are in `docs/slopdrop-bridge.md`, "Text art as pictures".
- **Handles.** A Jetstream mention carries only the DID. The adapter looks
  the handle up (AppView `getProfile`, cached 15 min, 3 s cap, alongside
  the thread fetch), so `[nick]`, the requester and the session show it.
  Display only: the ACL keys on the DID.
- **Hosting.**
  - Each result is two PDS blobs plus one owner-pointer record
    (`online.7778777.tcl.output`) that references them, so vlpds's
    unreferenced-blob sweep (6 h grace) keeps them.
  - Delete removes the post(s), every `/t/<id>` short-link file and every
    pointer record. The sync unit's rsync runs `--delete`, so the docroot
    copy goes too (proven live 10-07).
  - The blobs follow on the next sweep, within about 7 h. There is no
    faster purge without the vlpds admin token, which the gateway does not
    hold.
- **Verdicts.** `safety.tsv` maps hash → safe, borderline, unsafe,
  approved or blocked. The owner's allowlist and block list sit beside it.

`tcl-short-sync.path` + `.service` (user `veles`, `rsync -rt --delete
--chmod=D755,F644`: no symlinks, no owners) copy the short links into
`/srv/hermes-published/tcl/s`.

## Verify (the live checklist)

1. `curl https://veles.7778777.online/xrpc/com.atproto.server.describeServer`
   → did, `.veles.7778777.online` handle domain, inviteCodeRequired true.
2. Operator paths blocked: `/admin`, `/xrpc/vlpds.admin.*`, `/metrics` → 404.
3. `curl https://jackma.veles.7778777.online/.well-known/atproto-did` → the bot DID.
4. `curl https://bsky.network/xrpc/com.atproto.sync.getRepoStatus?did=did:plc:xrhhnoqaa7nth7coplnfy4b7`
   → active. `getHostStatus?hostname=veles.7778777.online` → active, accountCount 1.
5. After the gateway image with the adapter is live: a mention from the
   operator's account → an owner-turn reply; a mention from a non-mutual →
   silence (one INFO line in the gateway log). Restart → no double answer.
