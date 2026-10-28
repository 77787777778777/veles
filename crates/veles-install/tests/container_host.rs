//! Container-host plan builder: focused tests.
//!
//! The module under test is compiled here directly because `lib.rs` is not
//! part of this change; the wiring line (`mod container_host;`) is owed by
//! whoever owns that file (see the hand-off note in the report).
// Linux/FreeBSD host plans (system units, accounts, /etc paths); the
// Windows desktop profile is exercised by veles-frontends' API tests.
#![cfg(unix)]
#![allow(dead_code)]

#[path = "../src/container_host.rs"]
mod container_host;

use container_host::*;
use std::net::IpAddr;
use std::path::PathBuf;

/// Every string that belongs to the host `deploy/vps/setup.sh` was written
/// for. A generalized builder must not emit any of them: host, accounts,
/// the gitea-backed pool path and the published-media/SearXNG pieces.
// The site's own host, account and domain names are kept out of the
// public tree altogether (scripts/release export check); the generic
// pieces below are what a builder default could still reintroduce.
const REFERENCE_SITE: &[&str] = &[
    "gitea",
    "veles-pool",
    "veles-media",
    "searxng",
    "veles-sbx",
    "veles-signer",
    "velessbx",
    "velesweb",
];

fn digest(seed: &str) -> String {
    format!("sha256:{}", container_host::digest_hex(seed.as_bytes()))
}

fn user(name: &str) -> RootlessUser {
    RootlessUser::system(name, PathBuf::from(format!("/srv/containers/{name}")))
}

fn base() -> ContainerHostOptions {
    let mut o = ContainerHostOptions::new(
        StorageOptions::new(PathBuf::from("/srv/containers")),
        user("gw"),
        user("sbx"),
    );
    o.packages = PackageOptions::default();
    o
}

fn tor() -> TorOptions {
    TorOptions {
        sandbox: TorInstance {
            name: "sbxtor".into(),
            transport_port: Some("127.0.0.1:9040".parse().unwrap()),
            socks_port: None,
            isolate_socks_auth: false,
            requires_interface: None,
        },
        web: Some(TorInstance {
            name: "webtor".into(),
            transport_port: None,
            socks_port: Some("127.0.0.1:9050".parse().unwrap()),
            isolate_socks_auth: true,
            requires_interface: Some("wg0".into()),
        }),
    }
}

fn firewall() -> FirewallOptions {
    FirewallOptions {
        table: "velesnet".into(),
        confined: vec![
            ConfinedUser {
                user: "gw".into(),
                egress: Egress::Gateway {
                    allow: vec![AllowRule {
                        address: "10.20.30.1".parse::<IpAddr>().unwrap(),
                        ports: vec![8080, 9911],
                        tcp: true,
                        udp: false,
                    }],
                },
            },
            ConfinedUser {
                user: "sbx".into(),
                egress: Egress::Sandbox { tor: true },
            },
        ],
        protected_ports: vec![9900, 9911],
        peer_interface: Some("mesh0".into()),
        peer_source: Some(Cidr::parse("10.99.0.0/24").unwrap()),
    }
}

fn quadlet() -> QuadletSpec {
    QuadletSpec {
        user: "sbx".into(),
        name: "runner".into(),
        container: ContainerSpec {
            description: "sandbox runner".into(),
            image: PinnedImage::new("docker.io/library/alpine:3.20", &digest("alpine")).unwrap(),
            extra: vec!["PublishPort=127.0.0.1:8642:8642".into()],
        },
    }
}

fn full() -> ContainerHostOptions {
    let mut o = base();
    o.tor = Some(tor());
    o.firewall = Some(firewall());
    o.quadlets = vec![quadlet()];
    o.images = vec![PullRequest {
        user: "sbx".into(),
        image: PinnedImage::new("docker.io/library/alpine:3.20", &digest("alpine")).unwrap(),
    }];
    o
}

// ── unpinned images ────────────────────────────────────────────────────

#[test]
fn an_image_without_a_digest_is_refused() {
    let err = PinnedImage::new("docker.io/library/alpine:3.20", "").unwrap_err();
    assert!(err.to_string().contains("digest"), "{err}");

    let mut o = base();
    // Bypassing `new` on purpose: validate() must not trust a struct.
    o.images = vec![PullRequest {
        user: "sbx".into(),
        image: PinnedImage {
            source: "docker.io/library/alpine:3.20".into(),
            digest: "sha256:not-a-digest".into(),
        },
    }];
    let err = o.validate().unwrap_err();
    assert!(err.to_string().contains("digest"), "{err}");
}

#[test]
fn a_tag_that_means_the_local_build_is_refused() {
    for reference in [
        "localhost/veles:current",
        "veles:current",
        "quay.io/x/current:current",
        "localhost/veles",
    ] {
        let err = PinnedImage::new(reference, &digest("x")).unwrap_err();
        assert!(
            err.to_string().contains("current") || err.to_string().contains("registry"),
            "{reference}: {err}"
        );
    }
}

#[test]
fn a_quadlet_cannot_unpin_the_image_it_was_given() {
    let mut q = quadlet();
    q.container.extra = vec!["Image=localhost/veles:current".into()];
    let mut o = base();
    o.quadlets = vec![q];
    let err = o.validate().unwrap_err();
    assert!(err.to_string().contains("Image="), "{err}");
}

#[test]
fn every_pulled_image_is_rendered_with_its_digest() {
    let plan = full().plan().unwrap();
    assert_eq!(plan.images.len(), 1, "the fixture's pull was dropped");
    for request in &plan.images {
        assert!(request.image.digest.starts_with("sha256:"), "{:?}", request);
        assert!(
            plan.script.contains(&request.image.reference()),
            "pull of {} lost its digest",
            request.image.source
        );
    }
    assert!(!plan.render_all().contains(":current"));
}

// ── no site-specific values ────────────────────────────────────────────

#[test]
fn the_plan_carries_no_reference_host_values() {
    for token in REFERENCE_SITE {
        for options in [base(), full()] {
            let text = options.plan().unwrap().render_all();
            assert!(
                !text.contains(token),
                "reference-host value {token:?} leaked into the plan:\n{text}"
            );
        }
    }
}

#[test]
fn the_plan_carries_no_reference_host_values_even_when_asked_to() {
    // Feeding a reference value in as an OPTION is the operator's own
    // choice; what must not happen is the builder producing one on its
    // own. Named generically, so a default that creeps back is caught.
    let defaults = base();
    let plan = defaults.plan().unwrap();
    for token in ["/srv/pool", "/var/www", "/etc/gitea", "10.0.30."] {
        assert!(!plan.render_all().contains(token), "{token} leaked");
    }
}

// ── optional Tor and nft ───────────────────────────────────────────────

#[test]
fn tor_and_the_firewall_are_off_unless_asked_for() {
    let plan = base().plan().unwrap();
    assert!(
        !plan.files.is_empty(),
        "the base plan writes no files at all"
    );
    assert!(plan.tor.is_none());
    assert!(plan.firewall.is_none());
    let text = plan.render_all();
    for token in ["nft", "tor-instance-create", "torrc", "TransPort"] {
        assert!(!text.contains(token), "{token} present with Tor/nft off");
    }
    assert!(plan
        .files
        .iter()
        .all(|f| !f.path.to_string_lossy().contains("tor")));
}

#[test]
fn tor_enabled_without_a_single_valid_listener_is_refused() {
    let mut o = base();
    o.tor = Some(TorOptions {
        sandbox: TorInstance {
            name: "sbxtor".into(),
            transport_port: None,
            socks_port: None,
            isolate_socks_auth: false,
            requires_interface: None,
        },
        web: None,
    });
    let err = o.validate().unwrap_err();
    assert!(err.to_string().contains("port"), "{err}");
}

#[test]
fn a_redirect_to_tor_without_tor_is_refused() {
    let mut o = base();
    o.firewall = Some(firewall());
    let err = o.validate().unwrap_err();
    assert!(err.to_string().contains("tor"), "{err}");
}

#[test]
fn a_tor_instance_name_must_survive_the_package_generator() {
    let mut o = base();
    o.tor = Some(tor());
    o.firewall = Some(firewall());
    o.tor.as_mut().unwrap().sandbox.name = "sbx-tor".into();
    let err = o.validate().unwrap_err();
    assert!(err.to_string().contains("alphanumeric"), "{err}");
}

#[test]
fn a_firewall_that_confines_nobody_is_refused() {
    let mut o = base();
    o.firewall = Some(FirewallOptions {
        table: "velesnet".into(),
        confined: vec![],
        protected_ports: vec![],
        peer_interface: None,
        peer_source: None,
    });
    let err = o.validate().unwrap_err();
    assert!(err.to_string().contains("confined"), "{err}");
}

#[test]
fn a_peer_allowrule_needs_both_an_interface_and_a_source() {
    let mut o = base();
    o.firewall = Some(firewall());
    o.tor = None;
    o.firewall.as_mut().unwrap().confined[1].egress = Egress::Gateway { allow: vec![] };
    o.firewall.as_mut().unwrap().peer_source = None;
    let err = o.validate().unwrap_err();
    assert!(err.to_string().contains("interface"), "{err}");
    o.firewall.as_mut().unwrap().peer_interface = None;
    o.firewall.as_mut().unwrap().peer_source = Some(Cidr::parse("10.99.0.0/24").unwrap());
    let err = o.validate().unwrap_err();
    assert!(err.to_string().contains("interface"), "{err}");
}

#[test]
fn a_cidr_is_not_accepted_on_trust() {
    for bad in ["10.0.0.0/33", "10.0.0.0", "not-an-address", "10.0.0.0/", ""] {
        assert!(Cidr::parse(bad).is_err(), "{bad:?} parsed");
    }
    let good = Cidr::parse("10.99.0.0/24").unwrap();
    assert_eq!(good.to_string(), "10.99.0.0/24");
}

// ── validated options ──────────────────────────────────────────────────

/// A mutation applied to otherwise-valid options.
type Mutation = Box<dyn Fn(&mut ContainerHostOptions)>;

#[test]
fn unvalidated_values_never_reach_the_script() {
    let cases: Vec<Mutation> = vec![
        Box::new(|o| o.gateway.name = "gw; rm -rf /".into()),
        Box::new(|o| o.sandbox.name = "Root".into()),
        Box::new(|o| o.sandbox.name = "root".into()),
        Box::new(|o| o.gateway.home = PathBuf::from("relative/home")),
        Box::new(|o| o.storage.root = PathBuf::from("/srv/../etc")),
        Box::new(|o| o.storage.root = PathBuf::from("/")),
        Box::new(|o| o.socket.dir = PathBuf::from("/run/combined\nnft flush ruleset")),
        Box::new(|o| o.unit_prefix = "veles unit".into()),
    ];
    for (index, mutate) in cases.iter().enumerate() {
        let mut o = base();
        mutate(&mut o);
        assert!(o.validate().is_err(), "case {index} was accepted");
    }
}

#[test]
fn the_script_is_quoted_against_the_values_it_does_accept() {
    let plan = full().plan().unwrap();
    assert!(plan.script.starts_with("#!/bin/sh\n"));
    assert!(plan.script.contains("\nset -eu\n"));
    // The guard is one exact line, not any line containing `id -u`.
    assert!(
        plan.script
            .lines()
            .any(|l| l == "[ \"$(id -u)\" = 0 ] || die 'run this script as root (sudo)'"),
        "no root guard:\n{}",
        plan.script
    );
    // Every managed user is quoted at every use site.
    assert!(plan.script.contains("'gw'"));
    assert!(plan.script.contains("'sbx'"));
    // The user manager needs the runtime and bus of the account itself,
    // or `systemctl --user` cannot reach it. Both values are pinned
    // exactly: a typo in either leaves podman talking to nothing.
    assert!(
        plan.script
            .contains("runuser -u \"$u\" -- env XDG_RUNTIME_DIR=\"/run/user/$uid\" \\"),
        "as_user does not set XDG_RUNTIME_DIR:\n{}",
        plan.script
    );
    assert!(
        plan.script
            .contains("DBUS_SESSION_BUS_ADDRESS=\"unix:path=/run/user/$uid/bus\""),
        "as_user does not set the user bus address"
    );
    assert!(
        plan.script.contains("cd /\n"),
        "podman refuses a dir it cannot read"
    );
}

/// Command lines only: comments and heredoc bodies excluded, so a pin
/// cannot be satisfied by a comment that mentions the command.
fn commands(script: &str) -> Vec<&str> {
    script
        .lines()
        .filter(|l| {
            let t = l.trim();
            !t.is_empty() && !t.starts_with('#') && !t.starts_with("cat >")
        })
        .collect()
}

#[test]
fn the_privileged_script_never_ignores_a_failure() {
    for line in commands(&full().plan().unwrap().script) {
        assert!(!line.contains("|| true"), "ignored failure: {line}");
        assert!(
            !line.contains("2>/dev/null ||"),
            "suppressed failure: {line}"
        );
    }
}

#[test]
fn the_firewall_is_the_loaded_table_and_it_is_a_transaction() {
    let plan = full().plan().unwrap();
    let cmds = commands(&plan.script);
    let count = |needle: &str| cmds.iter().filter(|c| c.contains(needle)).count();
    // Syntax-checked before it is loaded, and again after the reload.
    assert_eq!(count("nft -c -f"), 2, "{:#?}", cmds);
    // reenable, because the unit's [Install] target moved once already.
    assert_eq!(count("systemctl reenable"), 1, "{:#?}", cmds);
    // The table itself is reloaded, never restarted: a restart leaves the
    // accounts unconfined between its stop and its start.
    let nft_unit = "'veles-nft.service'";
    assert_eq!(
        cmds.iter()
            .filter(|c| c.contains("reload-or-restart") && c.contains(nft_unit))
            .count(),
        1,
        "{:#?}",
        cmds
    );
    assert_eq!(
        cmds.iter()
            .filter(|c| c.contains("systemctl restart") && c.contains(nft_unit))
            .count(),
        0,
        "a restart of the table: {:#?}",
        cmds
    );
    // Tor's own units may be restarted; they are not the firewall.
    assert!(
        cmds.iter()
            .any(|c| c.contains("reload-or-restart") && c.contains("tor@")),
        "Tor is never reloaded after its torrc is written: {:#?}",
        cmds
    );
}

// ── what the plan actually builds ──────────────────────────────────────

#[test]
fn linger_subids_and_the_podman_socket_are_planned_in_that_order() {
    let plan = full().plan().unwrap();
    let script = &plan.script;
    let useradd = script.find("useradd --system").expect("no useradd");
    let subids = script.find("--add-subuids").expect("no subuids");
    let linger = script.find("enable-linger").expect("no linger");
    let socket = script
        .find("podman --log-level=info system service")
        .expect("no podman service unit");
    assert!(useradd < subids, "subids before the user exists");
    assert!(subids < linger, "linger before subids");
    assert!(linger < socket, "socket before its user is complete");
    assert!(script.contains("--add-subgids"));
    assert!(script.contains("/etc/tmpfiles.d/"));
    assert!(script.contains("systemd-tmpfiles --create"));
    assert!(script.contains("as_user 'sbx' systemctl --user enable --now"));
}

#[test]
fn the_firewall_and_its_user_manager_drop_in_are_planned_together() {
    let plan = full().plan().unwrap();
    let script = &plan.script;
    assert!(script.contains("nft -c -f"), "no syntax check");
    assert!(
        script.contains("reenable"),
        "a moved [Install] needs reenable"
    );
    assert!(script.contains("reload-or-restart"), "no reload");
    assert!(script.contains("user@${uid_"), "no user-manager drop-in");
    assert!(
        script.contains("nft list table inet velesnet"),
        "drop-in does not gate on the table"
    );
    let nft = plan.firewall.as_ref().unwrap();
    let rules: String = nft
        .contents
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        rules.contains("redirect to :9040"),
        "no Tor redirect:\n{rules}"
    );
    assert!(rules.contains("\"sbx\""), "sandbox user not confined");
    assert!(rules.contains("tcp dport { 8080, 9911 }"), "{rules}");
    let held = plan
        .files
        .iter()
        .find(|f| {
            f.path.to_string_lossy().ends_with("veles.conf")
                && f.path.to_string_lossy().contains("user@.service.d")
        })
        .expect("no user-manager drop-in");
    assert!(
        held.contents
            .lines()
            .any(|l| l.trim() == "ExecStartPre=+/usr/sbin/nft list table inet velesnet"),
        "the drop-in does not read the table back:\n{}",
        held.contents
    );
    // It orders before the table and does not hard-require it.
    assert!(held.contents.contains("After=veles-nft.service"));
    assert!(held.contents.contains("Wants=veles-nft.service"));
    assert!(
        !held.contents.contains("Requires=veles-nft.service"),
        "a Requires= would restart every container when the table reloads"
    );
}

#[test]
fn the_script_is_deterministic_and_every_file_appears_exactly_once() {
    let first = full().plan().unwrap();
    let second = full().plan().unwrap();
    assert_eq!(first.script, second.script);
    assert_eq!(first.script_sha256, second.script_sha256);
    for file in &first.files {
        let marker = container_host::heredoc_marker(&file.contents);
        assert_eq!(
            first.script.matches(&marker).count(),
            2,
            "marker {marker} for {} is not exactly one heredoc",
            file.path.display()
        );
        assert!(
            file.contents.ends_with('\n'),
            "{} is not newline-terminated",
            file.path.display()
        );
    }
    let mut changed = full();
    changed.unit_prefix = "other".into();
    assert_ne!(
        changed.plan().unwrap().script_sha256,
        first.script_sha256,
        "an input change must change the reviewed script"
    );
}

#[test]
fn a_file_content_that_looks_like_the_marker_stays_inside_its_heredoc() {
    let mut q = quadlet();
    q.container.extra = vec!["Environment=VELES_PLAN=harmless".into()];
    let mut o = base();
    o.quadlets = vec![q];
    let plan = o.plan().unwrap();
    assert!(!plan.files.is_empty(), "the quadlet was dropped");
    for file in &plan.files {
        assert!(!file
            .contents
            .contains(&container_host::heredoc_marker(&file.contents)));
    }
}

#[test]
fn packages_follow_what_was_enabled() {
    let base_plan = base().plan().unwrap();
    assert!(base_plan.script.contains("podman"), "no container engine");
    assert!(!base_plan.script.contains("--no-install-recommends tor"));

    let mut o = full();
    o.packages.install_missing = true;
    let plan = o.plan().unwrap();
    let install = plan
        .script
        .lines()
        .find(|l| l.contains("apt-get install"))
        .expect("no install command with install_missing on");
    assert!(install.contains("'podman'"), "{install}");
    assert!(install.contains("'shadow'"), "{install}");
    assert!(install.contains("'tor'"), "{install}");
    assert!(install.contains("'nftables'"), "{install}");
    assert!(plan.packages.contains(&"podman".to_string()));
    assert!(plan.packages.contains(&"tor".to_string()));

    // With the optional halves off, their packages must not be asked for.
    let mut plain = o.clone();
    plain.tor = None;
    plain.firewall = None;
    plain.quadlets.clear();
    let install = plain
        .plan()
        .unwrap()
        .script
        .lines()
        .find(|l| l.contains("apt-get install"))
        .map(str::to_string)
        .expect("no install command");
    assert!(!install.contains("'tor'"), "{install}");
    assert!(!install.contains("'nftables'"), "{install}");

    o.packages.install_missing = false;
    let plan = o.plan().unwrap();
    assert!(
        plan.packages.is_empty(),
        "the operator asked for no installs"
    );
    assert!(!plan.script.contains("apt-get install"));
    assert!(
        plan.script.contains("command -v"),
        "no preflight for a missing package"
    );
}

#[test]
fn the_bind_mount_is_guarded_and_derived_from_the_source() {
    let mut o = base();
    o.storage = StorageOptions {
        root: PathBuf::from("/srv/containers"),
        source: Some(PathBuf::from("/mnt/data/containers")),
        requires_mounts_for: Some(PathBuf::from("/mnt/data")),
        bind: true,
        mode: 0o755,
        home_mode: 0o700,
    };
    let plan = o.plan().unwrap();
    let script = &plan.script;
    assert!(script.contains("grep -qF '/mnt/data/containers /srv/containers ' /etc/fstab"));
    assert!(
        script.contains("x-systemd.requires-mounts-for=/mnt/data"),
        "{script}"
    );
    // A bind with no stated mount unit is not the plan's guess to make.
    let mut unstated = o.clone();
    unstated.storage.requires_mounts_for = None;
    let plain = unstated.plan().unwrap();
    assert!(!plain.script.contains("x-systemd.requires-mounts-for"));
    assert!(plain.script.contains("bind 0 0"));
    let mut off = o.clone();
    off.storage.bind = false;
    assert!(!off.plan().unwrap().script.contains("/etc/fstab"));
    // A home outside the root would be hidden by the bind.
    let mut outside = o.clone();
    outside.sandbox.home = PathBuf::from("/home/sbx");
    assert!(outside.plan().is_err());
}

#[test]
fn the_scope_names_what_this_plan_does_not_do() {
    let scope = base().plan().unwrap().scope;
    let text = scope.excludes.join(" ").to_lowercase();
    for missing in ["pds", "media", "pool", "searxng"] {
        assert!(
            text.contains(missing),
            "{missing} is not declared out of scope: {}",
            scope.excludes.join(" / ")
        );
    }
    assert!(!text.contains("complete"));
    assert!(!scope
        .includes
        .iter()
        .any(|i| i.to_lowercase().contains("complete")));
}

#[test]
fn a_name_is_never_interpolated_inside_a_grep_pattern_with_quotes() {
    // Regression: the subuid check once emitted grep -q "^'gw':", which
    // never matches, so the range was re-added on every run.
    let plan = base().plan().unwrap();
    assert!(
        plan.script.contains("grep -q \"^gw:\" /etc/subuid"),
        "{}",
        plan.script
    );
    assert!(
        !plan.script.contains("\"^'"),
        "a quoted name reached a grep pattern"
    );
    // And no systemd unit name is left half-quoted (`tor@'x'.service`).
    let full_plan = full().plan().unwrap();
    for line in full_plan.script.lines() {
        assert!(
            !line.contains("@'") && !line.contains("'."),
            "a unit name is split around a quote: {line}"
        );
    }
}

#[test]
fn the_socket_serves_the_sandbox_account_and_nobody_else() {
    // The directory's group is the gate, so it must not be the sandbox's
    // own group (that would exclude the gateway) and the socket must
    // serve the sandbox store, not a third account.
    let mut wrong_user = base();
    wrong_user.socket.user = "gw".into();
    let err = wrong_user.plan().unwrap_err();
    assert!(err.to_string().contains("sandbox account"), "{err}");

    let mut same_group = base();
    same_group.socket.group = "sbx".into();
    let err = same_group.plan().unwrap_err();
    assert!(err.to_string().contains("gateway"), "{err}");

    let mut outside_run = base();
    outside_run.socket.dir = PathBuf::from("/var/run/gw");
    let err = outside_run.plan().unwrap_err();
    assert!(err.to_string().contains("tmpfs"), "{err}");

    let good = base().plan().unwrap();
    assert!(
        good.script.contains("-g 'gw'"),
        "the gate is not the gateway's group"
    );
    assert!(good
        .files
        .iter()
        .any(|f| f.path.ends_with("sbx-podman.socket")));
    assert!(good.script.contains("systemd-tmpfiles --create"));
}

#[test]
fn a_quadlet_or_rule_for_an_unknown_account_is_refused() {
    let mut orphan_quadlet = base();
    orphan_quadlet.quadlets = vec![QuadletSpec {
        user: "absent".into(),
        ..quadlet()
    }];
    let err = orphan_quadlet.plan().unwrap_err();
    assert!(err.to_string().contains("absent"), "{err}");

    let mut orphan_pull = base();
    orphan_pull.images = vec![PullRequest {
        user: "absent".into(),
        image: PinnedImage::new("docker.io/library/alpine:3.20", &digest("x")).unwrap(),
    }];
    assert!(orphan_pull.plan().is_err());

    let mut orphan_rule = base();
    orphan_rule.tor = Some(tor());
    orphan_rule.firewall = Some(firewall());
    orphan_rule.firewall.as_mut().unwrap().confined[0].user = "absent".into();
    let err = orphan_rule.plan().unwrap_err();
    assert!(err.to_string().contains("absent"), "{err}");
}

#[test]
fn the_artifacts_are_accepted_by_the_real_parsers() {
    // The strongest check available without a host: the generated files go
    // through the system tools themselves. A missing tool skips rather
    // than fails, so the suite stays portable.
    let plan = full().plan().unwrap();
    let dir = tempfile::tempdir().unwrap();

    // `nft -c` parses the ruleset but needs netlink, so it runs in a
    // private network namespace. The fixture's account names become ones
    // that exist: nft resolves them while parsing.
    let rules = plan.firewall.as_ref().unwrap();
    let existing = rules
        .contents
        .replace("\"gw\"", "\"nobody\"")
        .replace("\"sbx\"", "\"root\"");
    let path = dir.path().join("veles.nft");
    std::fs::write(&path, &existing).unwrap();
    for argv in [&["unshare", "-rn", "nft"][..], &["nft"][..]] {
        let out = std::process::Command::new(argv[0])
            .args(&argv[1..])
            .arg("-c")
            .arg("-f")
            .arg(&path)
            .output();
        let Ok(out) = out else { continue };
        if out.status.success() {
            break;
        }
        let stderr = String::from_utf8_lossy(&out.stderr);
        // No netlink here: not a verdict on the file.
        // Nor is a missing nft behind an available unshare (the Nix build
        // sandbox): skip, as the comment above promises.
        if stderr.contains("cache initialization failed")
            || stderr.contains("Operation not permitted")
            || stderr.contains("failed to execute")
        {
            continue;
        }
        panic!("nft rejected the ruleset:\n{stderr}\n---\n{existing}");
    }

    // systemd's own parser on every unit and socket. A drop-in is not a
    // unit by itself, so it is verified as the fragment it is: its body
    // appended to a minimal unit of the type it patches. A tmpfiles.d
    // entry has its own format and is not verified here.
    let Some(systemd_analyze) = which("systemd-analyze") else {
        return;
    };
    for file in &plan.files {
        let shown = file.path.to_string_lossy();
        if !shown.contains("systemd") {
            continue;
        }
        let name = file
            .path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unit");
        let target = if shown.contains(".service.d") {
            let wrapped = dir.path().join("wrapped.service");
            std::fs::write(
                &wrapped,
                format!(
                    "[Unit]\nDescription=drop-in under test\n\n[Service]\nType=oneshot\nExecStart=/bin/true\n\n{}",
                    file.contents
                ),
            )
            .unwrap();
            wrapped
        } else {
            let plain = dir.path().join(name);
            std::fs::write(&plain, &file.contents).unwrap();
            plain
        };
        let out = std::process::Command::new(&systemd_analyze)
            .arg("verify")
            .arg(&target)
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        // Complaints about THIS machine's paths (`/usr/sbin/nft` is not
        // where NixOS puts it) are not shape errors in the unit.
        let shape_error = stderr.lines().any(|l| {
            let l = l.trim();
            !l.is_empty()
                && !l.contains("is not executable")
                && !l.contains("socket cannot be started")
                && !l.contains("legacy directory")
                && !l.contains("cups")
        });
        assert!(
            !shape_error,
            "systemd-analyze rejected {name}:\n{stderr}\n---\n{}",
            file.contents
        );
    }
}

fn which(binary: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(binary))
        .find(|candidate| candidate.is_file())
}

#[test]
fn the_script_parses_as_posix_shell() {
    // `sh -n` reads the file without running any of it: the reviewed
    // artifact must at least be a syntactically real script.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("plan.sh");
    std::fs::write(&path, &full().plan().unwrap().script).unwrap();
    let out = std::process::Command::new("sh")
        .arg("-n")
        .arg(&path)
        .output()
        .expect("sh -n did not run");
    assert!(
        out.status.success(),
        "sh -n rejected the plan:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // And with the optional halves enabled, which is the larger script.
    let mut o = full();
    o.packages.install_missing = true;
    o.network = Some("velesnet".into());
    o.storage.source = Some(PathBuf::from("/mnt/data"));
    o.storage.bind = true;
    o.storage.requires_mounts_for = Some(PathBuf::from("/mnt/data"));
    std::fs::write(&path, &o.plan().unwrap().script).unwrap();
    let out = std::process::Command::new("sh")
        .arg("-n")
        .arg(&path)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "sh -n rejected the full plan:\n{}\n---\n{}",
        String::from_utf8_lossy(&out.stderr),
        o.plan().unwrap().script
    );
}

#[test]
fn a_quadlet_lands_in_its_users_own_quadlet_directory() {
    let plan = full().plan().unwrap();
    let quadlet = &plan.quadlets[0];
    assert_eq!(
        quadlet.path,
        PathBuf::from("/srv/containers/sbx/.config/containers/systemd/runner.container")
    );
    assert!(quadlet.contents.contains(&format!(
        "Image=docker.io/library/alpine:3.20@{}",
        digest("alpine")
    )));
    assert!(plan.script.contains("PublishPort=127.0.0.1:8642:8642"));
    assert!(plan
        .script
        .contains("as_user 'sbx' systemctl --user daemon-reload"));
}

#[test]
fn every_quadlet_joins_the_planned_network_and_never_pulls_on_start() {
    let mut options = full();
    options.network = Some("veles-net".into());
    let plan = options.plan().unwrap();
    assert!(!plan.quadlets.is_empty());
    for quadlet in &plan.quadlets {
        assert!(
            quadlet.contents.contains("\nNetwork=veles-net\n"),
            "{}",
            quadlet.contents
        );
        assert!(
            quadlet.contents.contains("\nPull=never\n"),
            "{}",
            quadlet.contents
        );
    }
    let mut with_pull = quadlet();
    with_pull.container.extra.push("Pull=always".into());
    let mut options = full();
    options.quadlets = vec![with_pull];
    assert!(options.plan().is_err(), "Pull= belongs to the plan");
}

#[test]
fn the_script_waits_for_each_lingering_user_manager_before_using_it() {
    let plan = full().plan().unwrap();
    let linger = plan.script.find("loginctl enable-linger 'sbx'").unwrap();
    let start = plan
        .script
        .find("systemctl start \"user@$(id -u 'sbx').service\"")
        .unwrap();
    let first_use = plan.script.find("as_user 'sbx'").unwrap();
    assert!(linger < start && start < first_use, "{}", plan.script);
}

#[test]
fn every_quadlet_directory_level_belongs_to_its_account() {
    let plan = full().plan().unwrap();
    assert!(
        plan.script.contains(
            "install -d -o 'sbx' -g 'sbx' '/srv/containers/sbx/.config' '/srv/containers/sbx/.config/containers' '/srv/containers/sbx/.config/containers/systemd'"
        ),
        "{}",
        plan.script
    );
}

#[test]
fn a_keep_id_quadlet_has_its_layers_warmed_after_the_pull() {
    let mut options = full();
    options.quadlets[0]
        .container
        .extra
        .push("UserNS=keep-id:uid=1000,gid=1000".into());
    let plan = options.plan().unwrap();
    let pull = plan.script.find("podman pull -q").unwrap();
    let warm = plan
        .script
        .find("podman create --pull=never --userns='keep-id:uid=1000,gid=1000'")
        .unwrap();
    assert!(pull < warm, "{}", plan.script);
    assert!(plan.quadlets[0].contents.contains("TimeoutStartSec=900"));
}
