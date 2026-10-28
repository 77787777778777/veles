# veles dev-sandbox image — a full developer environment for the model
# to work in, jailed so it cannot touch the host.
#
# Design (2026-09-03, from the plan's WP4 and USER direction):
#   - Base is Ubuntu LTS, because that is broadly what models were RL'd
#     on; apt/pip/npm behave the way a model expects.
#   - The model gets a REAL dev environment: compilers, python+pip+venv,
#     node+npm, git, and the usual unix toolkit. It runs as root INSIDE
#     the container so package installs just work, and gets network
#     access when the backend is configured for it (`[sandbox] network`).
#   - Host safety is NOT this image's job — it comes from running the
#     image in rootless podman (user namespace, caps, no-new-privileges).
#     Inside the container the user may even be root-equivalent; the jail
#     is the fence. So we do NOT strip tools to "protect" the host here —
#     we give a capable environment and let the container bound it.
#   - This image ALSO carries tclsh, so it still satisfies the
#     safe-interp sandbox contract (tclsh on PATH) if pointed at the
#     pure-compute backends. One image, both postures.
#
# Build: scripts/build-sandbox-image.sh   (needs podman or docker)

# FULLY QUALIFIED on purpose. A bare `ubuntu:24.04` needs the host to
# have unqualified-search registries configured, and podman on NixOS
# ships none — the build dies with "short-name did not resolve to an
# alias and no unqualified-search registries are defined". Naming the
# registry makes this Dockerfile build the same everywhere instead of
# depending on a host setting nobody remembers.
FROM docker.io/library/ubuntu:24.04@sha256:68ee22004a9109b44973313b02ff7b1de250f1de0f5ecb628d73f579aaff33ac

# Deterministic, non-interactive builds.
ENV DEBIAN_FRONTEND=noninteractive \
    LANG=C.UTF-8 \
    LC_ALL=C.UTF-8

# Core toolkit + languages + build tools. Kept to what models actually
# reach for; the container runs as root, so `apt-get install <more>` at
# run time needs no ceremony. tcl/tclsh is present so the safe-interp
# driver still runs. poppler-utils (pdftotext) reads the PDFs web_extract
# downloads into /work/downloads, and `file` is what a model reaches for
# first to see what it got (2026-10-12: both were missing, mid-turn).
# imagemagick (ImageMagick 6: convert/identify/composite, no `magick`)
# is what the meme pipeline on the VPS reaches for (the memory
# reference/projects/veles-vps-image-meme-pipeline); it was apt-installed
# into one box by hand, which a recreated box loses (2026-10-13).
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates curl wget gnupg \
        git openssh-client rsync \
        build-essential cmake pkg-config \
        python3 python3-pip python3-venv python3-dev \
        nodejs npm \
        tcl tcl-dev tcllib \
        jq ripgrep fd-find \
        sqlite3 \
        poppler-utils file imagemagick \
        unzip xz-utils tar \
        sudo locales \
    && sed -i 's/^# *C.UTF-8/C.UTF-8/' /etc/locale.gen && locale-gen \
    && rm -rf /var/lib/apt/lists/*

# Root INSIDE the container, on purpose: rootless podman maps that to the
# unprivileged host user, so the jail stays the fence while apt/pip/npm
# install simply WORK — no sudo setuid dance, and no-new-privileges (which
# the backend applies in every posture) cannot break anything root already
# is. sudo is installed anyway because models reach for `sudo apt-get` by
# muscle memory, and sudo-as-root needs no privilege transition.
RUN echo 'export PATH="$HOME/.local/bin:$PATH"' >> /root/.bashrc

# apt's download methods drop to the `_apt` user, which needs setgroups,
# and the box has no capability for it: every `apt-get update` died with
# "setgroups 0 failed - Operation not permitted" (VPS, 2026-10-12), so the
# run-time install promised above did not work at all. Downloading as the
# box's root changes nothing outside the box: the jail is the fence. Its
# root cannot use the image's archives/partial either (owned by _apt, mode
# 0700, and no CAP_DAC_OVERRIDE or CAP_CHOWN to get past that), so
# downloads go to a cache directory of root's own.
RUN printf '%s\n' 'APT::Sandbox::User "root";' \
        'Dir::Cache::Archives "/var/cache/apt/veles-archives/";' \
        > /etc/apt/apt.conf.d/99veles-no-apt-user

# The guest browser (PLAN-challenge-relay.md D6): a page opened for a
# session that is not the owner's is opened HERE, in that session's own
# box, so its only way out is the box's (Tor, on the VPS) and file:// shows
# nothing but the box. Google Chrome rather than a test build: Cloudflare
# let a real, headed Chrome through on its own (2026-10-12). It draws on
# a private Xvfb inside the box, and agent-browser drives it: the npm
# package's prebuilt Linux binary, checked against the hash below
# (agent-browser 0.38.1, the version the laptop runs).
ARG AGENT_BROWSER_VERSION=0.38.1
ARG AGENT_BROWSER_SHA256=5100149a1903211c889de4e545bf36d90803740cea4f99aa22651649f9205ea1
RUN apt-get update && apt-get install -y --no-install-recommends \
        xvfb xauth fonts-liberation fonts-noto-color-emoji \
    && curl -fsSLo /tmp/chrome.deb \
        https://dl.google.com/linux/direct/google-chrome-stable_current_amd64.deb \
    && apt-get install -y --no-install-recommends /tmp/chrome.deb \
    && rm -f /tmp/chrome.deb \
    && curl -fsSLo /tmp/ab.tgz \
        "https://registry.npmjs.org/agent-browser/-/agent-browser-${AGENT_BROWSER_VERSION}.tgz" \
    && tar -xzf /tmp/ab.tgz -C /tmp package/bin/agent-browser-linux-x64 \
    && install -m 0755 /tmp/package/bin/agent-browser-linux-x64 /usr/local/bin/agent-browser \
    && echo "${AGENT_BROWSER_SHA256}  /usr/local/bin/agent-browser" | sha256sum -c - \
    && rm -rf /tmp/ab.tgz /tmp/package /var/lib/apt/lists/*
ENV AGENT_BROWSER_EXECUTABLE_PATH=/usr/bin/google-chrome-stable

# pip as root without the PEP-668 "externally managed" wall — a venv is
# still the polite choice, but a sandbox is disposable.
ENV PIP_BREAK_SYSTEM_PACKAGES=1

WORKDIR /root

# The sandbox backends mount the scratch workspace at /work and set
# HOME/TMPDIR accordingly; this WORKDIR is just the image default.
# Default command keeps the container inspectable if run bare.
CMD ["/bin/bash"]
