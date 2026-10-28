//! Host components apply to a Linux host (accounts, nginx, systemd,
//! WireGuard); previews are cross-platform but these fixtures are not.
#![cfg(unix)]
use std::path::{Path, PathBuf};
use veles_install::container_host::PinnedImage;
use veles_install::host_components::{
    HostCommands, HostComponent, MediaOptions, PdsOptions, PoolHubOptions, PoolMemberOptions,
    SecretCommandRunner,
};

fn me() -> String {
    veles_install::current_user_name().unwrap()
}

fn pds_options(component: HostComponent) -> PdsOptions {
    match component {
        HostComponent::Pds(options) => *options,
        other => panic!("expected PDS fixture, got {other:?}"),
    }
}

/// The typed command boundary, recorded. `fail_on` fails the first call
/// whose argv (joined) contains it; certbot "issues" a certificate under
/// the fixture root; curl reads the probe back from the fixture root; wg
/// answers from the fixture's interface config.
struct Fake {
    root: PathBuf,
    calls: Vec<String>,
    fail_on: Option<String>,
    issue_certificate: bool,
    link_seen_by_certbot: bool,
}

impl Fake {
    fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            calls: Vec::new(),
            fail_on: None,
            issue_certificate: true,
            link_seen_by_certbot: false,
        }
    }

    fn record(&mut self, argv: &[String]) -> anyhow::Result<()> {
        let joined = argv.join(" ");
        self.calls.push(joined.clone());
        if let Some(needle) = &self.fail_on {
            if joined.contains(needle.as_str()) {
                self.fail_on = None;
                anyhow::bail!("injected failure: {joined}");
            }
        }
        Ok(())
    }
}

impl HostCommands for Fake {
    fn run(&mut self, argv: &[String]) -> anyhow::Result<()> {
        self.record(argv)?;
        if argv[0] == "certbot" {
            let domain = &argv[argv.iter().position(|a| a == "-d").unwrap() + 1];
            let site = if domain.starts_with("media") {
                "veles-media"
            } else {
                "veles-pds"
            };
            self.link_seen_by_certbot = std::fs::symlink_metadata(
                self.root
                    .join(format!("etc/nginx/sites-enabled/{site}.conf")),
            )
            .is_ok();
            if self.issue_certificate {
                let live = self.root.join("etc/letsencrypt/live").join(domain);
                std::fs::create_dir_all(&live).unwrap();
                for file in ["fullchain.pem", "privkey.pem", "chain.pem"] {
                    std::fs::write(live.join(file), "fixture").unwrap();
                }
            }
        }
        Ok(())
    }

    fn query(&mut self, argv: &[String]) -> anyhow::Result<bool> {
        self.record(argv)?;
        Ok(false)
    }

    fn output(&mut self, argv: &[String]) -> anyhow::Result<(Vec<u8>, Vec<u8>)> {
        self.record(argv)?;
        match argv[0].as_str() {
            "curl" => {
                let url = argv.last().unwrap();
                let name = url.rsplit('/').next().unwrap();
                Ok((
                    std::fs::read(self.root.join("srv/veles/media-public").join(name))?,
                    vec![],
                ))
            }
            "wg" if argv[3] == "public-key" => {
                let conf = std::fs::read_to_string(self.root.join("etc/wireguard/wg0.conf"))?;
                let private = conf
                    .lines()
                    .find_map(|l| l.strip_prefix("PrivateKey = "))
                    .unwrap();
                Ok((
                    veles_core::pool::wg_public_b64(private)?.into_bytes(),
                    vec![],
                ))
            }
            "wg" => Ok((b"hubkey=\t1761600000\n".to_vec(), vec![])),
            other => anyhow::bail!("unexpected output command {other}"),
        }
    }

    fn pause(&mut self, _seconds: u64) {}
}

fn media() -> HostComponent {
    HostComponent::Media(MediaOptions {
        public_url: "https://media.example".into(),
        publish_dir: "/srv/veles/media-public".into(),
        gateway_user: me(),
        acme_email: "acme@example".into(),
        max_age_days: Some(30),
    })
}

fn apply(
    components: &[HostComponent],
    root: &Path,
    fake: &mut Fake,
) -> anyhow::Result<serde_json::Value> {
    veles_install::host_components::apply_all(
        components,
        root,
        &std::collections::BTreeMap::new(),
        fake,
    )
}

#[test]
fn media_preview_reviews_safe_static_files_without_deleting_existing_media() {
    let preview = media().preview().unwrap();
    assert_eq!(preview["component"], "local-media");
    assert_eq!(preview["dns_changed"], false);
    assert_eq!(preview["existing_media_preserved"], true);
    let files = preview["host_effects"]["files"].to_string();
    assert!(files.contains("default_type application/octet-stream"));
    assert!(files.contains("Content-Security-Policy"));
    assert!(files.contains("location ~ /\\\\."));
    assert!(files.contains("/etc/nginx/sites-available/veles-media.conf"));
    let text = preview["host_effects"].to_string();
    assert!(text.contains("certbot"));
    assert!(!text.contains("rm -rf"));
    assert!(preview["gateway_configuration"]
        .as_str()
        .unwrap()
        .contains("media_public_max_age_days = 30"));
}

#[test]
fn media_first_run_enables_port_80_before_acme_then_serves_and_probes() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("host");
    let publish = root.join("srv/veles/media-public");
    std::fs::create_dir_all(&publish).unwrap();
    std::fs::write(publish.join("existing.png"), "existing").unwrap();
    let mut fake = Fake::new(&root);
    let result = apply(&[media()], &root, &mut fake).unwrap();
    let report = &result["host_components"][0];
    assert_eq!(report["probe_verified"], true, "{result}");
    assert!(
        fake.link_seen_by_certbot,
        "HTTP-01 needs the port-80 vhost ENABLED before certbot runs"
    );
    let certbot = fake
        .calls
        .iter()
        .position(|c| c.starts_with("certbot"))
        .unwrap();
    let first_reload = fake
        .calls
        .iter()
        .position(|c| c == "systemctl reload nginx.service")
        .unwrap();
    assert!(first_reload < certbot, "{:?}", fake.calls);
    assert!(fake.calls.last().unwrap().starts_with("curl"));
    let vhost =
        std::fs::read_to_string(root.join("etc/nginx/sites-available/veles-media.conf")).unwrap();
    assert!(vhost.contains("listen 443 ssl"));
    assert!(
        std::fs::symlink_metadata(root.join("etc/nginx/sites-enabled/veles-media.conf"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        std::fs::read(publish.join("existing.png")).unwrap(),
        b"existing"
    );
    assert_eq!(
        std::fs::read_dir(&publish).unwrap().count(),
        1,
        "the probe is removed"
    );
    assert!(report["gateway_configuration"]
        .as_str()
        .unwrap()
        .contains("media_public_dir = \"/srv/veles/media-public\""));
    // The lock is root's, never in an account's own data directory.
    assert!(root
        .join("var/lib/veles-install/host/.operation.lock")
        .is_file());
}

#[test]
fn media_acme_failure_restores_the_vhost_and_reloads_nginx() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("host");
    let available = root.join("etc/nginx/sites-available/veles-media.conf");
    std::fs::create_dir_all(available.parent().unwrap()).unwrap();
    std::fs::create_dir_all(root.join("etc/nginx/sites-enabled")).unwrap();
    std::fs::write(&available, "# the operator's previous vhost\n").unwrap();
    let mut fake = Fake::new(&root);
    fake.fail_on = Some("certbot".into());
    let error = apply(&[media()], &root, &mut fake).unwrap_err();
    let text = format!("{error:#}");
    assert!(text.contains("ACME HTTP-01"), "{text}");
    assert!(text.contains("compensated"), "{text}");
    assert_eq!(
        std::fs::read_to_string(&available).unwrap(),
        "# the operator's previous vhost\n",
        "the replaced vhost is restored"
    );
    assert!(
        std::fs::symlink_metadata(root.join("etc/nginx/sites-enabled/veles-media.conf")).is_err()
    );
    assert!(
        !root.join("srv/veles/media-public").exists(),
        "a directory this run made is removed"
    );
    assert!(!root.join("var/www/veles-media-acme").exists());
    assert_eq!(
        fake.calls.last().unwrap(),
        "systemctl reload nginx.service",
        "nginx is reloaded on the restored configuration: {:?}",
        fake.calls
    );
}

#[test]
fn media_rejects_non_https_and_unconfined_publication_paths() {
    let mut options = match media() {
        HostComponent::Media(options) => options,
        other => panic!("expected media fixture, got {other:?}"),
    };
    options.public_url = "http://media.example".into();
    assert!(HostComponent::Media(options.clone()).preview().is_err());
    options.public_url = "https://media.example".into();
    options.publish_dir = "/etc/nginx".into();
    assert!(HostComponent::Media(options.clone()).preview().is_err());
    options.publish_dir = "/srv/veles".into();
    assert!(HostComponent::Media(options).preview().is_err());
}

fn hub(root: &Path) -> HostComponent {
    let _ = root;
    HostComponent::PoolHub(PoolHubOptions {
        interface: "wg0".into(),
        subnet: "10.0.30.0/24".into(),
        address: "10.0.30.1".into(),
        listen_port: 51820,
        endpoint: "hub.example:51820".into(),
        gateway_user: me(),
        gateway_data_dir: "/var/lib/veles".into(),
    })
}

#[cfg(unix)]
#[test]
fn pool_hub_keeps_its_key_installs_the_reviewed_helper_and_enables_its_units() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("host");
    std::fs::create_dir_all(root.join("var/lib/veles")).unwrap();
    let mut fake = Fake::new(&root);
    let result = apply(&[hub(&root)], &root, &mut fake).unwrap();
    let report = &result["host_components"][0];
    let conf = root.join("etc/wireguard/wg0.conf");
    assert_eq!(
        std::fs::metadata(&conf).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let text = std::fs::read_to_string(&conf).unwrap();
    assert!(text.contains("Address = 10.0.30.1/24") && text.contains("ListenPort = 51820"));
    assert_eq!(
        std::fs::read(root.join("usr/local/sbin/veles-wg-peers")).unwrap(),
        std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../deploy/vps/wg/veles-wg-peers.sh"
        ))
        .unwrap()
    );
    let unit =
        std::fs::read_to_string(root.join("etc/systemd/system/veles-wg-peers.service")).unwrap();
    assert!(unit.contains(
        "ExecStart=/usr/local/sbin/veles-wg-peers wg0 10.0.30.0/24 /var/lib/veles/pool/wg-pending"
    ));
    assert!(root.join("var/lib/veles/pool/wg-pending").is_dir());
    assert!(fake
        .calls
        .contains(&"systemctl enable --now wg-quick@wg0.service".to_string()));
    assert!(fake
        .calls
        .contains(&"systemctl enable --now veles-wg-peers.path".to_string()));
    let key = report["public_key"].as_str().unwrap().to_string();
    assert!(report["gateway_configuration"]
        .as_str()
        .unwrap()
        .contains(&key));
    // A second run never rotates the hub key every member's tunnel names.
    let mut again = Fake::new(&root);
    let result = apply(&[hub(&root)], &root, &mut again).unwrap();
    assert_eq!(result["host_components"][0]["public_key"], key.as_str());
    assert_eq!(std::fs::read_to_string(&conf).unwrap(), text);
}

#[test]
fn pool_hub_failure_disables_what_it_enabled_and_removes_its_new_files() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("host");
    std::fs::create_dir_all(root.join("var/lib/veles")).unwrap();
    let mut fake = Fake::new(&root);
    fake.fail_on = Some("public-key".into());
    let error = apply(&[hub(&root)], &root, &mut fake).unwrap_err();
    assert!(format!("{error:#}").contains("compensated"), "{error:#}");
    assert!(!root.join("etc/wireguard").exists());
    assert!(!root.join("usr/local/sbin/veles-wg-peers").exists());
    assert!(!root
        .join("etc/systemd/system/veles-wg-peers.service")
        .exists());
    assert!(!root.join("var/lib/veles/pool").exists());
    assert!(
        root.join("var/lib/veles").is_dir(),
        "the gateway's own directory stays"
    );
    let tail: Vec<_> = fake.calls.iter().rev().take(3).cloned().collect();
    assert_eq!(
        tail,
        vec![
            "systemctl daemon-reload".to_string(),
            "systemctl disable --now wg-quick@wg0.service".to_string(),
            "systemctl disable --now veles-wg-peers.path".to_string(),
        ]
    );
}

#[test]
fn pool_hub_refuses_a_queue_its_root_helper_could_not_see() {
    let HostComponent::PoolHub(mut options) = hub(Path::new("/")) else {
        unreachable!()
    };
    options.gateway_data_dir = "/home/veles/.local/share/veles".into();
    assert!(HostComponent::PoolHub(options.clone()).preview().is_err());
    options.gateway_data_dir = "/var/lib/veles".into();
    options.subnet = "10.0.30.0/16".into();
    assert!(HostComponent::PoolHub(options).preview().is_err());
}

fn invite_client_config(dir: &Path) -> PathBuf {
    let wg = veles_core::config::PoolWireguard {
        subnet: "10.0.30.0/24".into(),
        address: "10.0.30.1".into(),
        endpoint: "hub.example:51820".into(),
        public_key: veles_core::pool::wg_keypair().unwrap().1,
        ..Default::default()
    };
    let (private, public) = veles_core::pool::wg_keypair().unwrap();
    let peer = veles_core::pool::WgPeer {
        address: "10.0.30.2".parse().unwrap(),
        private_key: private,
        public_key: public,
    };
    let path = dir.join("pool-wg.conf");
    std::fs::write(&path, veles_core::pool::wg_client_config(&wg, &peer)).unwrap();
    path
}

#[cfg(unix)]
#[test]
fn pool_member_installs_the_invite_tunnel_without_showing_its_key() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("host");
    std::fs::create_dir_all(&root).unwrap();
    let client = invite_client_config(temp.path());
    let private = std::fs::read_to_string(&client)
        .unwrap()
        .lines()
        .find_map(|l| l.strip_prefix("PrivateKey = ").map(str::to_string))
        .unwrap();
    let member = HostComponent::PoolMember(PoolMemberOptions {
        interface: "wg0".into(),
        client_config: client.clone(),
    });
    let preview = member.preview().unwrap();
    assert!(
        !preview.to_string().contains(&private),
        "the private key is never previewed"
    );
    assert_eq!(preview["address"], "10.0.30.2/32");
    let mut fake = Fake::new(&root);
    let result = apply(&[member], &root, &mut fake).unwrap();
    assert_eq!(result["host_components"][0]["handshake_observed"], true);
    let conf = root.join("etc/wireguard/wg0.conf");
    assert_eq!(
        std::fs::metadata(&conf).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(std::fs::read_to_string(&conf).unwrap().contains(&private));
    assert!(fake
        .calls
        .contains(&"systemctl enable --now wg-quick@wg0.service".to_string()));
}

#[test]
fn pool_member_refuses_anything_but_the_invite_shape() {
    let temp = tempfile::tempdir().unwrap();
    let client = invite_client_config(temp.path());
    let text = std::fs::read_to_string(&client).unwrap();
    for (from, to) in [
        ("[Peer]\n", "DNS = 1.1.1.1\n[Peer]\n"),
        ("AllowedIPs = 10.0.30.0/24", "AllowedIPs = 0.0.0.0/0"),
        ("/32", "/24"),
    ] {
        std::fs::write(&client, text.replacen(from, to, 1)).unwrap();
        let member = HostComponent::PoolMember(PoolMemberOptions {
            interface: "wg0".into(),
            client_config: client.clone(),
        });
        assert!(member.preview().is_err(), "accepted {to:?}");
    }
}

#[test]
fn host_component_answers_refuse_unknown_fields() {
    let mut value = serde_json::to_value(media()).unwrap();
    value["publish_mode"] = serde_json::json!("0777");
    assert!(serde_json::from_value::<HostComponent>(value).is_err());
}

fn pds() -> HostComponent {
    HostComponent::Pds(Box::new(PdsOptions::new(
        "https://pds.example".into(),
        "pds.example".into(),
        "acme@example".into(),
        PinnedImage::new(
            "ghcr.io/jazware/vlpds:1.2.2",
            &format!("sha256:{}", "a".repeat(64)),
        )
        .unwrap(),
        "https://s3.example".into(),
        "auto".into(),
        "pds-bucket".into(),
        "pds".into(),
        "veles-gateway.service".into(),
        veles_install::current_user_name().unwrap(),
        "veles-net".into(),
        "veles-gateway".into(),
        None,
    )))
}

#[test]
fn pds_plc_wrap_uses_the_pinned_image_and_reports_only_the_public_did() {
    let temp = tempfile::tempdir().unwrap();
    let mut options = pds_options(pds());
    let root = temp.path().join("host");
    options.set_state_root_for_test(root.join("srv/veles/vlpds"));
    std::fs::create_dir_all(root.join("srv/veles/vlpds/secrets")).unwrap();
    std::fs::write(root.join("srv/veles/vlpds/secrets/kek"), "fixture-kek").unwrap();
    let runner = |argv: &[String]| {
        assert!(argv.iter().any(|arg| arg == "runuser"));
        assert!(argv.iter().any(|arg| arg == &options.image.reference()));
        assert!(argv.iter().any(|arg| arg == "--wrap-plc-rotation-key"));
        assert!(argv.iter().any(|arg| arg == "--pull=never"));
        assert!(!argv.iter().any(|arg| arg.contains("fixture-kek")));
        Ok((
            b"vw1.fixture-wrapped-private\n".to_vec(),
            b"PLC rotation key did:key:zFixture wrapped under KEK LFixture\n".to_vec(),
        ))
    };
    let report = options.wrap_plc_key(&root, runner).unwrap();
    assert_eq!(report["plc_did_key"], "did:key:zFixture");
    assert_eq!(report["service_running"], false);
    assert!(!serde_json::to_string(&report)
        .unwrap()
        .contains("wrapped-private"));
    let path = root.join("srv/veles/vlpds/secrets/plc-rotation.key");
    assert_eq!(
        std::fs::read(&path).unwrap(),
        b"vw1.fixture-wrapped-private\n"
    );
    let error = options.wrap_plc_key(&root, runner).unwrap_err();
    assert!(error.to_string().contains("already exists"), "{error:#}");
}

#[test]
fn pds_plc_wrap_rejects_a_symlink_destination() {
    let temp = tempfile::tempdir().unwrap();
    let mut options = pds_options(pds());
    let root = temp.path().join("host");
    options.set_state_root_for_test(root.join("srv/veles/vlpds"));
    std::fs::create_dir_all(root.join("srv/veles/vlpds/secrets")).unwrap();
    std::fs::write(root.join("srv/veles/vlpds/secrets/kek"), "fixture").unwrap();
    std::os::unix::fs::symlink(
        "/tmp/caller-target",
        root.join("srv/veles/vlpds/secrets/plc-rotation.key"),
    )
    .unwrap();
    let runner = |_: &[String]| -> anyhow::Result<(Vec<u8>, Vec<u8>)> {
        panic!("a refused destination must not run a command")
    };
    assert!(options.wrap_plc_key(&root, runner).is_err());
}

#[test]
fn process_secret_runner_captures_stdout_and_uses_empty_stdin() {
    let argv = vec![
        "/bin/sh".into(),
        "-c".into(),
        "test ! -s /dev/stdin && printf 'vw1.private\\n' && printf 'did:key:zPublic\\n' >&2".into(),
    ];
    let (out, err) = veles_install::host_components::ProcessSecretRunner
        .run_secret(&argv)
        .unwrap();
    assert_eq!(out, b"vw1.private\n");
    assert_eq!(err, b"did:key:zPublic\n");
}

#[test]
fn pds_nginx_plan_blocks_operator_surfaces_and_keeps_acme_on_port_80() {
    let options = pds_options(pds());
    let http = options.nginx(false).unwrap();
    assert!(http.contains("listen 80"));
    assert!(http.contains("server_name pds.example *.pds.example"));
    assert!(http.contains("location /.well-known/acme-challenge/"));
    assert!(!http.contains("listen 443"));
    let full = options.nginx(true).unwrap();
    assert!(full.contains("listen 443 ssl http2"));
    assert!(full.contains("location ~ ^/(admin|metrics|internal)(/|$) { return 404; }"));
    assert!(full.contains("location ~ ^/xrpc/vlpds\\.admin\\. { return 404; }"));
    assert!(full.contains("location /xrpc/com.atproto.sync.subscribeRepos"));
    assert!(full.contains("proxy_buffering off"));
    assert!(full.contains("ssl_certificate     /etc/letsencrypt/live/pds.example/fullchain.pem"));
    let records = options.dns_records().unwrap();
    assert_eq!(records.len(), 2);
    assert!(records.iter().all(|record| !record.proxied));
}

#[test]
fn pds_secrets_are_not_marked_for_rollback_when_a_later_phase_fails() {
    let options = pds_options(pds());
    let paths = options.recovery_paths().unwrap();
    assert!(!paths.iter().any(|path| path.ends_with("secrets/kek")));
    assert!(!paths
        .iter()
        .any(|path| path.ends_with("secrets/plc-rotation.key")));
    assert!(!paths
        .iter()
        .any(|path| path.ends_with("secrets/s3-secret-key")));
    assert_eq!(paths.len(), 2);
}

#[test]
fn pds_preview_names_secret_work_without_containing_secret_values() {
    let component = pds();
    let preview = component.preview().unwrap();
    let text = serde_json::to_string(&preview).unwrap();
    assert!(preview["backup_gate_required"].as_bool().unwrap());
    let effects = &preview["host_effects"];
    assert!(effects["files"]
        .as_array()
        .unwrap()
        .iter()
        .any(
            |file| file["path"].as_str().unwrap().ends_with("vlpds.container")
                && file["contents"].as_str().unwrap().contains("Pull=never")
        ));
    assert!(effects["commands"]
        .as_array()
        .unwrap()
        .iter()
        .any(|command| command["argv"]
            .as_array()
            .unwrap()
            .iter()
            .any(|arg| arg == "--wrap-plc-rotation-key")));
    assert!(!preview.to_string().contains("certbot"));
    assert!(preview["secrets"]
        .as_array()
        .unwrap()
        .iter()
        .any(|s| s == "VLPDS_S3_ACCESS_KEY"));
    assert!(preview["secrets"]
        .as_array()
        .unwrap()
        .iter()
        .any(|s| s == "VLPDS_S3_SECRET_KEY"));
    assert!(text.contains("VLPDS_KEK_FILE"));
    assert!(text.contains("VLPDS_PLC_ROTATION_KEY_FILE"));
    assert!(!text.contains("private_key"));
    assert!(!text.contains("private key (hex)"));
    assert!(!text.contains("secret_value"));
    assert!(!text.contains("S3_ACCESS_KEY="));
}

#[test]
fn pds_backup_preview_reviews_tls_and_activation_commands() {
    let mut options = pds_options(pds());
    options.backup_confirmed = true;
    let preview = HostComponent::Pds(Box::new(options)).preview().unwrap();
    let text = preview["host_effects"].to_string();
    assert!(text.contains("certbot"));
    assert!(text.contains("veles-pds.conf"));
    assert!(text.contains("daemon-reload"));
    assert!(text.contains("offline KEK and PLC-key backup acknowledgement"));
}

#[test]
fn pds_quadlet_keeps_secrets_read_only_and_ports_on_loopback() {
    let options = pds_options(pds());
    let text = options.quadlet().unwrap();
    assert!(text.contains("Volume=/srv/veles/vlpds/secrets:/run/vlpds:ro"));
    assert!(text.contains("PublishPort=127.0.0.1:2583:2583"));
    assert!(text.contains("PublishPort=127.0.0.1:9583:9583"));
    assert!(!text.contains("PublishPort=2583:2583"));
    assert!(text.contains("Network=veles-net"));
    assert!(text.contains("Before=veles-gateway.service"));
    assert!(text.contains("VLPDS_PLC_ROTATION_KEY_FILE=/run/vlpds/plc-rotation.key"));
    assert!(!text.contains("VLPDS_S3_SECRET_KEY="));
    assert!(!text.contains("VLPDS_KEK="));
}

#[test]
fn pds_file_apply_preserves_existing_keys_and_does_not_start_a_service() {
    let temp = tempfile::tempdir().unwrap();
    let mut options = pds_options(pds());
    options.set_state_root_for_test(temp.path().join("host/srv/vlpds"));
    std::fs::create_dir_all(temp.path().join("host")).unwrap();
    fixture_home(&temp.path().join("host"), &options);
    let secrets = std::collections::BTreeMap::from([
        (
            "VLPDS_S3_ACCESS_KEY".to_string(),
            "access-fixture".to_string(),
        ),
        (
            "VLPDS_S3_SECRET_KEY".to_string(),
            "secret-fixture".to_string(),
        ),
    ]);
    let report = options
        .apply_files(&temp.path().join("host"), &secrets)
        .unwrap();
    assert_eq!(report["backup_required"], true);
    assert_eq!(report["service_running"], false);
    let secret_dir = options.state_root().join("secrets");
    assert_eq!(
        std::fs::read(secret_dir.join("s3-access-key")).unwrap(),
        b"access-fixture"
    );
    assert_eq!(
        std::fs::read(secret_dir.join("s3-secret-key")).unwrap(),
        b"secret-fixture"
    );
    for name in ["kek", "jwt-secret", "admin-token", "internal-token"] {
        assert_eq!(
            std::fs::read(secret_dir.join(name)).unwrap().len(),
            64,
            "{name}"
        );
    }
    let kek = std::fs::read(secret_dir.join("kek")).unwrap();
    std::fs::write(secret_dir.join("plc-rotation.key"), "existing-wrapped-key").unwrap();
    let second = options
        .apply_files(&temp.path().join("host"), &secrets)
        .unwrap();
    assert_eq!(second["applied"], true);
    assert_eq!(std::fs::read(secret_dir.join("kek")).unwrap(), kek);
    assert_eq!(
        std::fs::read(secret_dir.join("plc-rotation.key")).unwrap(),
        b"existing-wrapped-key"
    );
    // The fixture account is whoever runs the test; its home comes from
    // the password database, not a hard-coded path.
    let host = temp.path().join("host");
    let paths = options.recovery_paths().unwrap();
    let unit = host.join(paths[0].strip_prefix("/").unwrap());
    assert!(std::fs::read_to_string(unit)
        .unwrap()
        .contains("Pull=never"));
    let ordering = host.join(paths[1].strip_prefix("/").unwrap());
    let ordering = std::fs::read_to_string(ordering).unwrap();
    assert!(ordering.contains("After=vlpds.service"));
    assert!(ordering.contains("Wants=vlpds.service"));
    assert!(!ordering.contains("[Service]"));
}

#[test]
fn pds_answers_cannot_select_an_arbitrary_state_root() {
    let options = pds_options(pds());
    let mut value = serde_json::to_value(HostComponent::Pds(Box::new(options))).unwrap();
    value["state_root"] = serde_json::json!("/tmp/caller-selected");
    // Refused, not silently ignored: a request naming a state root must
    // not look as if it had been honoured.
    let error = serde_json::from_value::<HostComponent>(value).unwrap_err();
    assert!(error.to_string().contains("state_root"), "{error}");
    let decoded =
        pds_options(serde_json::from_value(serde_json::to_value(pds()).unwrap()).unwrap());
    assert_eq!(
        decoded.state_root(),
        std::path::Path::new("/srv/veles/vlpds")
    );
}

#[test]
fn pds_file_apply_refuses_a_state_root_outside_its_explicit_host_root() {
    let temp = tempfile::tempdir().unwrap();
    let options = pds_options(pds());
    let secrets = std::collections::BTreeMap::from([
        ("VLPDS_S3_ACCESS_KEY".to_string(), "fixture".to_string()),
        ("VLPDS_S3_SECRET_KEY".to_string(), "fixture".to_string()),
    ]);
    let error = options.apply_files(temp.path(), &secrets).unwrap_err();
    assert!(error.to_string().contains("inside"), "{error:#}");
    assert!(!temp.path().join("srv").exists());
}

#[test]
fn pds_file_failure_preserves_new_secrets_and_recovers_only_owned_files() {
    let temp = tempfile::tempdir().unwrap();
    let mut options = pds_options(pds());
    let root = temp.path().join("host");
    options.set_state_root_for_test(root.join("srv/veles/vlpds"));
    let account_home = options.recovery_paths().unwrap()[0]
        .ancestors()
        .nth(4)
        .unwrap()
        .to_path_buf();
    let home = root.join(account_home.strip_prefix("/").unwrap());
    let ordering = home.join(".config/systemd/user/veles-gateway.service.d/50-vlpds.conf");
    std::fs::create_dir_all(ordering.parent().unwrap()).unwrap();
    std::fs::write(&ordering, "operator-owned\n").unwrap();
    let secrets = std::collections::BTreeMap::from([
        ("VLPDS_S3_ACCESS_KEY".to_string(), "fixture".to_string()),
        ("VLPDS_S3_SECRET_KEY".to_string(), "fixture".to_string()),
    ]);
    let error = options.apply_files(&root, &secrets).unwrap_err();
    assert!(format!("{error:#}").contains("differs"), "{error:#}");
    assert!(root.join("srv/veles/vlpds/secrets/kek").is_file());
    assert_eq!(
        std::fs::read_to_string(ordering).unwrap(),
        "operator-owned\n"
    );
    assert!(!home
        .join(".config/containers/systemd/vlpds.container")
        .exists());
}

#[cfg(unix)]
#[test]
fn pds_file_apply_refuses_a_symlinked_secret_parent() {
    let temp = tempfile::tempdir().unwrap();
    let mut options = pds_options(pds());
    let root = temp.path().join("host");
    options.set_state_root_for_test(root.join("srv/veles/vlpds"));
    let outside = temp.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::create_dir_all(root.join("srv/veles")).unwrap();
    std::os::unix::fs::symlink(&outside, root.join("srv/veles/vlpds")).unwrap();
    let secrets = std::collections::BTreeMap::from([
        ("VLPDS_S3_ACCESS_KEY".to_string(), "fixture".to_string()),
        ("VLPDS_S3_SECRET_KEY".to_string(), "fixture".to_string()),
    ]);
    assert!(options.apply_files(&root, &secrets).is_err());
    assert!(!outside.join("secrets/kek").exists());
}

#[test]
fn pds_requires_https_public_urls_and_an_https_s3_endpoint() {
    let mut options = pds_options(pds());
    options.public_url = "http://pds.example".into();
    assert!(HostComponent::Pds(Box::new(options.clone()))
        .preview()
        .is_err());
    options.public_url = "https://pds.example".into();
    options.s3_endpoint = "http://s3.example".into();
    assert!(HostComponent::Pds(Box::new(options)).preview().is_err());
}

/// The gateway account's home inside a fixture host root: on a real host
/// it exists before a PDS is prepared.
fn fixture_home(root: &Path, options: &PdsOptions) {
    let quadlet = &options.recovery_paths().unwrap()[0];
    let home = quadlet.ancestors().nth(4).unwrap();
    std::fs::create_dir_all(root.join(home.strip_prefix("/").unwrap())).unwrap();
}

#[test]
fn pds_plc_wrap_accepts_the_real_host_root() {
    // The CLI handoff applies with root "/": the wrap must accept it
    // (found on a real Debian host, where it refused "/").
    let temp = tempfile::tempdir().unwrap();
    let mut options = pds_options(pds());
    let state = temp.path().join("srv/veles/vlpds");
    options.set_state_root_for_test(state.clone());
    std::fs::create_dir_all(state.join("secrets")).unwrap();
    std::fs::write(state.join("secrets/kek"), "fixture-kek").unwrap();
    let runner = |_: &[String]| Ok((b"vw1.fixture\n".to_vec(), b"did:key:zFixture\n".to_vec()));
    let report = options.wrap_plc_key(Path::new("/"), runner).unwrap();
    assert_eq!(report["plc_did_key"], "did:key:zFixture");
}
