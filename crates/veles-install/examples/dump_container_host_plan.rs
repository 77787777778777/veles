//! Print a fully-enabled container-host plan, for reading the reviewed
//! script by hand:
//!
//! ```text
//! cargo run -p veles-install --example dump_container_host_plan
//! ```
//!
//! The module is compiled in by path, exactly as `tests/container_host.rs`
//! does, so this example works before `container_host` is re-exported
//! from the crate root. In a binary the module is private, so the parts
//! this demo does not call read as dead code; in the crate they do not.
#![allow(dead_code)]

#[path = "../src/container_host.rs"]
mod container_host;

use container_host::*;
use std::path::PathBuf;

fn main() -> anyhow::Result<()> {
    let digest = digest_hex(b"an image");
    let image = PinnedImage::new("docker.io/library/alpine:3.20", &format!("sha256:{digest}"))?;
    let mut options = ContainerHostOptions::new(
        StorageOptions::new(PathBuf::from("/srv/containers")),
        RootlessUser::system("gw", PathBuf::from("/srv/containers/gw")),
        RootlessUser::system("sbx", PathBuf::from("/srv/containers/sbx")),
    );
    options.tor = Some(TorOptions {
        sandbox: TorInstance {
            name: "sbxtor".into(),
            transport_port: Some("127.0.0.1:9040".parse()?),
            socks_port: None,
            isolate_socks_auth: false,
            requires_interface: None,
        },
        web: Some(TorInstance {
            name: "webtor".into(),
            transport_port: None,
            socks_port: Some("127.0.0.1:9050".parse()?),
            isolate_socks_auth: true,
            requires_interface: Some("wg0".into()),
        }),
    });
    options.firewall = Some(FirewallOptions {
        table: "velesnet".into(),
        confined: vec![
            ConfinedUser {
                user: "gw".into(),
                egress: Egress::Gateway {
                    allow: vec![AllowRule {
                        address: "10.20.30.1".parse()?,
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
        peer_interface: Some("wg0".into()),
        peer_source: Some(Cidr::parse("10.20.30.0/24")?),
    });
    options.quadlets = vec![QuadletSpec {
        user: "sbx".into(),
        name: "runner".into(),
        container: ContainerSpec {
            description: "sandbox runner".into(),
            image: image.clone(),
            extra: vec!["PublishPort=127.0.0.1:8642:8642".into()],
        },
    }];
    options.images = vec![PullRequest {
        user: "sbx".into(),
        image,
    }];
    options.network = Some("velesnet".into());

    let plan = options.plan()?;
    for line in plan.preview() {
        println!("{line}");
    }
    println!("\n--- included ---");
    for line in &plan.scope.includes {
        println!("  + {line}");
    }
    println!("--- NOT included ---");
    for line in &plan.scope.excludes {
        println!("  - {line}");
    }
    println!(
        "\n--- the privileged script (sha256 {}) ---\n",
        plan.script_sha256
    );
    print!("{}", plan.script);
    Ok(())
}
