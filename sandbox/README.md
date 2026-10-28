# The sandbox images

Two postures, one `[sandbox]` section, and a hard rule about where host
work happens.

## Dev posture — `sandbox/Dockerfile`

Ubuntu LTS with a full developer environment (compilers, python3 + pip +
venv, node + npm, git, ripgrep, jq, sqlite, tclsh, ImageMagick 6). Built for the model
to WORK in: it can install packages (root inside the container, no sudo
setuid dance — the backend's `no-new-privileges` would break one anyway)
and gets network when the backend allows it.

Host safety is the JAIL's job, not the image's: rootless podman maps the
container's root to the unprivileged host user, drops what the config
says to drop, and keeps `no-new-privileges` on. The image is therefore
deliberately CAPABLE — starving it would only teach models workarounds.
The sharp edge is the fence around it.

```sh
scripts/build-sandbox-image.sh        # needs podman (any Linux host with podman)
```

```toml
# ~/.config/veles/config.toml — the dev posture
[sandbox]
backend = "podman"
image = "veles-sandbox:latest"
capabilities = "default"        # apt/pip need the runtime's caps in the userns
# network stays on (the default): the container's own network, never
# --network host (it was, for the per-run fence, until 2026-10-02);
# explicit host access is opt-in:
# [[sandbox.mounts]]
# host = "/home/user/projects"
# container = "/projects"
# mode = "rw"                    # ro | rw — nothing is mounted unless listed
```

## Benchmark / untrusted posture

Same image (or any tclsh image — the pure-compute contract only needs
`tclsh` on PATH), fence tightened:

```toml
[sandbox]
backend = "podman"
network = false                  # --network none; the benchmark switch
capabilities = "drop"            # --cap-drop ALL (the default)
```

## Through another user's podman (`CONTAINER_HOST`)

On the VPS the gateway itself runs in a container, and its sandboxes
run beside it, not inside it: as the `veles-sbx` user, driven through
that user's podman socket (deploy/README.md). With podman's own
`CONTAINER_HOST` set, every podman call is remote and a path on the
gateway's side means nothing to the service, so:

- a user env's `/work` is the named volume `veles-env-<key>` (removed
  with the env), not a host directory;
- files go in and out with `podman cp`; a background shell's output is
  read with an exec a second;
- the per-run fence is `create`, `cp` the scratch dir in, `start
  --attach`.

`veles doctor --sandbox` makes a user env end to end either way. A node
with no container runtime at all (the phone) uses another node's
sandboxes instead: the one `[cluster] sandbox_home` names, or, unset,
the least loaded task peer that advertises a runtime (docs/pool.md,
"Automatic homes"; PLAN-phone-userland.md T2).

## Not a shell, and on purpose

Every `[sandbox]` backend — including `native`/`host` — runs the
SAFE-INTERP Tcl driver. That is the fence for untrusted callers (IRC's
unprivileged `tcl`, cron no_agent jobs, `execute_code{sandbox:true}`).

Real host work lives OUTSIDE the fence, where it always has:

- the **`terminal`** tool — an unfenced host shell (`bash -c`, ACL-tier
  trusted), core-resident in every interactive session;
- **`execute_code`** without the sandbox flag — unfenced host Tcl.

If a task "can only really be done inside the host system", it goes
through those, not through a backend.

## Why not a flake output

`flake.nix`'s dockerTools builds nix-store images — honest for the veles
runtime image (`packages.ociImage`), wrong for this one: the point is an
ubuntu base with a WORKING apt, which dockerTools cannot produce. So this
image ships as a Dockerfile + script, built where a container builder
exists, and the cargo gate pins backend argv daemon-free instead of
building images.

---

## firecracker: the artifact contract (2026-09-04)

Until this date `PENDING.md` carried *"Firecracker kernel/rootfs
artifacts, if/when that fence is wanted for real (the backend + config
contract exist; the artifacts don't)."* They exist now, built by
`nix/sandbox-artifacts.nix`:

```sh
nix build .#firecrackerKernel   # → $out/vmlinux
nix build .#firecrackerRootfs   # → an ext4 image
```

and wired automatically by the NixOS module when
`services.veles.sandbox.backend = "firecracker"`.

### The guest kernel is not the host's

Firecracker loads its own `vmlinux`; the running kernel is untouched.
It is nonetheless built **from the same source the host runs** — the
module defaults the base to `config.boot.kernelPackages.kernel`, so nix
has usually fetched the tarball already and only the config differs.

That difference is the entire reason a custom kernel is needed. A stock
NixOS kernel has

```
CONFIG_VIRTIO_MMIO=m   CONFIG_VIRTIO_BLK=m   CONFIG_EXT4_FS=m
```

as **modules**. A firecracker guest boots with no initrd, so it has
nowhere to load a module from and cannot find its own root device. The
overlay forces those three plus `SERIAL_8250_CONSOLE` (the only channel
out of a network-less microVM) to `=y`.

### The exit protocol

Firecracker's exit status describes the **VMM**, not the guest. Taking
it as the sandbox's result — which is what shipped 2026-09-03 — made
`SandboxOutput::success()` answer true whenever the VM shut down
cleanly, however the script had fared.

So the guest init (`sandbox/firecracker/init.sh`) prints, as its very
last act on the console:

```
VELES_SANDBOX_EXIT:<n>
```

and `sandbox::split_console` splits the console stream there:

- everything before the sentinel is the script's stdout,
- `<n>` is its exit code,
- **no sentinel at all is `-1`, never success** — it means the guest
  never reached the end of init (kernel panic, wrong rootfs, a hang the
  VMM outlived), and the whole console is kept as evidence,
- the **last** sentinel wins, so a script printing the marker itself
  cannot forge its own status.

### Drive postures

| drive | mode | why |
| --- | --- | --- |
| `/dev/vda` root | **read-only** | it is a shared artifact (a nix store path). Writable — as it shipped — either fails to open or lets concurrent runs corrupt each other. The guest's writable surfaces are tmpfs. |
| `/dev/vdb` work | **read-write** | built per run by `mkfs.ext4 -d`, and the sandbox driver points `HOME` and `TMPDIR` at it. Read-only — as it shipped — killed any script that wrote a temp file. |

The work image is also built *beside* the staged payload rather than
inside it: `mkfs.ext4 -d <dir>` copies the whole tree, so building the
image in the directory being copied makes the image a member of its own
contents.

### Writing your own rootfs

Anything satisfying these four points works:

1. `/sbin/init` mounts `/dev/vdb` at `/work`,
2. runs `tclsh /work/driver.tcl` with `VELES_SANDBOX_SCRIPT=/work/user.tcl`,
   `HOME=/work`, `TMPDIR=/work`,
3. prints `VELES_SANDBOX_EXIT:$?` on the console as its last line,
4. powers off.

Two things the nix build learned the hard way and yours will too:
**give PID 1 an explicit `PATH`** (the kernel hands it an empty
environment, so `mount` itself is not found), and **do not put `init` in
a directory that is a symlink to one holding busybox's own `init`
applet**.

## nspawn: the rootfs must be writable

`nix build .#nspawnRootfs` produces a tree, but `systemd-nspawn` takes a
per-container lock *next to* the rootfs — so a `/nix/store` path fails
with `Failed to lock …: Read-only file system`. Stage a writable copy
once:

```sh
cp -a "$(nix build .#nspawnRootfs --no-link --print-out-paths)" \
   ~/.local/share/veles/sandbox/nspawn-root
chmod -R u+w ~/.local/share/veles/sandbox/nspawn-root
```

`scripts/sandbox_vm_check.sh nspawn` does this for you.

The tree is **usr-merged** — binaries in `/usr/bin`, `/bin` a symlink —
because nspawn refuses anything else outright: *"doesn't look like it
has an OS tree (/usr/ directory is missing). Refusing."*

## jail: the root tree contract (FreeBSD, 2026-09-04)

`[sandbox] image` is the jail ROOT — a directory tree the guest sees as
`/`. Building one that works needs three things, each of which was a
separate wrong turn the first time the fence was actually run:

```sh
JROOT=/usr/local/veles-jail
# 1. pkg resolves its trust keys RELATIVE to the root, so an empty
#    directory gives "Error opening the trusted directory
#    /usr/share/keys/pkg/trusted" and then "no packages matching tcl86"
#    — which reads like a broken mirror.
sudo mkdir -p "$JROOT/usr/share/keys" "$JROOT/etc"
sudo cp -a /usr/share/keys/pkg /usr/share/keys/pkgbase-* "$JROOT/usr/share/keys/"
sudo cp -a /etc/pkg "$JROOT/etc/"
# 2. tcl86 alone is not a userland. FreeBSD-runtime brings
#    /libexec/ld-elf.so.1, /usr/bin/env and libc; without it the jail
#    execs nothing at all.
sudo pkg -r "$JROOT" install -y FreeBSD-runtime tcl86
# 3. …and FreeBSD's tcl86 installs `tclsh8.6`, never `tclsh`.
sudo veles doctor --sandbox        # → result: OK (42)
```

The backend handles (3) itself: `JailBackend::resolve_tclsh` looks
inside the root for `/usr/local/bin/tclsh8.6`, `tclsh86`, `tclsh` and
the `/usr/bin` equivalents, and passes an absolute path — which also
removes any dependence on the jail's PATH. If it finds none it says so,
with the list it searched, instead of letting jail(8) report a missing
file about a whole command line.

### The argv shape, which is not what it looks like

`jail(8)` does **not** word-split `command=`. It execs that value, and
every argv element *after* it on the jail command line becomes an
argument:

```
jail -c path=$JROOT name=… ip4=disable ip6=disable \
     command=/usr/bin/env VELES_SANDBOX_SCRIPT=/<work>/user.tcl \
     HOME=/<work> TMPDIR=/<work> /usr/local/bin/tclsh8.6 /<work>/driver.tcl
```

Packing that into one `command=…` string asks the kernel to exec a file
whose name contains spaces, and jail reports it as

```
jail: exec env VELES_SANDBOX_SCRIPT=… : No such file or directory
```

which reads like a missing interpreter. `command=` must also stay LAST
among the parameters, for the same reason.

Two more spellings that are not optional: the network switches are
`ip4=disable` / `ip6=disable` — parameters with values. `ip4.disable`
is not a parameter name jail knows, and it fails the whole call with
`jail: unknown parameter: ip4.disable`.

### What the jail root does NOT get

The root tree is **shared across runs** — a per-run copy of a FreeBSD
base tree is not affordable — so every path this backend creates inside
it carries a per-run tag (`.veles-work-<pid>_<nanos>`, nullfs-mounted
from a fresh scratch dir and unmounted afterwards). Explicit `[sandbox]
mounts` keep their configured container path, because that path is the
contract the script is written against; concurrent runs therefore stack
nullfs on the same target, which is safe for read-only mounts and is
what the mode validation is for.
