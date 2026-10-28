//! veles-userland-broker — the userland APK's exec broker
//! (lib/arm64-v8a/libveles_userland.so; UserlandService starts it).
//!
//!   libveles_userland.so --allow-uid UID --proot P --rootfs DIR --tmp DIR
//!       [--loader P] [--shared DIR] [--sandboxes DIR] [--socket NAME] [--handoff NAME]
//!
//! veles_userland::broker says what it does with a connection, and with
//! a session the userland service hands over on `@NAME` (--handoff; only
//! this process's own uid may hand sessions over). --sandboxes is where
//! named sandboxes live, each a guest root of its own made from a copy of
//! the rootfs (without it, a request naming one is refused).

use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::sync::Arc;

use veles_userland::{broker, proto, sock};

fn usage() -> ! {
    eprintln!(
        "usage: veles-userland-broker --allow-uid UID --proot PATH --rootfs DIR --tmp DIR \
         [--loader PATH] [--shared DIR] [--sandboxes DIR] [--socket NAME] [--handoff NAME]"
    );
    std::process::exit(2)
}

fn listen(name: &str) -> UnixListener {
    match sock::listen(name) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("veles-userland-broker: cannot listen on @{name}: {e}");
            std::process::exit(1)
        }
    }
}

fn main() {
    let mut socket = proto::DEFAULT_SOCKET.to_string();
    let mut handoff = None;
    let mut sandboxes = None;
    let (mut allow, mut proot, mut rootfs, mut tmp, mut loader, mut shared) =
        (None, None, None, None, None, None);
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let mut val = || args.next().unwrap_or_else(|| usage());
        match a.as_str() {
            "--socket" => socket = val(),
            "--handoff" => handoff = Some(val()),
            "--allow-uid" => allow = Some(val().parse::<u32>().unwrap_or_else(|_| usage())),
            "--proot" => proot = Some(PathBuf::from(val())),
            "--rootfs" => rootfs = Some(PathBuf::from(val())),
            "--tmp" => tmp = Some(PathBuf::from(val())),
            "--loader" => loader = Some(PathBuf::from(val())),
            "--shared" => shared = Some(PathBuf::from(val())),
            "--sandboxes" => sandboxes = Some(PathBuf::from(val())),
            _ => usage(),
        }
    }
    let (Some(allow_uid), Some(proot), Some(rootfs), Some(tmp)) = (allow, proot, rootfs, tmp)
    else {
        usage()
    };
    let cfg = Arc::new(broker::Config {
        proot,
        loader,
        rootfs,
        tmp,
        shared,
        allow_uid,
        handoff_uid: sock::my_uid(),
        sandboxes,
    });
    let listener = listen(&socket);
    if let Some(name) = handoff {
        let l = listen(&name);
        eprintln!(
            "veles-userland-broker: taking sessions on @{name} from uid {}",
            cfg.handoff_uid
        );
        let cfg = cfg.clone();
        std::thread::spawn(move || {
            if let Err(e) = broker::serve_handoff(l, cfg) {
                eprintln!("veles-userland-broker: handoff: {e}");
                std::process::exit(1)
            }
        });
    }
    eprintln!(
        "veles-userland-broker: listening on @{socket} for uid {allow_uid} (rootfs {})",
        cfg.rootfs.display()
    );
    if let Err(e) = broker::serve(listener, cfg) {
        eprintln!("veles-userland-broker: {e}");
        std::process::exit(1)
    }
}
