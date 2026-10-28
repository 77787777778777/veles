//! Commented answers files, so nobody hand-writes installer JSON.
//!
//! `veles install profile --template <profile>` and `veles install
//! host-component --template <component>` print one of these. Each is TOML
//! (comments survive editing) and parses as-is into the same typed
//! answers the CLI, the API and the Tk wizard use; the example values are
//! placeholders the operator replaces. The tests in this module parse every
//! template, so a renamed field cannot leave a template that no longer
//! loads.
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub const PROFILES: &[&str] = &["desktop", "server", "container-host", "nixos"];
pub const COMPONENTS: &[&str] = &["pds", "media", "pool-hub", "pool-member"];

/// The answers file of `veles install host-component`: a list of typed
/// host components, nothing else.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostComponentAnswers {
    pub component: Vec<crate::host_components::HostComponent>,
}

/// Parse an answers file: TOML (the templates), or JSON when it starts
/// with `{` (the API's shape). Unknown keys are refused in both.
pub fn parse_answers<T: serde::de::DeserializeOwned>(text: &str) -> Result<T> {
    if text.trim_start().starts_with('{') {
        serde_json::from_str(text).context("the answers file is not valid answers JSON")
    } else {
        toml::from_str(text).context("the answers file is not valid answers TOML")
    }
}

const SETUP: &str = r#"
[setup]
# The model provider: "openai_compatible" (any OpenAI-style endpoint),
# "ali_token_plan" (the default) or "skip".
provider = "openai_compatible"
base_url = "https://api.example.com/v1"
# The NAME of the environment variable that holds the API key. Its value
# is asked for (hidden) when the plan is applied and goes to .env, never
# to config.toml.
key_env = "OPENAI_API_KEY"
model = "example-model"
# Sections to leave out of a fresh config (or untouched in an existing
# one): any of "provider", "model", "telegram", "irc", "acl", "locale".
skipped = ["telegram", "irc", "acl", "locale"]
"#;

pub fn profile_template(name: &str) -> Result<String> {
    let head = match name {
        "desktop" => r#"# veles install profile: desktop
# The core, a user unit and your own config; no root action at all.
#
#   veles install profile --answers desktop.toml --core <unpacked bundle dir>         # preview
#   veles install profile --answers desktop.toml --core <unpacked bundle dir> --yes   # apply
#   systemctl --user enable --now veles-gateway                                       # you start it
profile = "desktop"
"#
        .to_string(),
        "server" => r#"# veles install profile: server
# A system service for one existing account: VELES_HOME=/var/lib/veles,
# /etc/systemd/system/veles-gateway.service. Prepared as you, applied as
# root by an explicit handoff, never enabled or started by the installer.
#
#   sudo useradd --system --create-home --shell /usr/sbin/nologin veles   # if the account is new
#   veles install profile --answers server.toml --core <unpacked bundle dir>         # preview
#   veles install profile --answers server.toml --core <unpacked bundle dir> --yes   # stage the reviewed request
#   sudo <bundle dir>/bin/veles install profile-handoff --request <printed path> --yes
#   sudo systemctl enable --now veles-gateway
#   sudo -u veles env VELES_HOME=/var/lib/veles /var/lib/veles/install/current/bin/veles doctor
profile = "server"

[host]
# The existing account the gateway runs as.
user = "veles"
# group = "veles"        # optional; the account's primary group otherwise
scope = "system"
"#
        .to_string(),
        "container-host" => {
            return Ok(r#"# veles install profile: container-host
# The confined container layout: rootless gateway and sandbox accounts on
# one storage root, their podman sockets, a shared network and the gateway
# quadlet, from ONE reviewed root script.
#
#   veles install profile --answers container-host.toml          # preview (prints the script)
#   veles install profile --answers container-host.toml --yes    # stage the reviewed request
#   sudo veles install profile-handoff --request <printed path> --yes
#
# The script checks, and does not install, podman, uidmap, passt or
# slirp4netns and dbus-user-session unless packages.install_missing is true.
profile = "container-host"

[host.container]
unit_prefix = "veles"
# The network rootless containers share.
network = "veles-net"
extra_users = []

[host.container.gateway]
name = "veles"
home = "/srv/veles/veles"
rootless = true
system = true

[host.container.sandbox]
name = "veles-sbx"
home = "/srv/veles/veles-sbx"
rootless = true
system = true

[host.container.storage]
root = "/srv/veles"
bind = false
mode = 0o755
home_mode = 0o700

[host.container.socket]
dir = "/run/veles-sbx"
user = "veles-sbx"
group = "veles"
dir_mode = 0o750
socket_mode = 0o666

[host.container.packages]
manager = "apt"          # "apt", "dnf" or "pacman"
install_missing = false

# The gateway image: a release image, pinned by its digest
# (`skopeo inspect docker://<image>` prints it). Moving tags are refused.
[[host.container.images]]
user = "veles"
image = { source = "ghcr.io/example/veles-agent:0.1.0", digest = "sha256:0000000000000000000000000000000000000000000000000000000000000000" }

[[host.container.quadlets]]
user = "veles"
name = "veles-gateway"

[host.container.quadlets.container]
description = "veles gateway"
image = { source = "ghcr.io/example/veles-agent:0.1.0", digest = "sha256:0000000000000000000000000000000000000000000000000000000000000000" }
# Extra quadlet keys. Image, Pull, Exec, User and Network are owned by the
# plan and refused here. The image runs as uid 1000 with VELES_HOME=/opt/data:
# keep-id maps the gateway account onto it, and the named volume keeps the
# gateway's state across image updates.
extra = [
  "UserNS=keep-id:uid=1000,gid=1000",
  "Volume=veles-data:/opt/data:U",
  "Environment=VELES_DESKTOP_HOST=0.0.0.0",
  "PublishPort=127.0.0.1:8642:8642",
  "HealthCmd=curl -sf http://127.0.0.1:8642/api/health",
]
"#
            .to_string());
        }
        "nixos" => {
            return Ok(r#"# veles install profile: nixos
# Prints a services.veles snippet for your configuration.nix. Nothing is
# applied and /etc/nixos is never edited.
#
#   veles install profile --answers nixos.toml --yes
profile = "nixos"

[host]
user = "veles"
"#
            .to_string());
        }
        other => anyhow::bail!(
            "no profile template {other:?}; choose one of: {}",
            PROFILES.join(", ")
        ),
    };
    Ok(format!("{head}{SETUP}"))
}

pub fn component_template(name: &str) -> Result<String> {
    let body = match name {
        "media" => {
            r#"# veles install host-component: local media
# An nginx vhost that serves the gateway's IRC media directory over HTTPS
# (HTTP-01 via certbot), verified by fetching a probe file back. DNS is
# never changed: point an A/AAAA record for the host at this machine first.
#
#   veles install host-component --answers media.toml          # preview
#   veles install host-component --answers media.toml --yes    # stage
#   sudo veles install profile-handoff --request <printed path> --yes
[[component]]
kind = "media"
public_url = "https://media.example.org"
publish_dir = "/srv/veles/media"
gateway_user = "veles"
acme_email = "you@example.org"
# max_age_days = 30     # printed into the gateway configuration snippet
"#
        }
        "pds" => {
            r#"# veles install host-component: own PDS (vlpds)
# First run (backup_confirmed = false): secrets, the pinned image, the PLC
# key and the quadlet, nothing started. Back up the KEK and the PLC key
# offline, then run again with backup_confirmed = true for HTTP-01 TLS,
# activation and a public health check. The S3 credentials are asked for
# (hidden) by the handoff.
#
#   veles install host-component --answers pds.toml --yes
#   sudo veles install profile-handoff --request <printed path> --yes
[[component]]
kind = "pds"
public_url = "https://pds.example.org"
handle_domain = "pds.example.org"
acme_email = "you@example.org"
backup_confirmed = false
image = { source = "ghcr.io/jazware/vlpds:1.2.2", digest = "sha256:0000000000000000000000000000000000000000000000000000000000000000" }
s3_endpoint = "https://s3.example.org"
s3_region = "auto"
s3_bucket = "my-pds-bucket"
s3_prefix = "pds"
# The gateway's rootless account, its user unit and its container network.
gateway_user = "veles"
gateway_unit = "veles-gateway.service"
gateway_quadlet = "veles-gateway"
network = "veles-net"
# recovery_did_key = "did:key:..."
"#
        }
        "pool-hub" => {
            r#"# veles install host-component: pool hub (WireGuard)
# The arbiter's tunnel: /etc/wireguard/<interface>.conf with a new key pair
# (an existing one is kept, never rotated), wg-quick, and the root helper
# that applies the peers `veles pool invite` queues. Run it on the node
# that is (or will be) the pool's arbiter, after its server profile.
#
#   veles install host-component --answers pool-hub.toml --yes
#   sudo veles install profile-handoff --request <printed path> --yes
[[component]]
kind = "pool_hub"
interface = "wg0"
subnet = "10.0.30.0/24"
address = "10.0.30.1"
listen_port = 51820
# Where the other nodes reach this host (name or address : port).
endpoint = "hub.example.org:51820"
gateway_user = "veles"
gateway_data_dir = "/var/lib/veles"
"#
        }
        "pool-member" => {
            r#"# veles install host-component: pool member (WireGuard)
# On the arbiter, `veles pool invite <name> --wireguard-file <file>` writes
# this node's WireGuard config (0600). Copy it here and name it below. It
# holds this node's private key; delete it once applied.
#
#   veles install host-component --answers pool-member.toml --yes
#   sudo veles install profile-handoff --request <printed path> --yes
#   veles pool join '<the veles invite text>'     # as the gateway's account
[[component]]
kind = "pool_member"
interface = "wg0"
client_config = "/home/you/pool-wg.conf"
"#
        }
        other => anyhow::bail!(
            "no host-component template {other:?}; choose one of: {}",
            COMPONENTS.join(", ")
        ),
    };
    Ok(body.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_profile_template_parses_into_typed_answers() {
        for name in PROFILES {
            let text = profile_template(name).unwrap();
            let answers: crate::ProfileAnswers = parse_answers(&text)
                .unwrap_or_else(|e| panic!("{name} template does not parse: {e:#}"));
            assert_eq!(answers.profile.as_str(), *name);
        }
        let container: crate::ProfileAnswers =
            parse_answers(&profile_template("container-host").unwrap()).unwrap();
        // Container-host options name Linux paths.
        #[cfg(unix)]
        container.host.container.unwrap().validate().unwrap();
        #[cfg(not(unix))]
        let _ = container;
    }

    #[test]
    fn every_component_template_parses_and_names_its_kind() {
        for name in COMPONENTS {
            let text = component_template(name).unwrap();
            let answers: HostComponentAnswers = parse_answers(&text)
                .unwrap_or_else(|e| panic!("{name} template does not parse: {e:#}"));
            assert_eq!(answers.component.len(), 1);
            let expected = match *name {
                "pds" => "own-pds",
                "media" => "local-media",
                other => other,
            };
            assert_eq!(answers.component[0].name(), expected);
        }
    }

    #[test]
    fn a_misspelt_answer_is_refused_not_ignored() {
        let text = profile_template("server").unwrap().replace(
            "scope = \"system\"",
            "scope = \"system\"\ntarget_rot = \"/x\"",
        );
        assert!(parse_answers::<crate::ProfileAnswers>(&text).is_err());
        let text = component_template("media")
            .unwrap()
            .replace("gateway_user", "gateway_usr");
        assert!(parse_answers::<HostComponentAnswers>(&text).is_err());
    }
}
