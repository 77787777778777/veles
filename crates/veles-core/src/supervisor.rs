//! Who brings this gateway back when it exits (0.2.2, the command
//! center's Restart, owner ask 287976: "deploy/redeploy/manage the
//! nodes themselves").
//!
//! A node restarts ITSELF: it exits with [`RESTART_EXIT_CODE`] and the
//! program that supervises it starts it again. Every supervisor veles
//! ships under restarts a gateway that exits with a code other than 0
//! (and, on the phone, other than the idle stop's 75):
//!
//! | supervisor                    | how it is known                         | restarts on            |
//! |-------------------------------|-----------------------------------------|------------------------|
//! | a systemd unit                | `INVOCATION_ID`, the unit in the cgroup | `Restart=on-failure` or `always`, read from systemd |
//! | the VPS container (quadlet)   | its unit declares `VELES_SUPERVISOR=container` | `Restart=always` on the quadlet |
//! | `veles-gw` (the Windows logon task) | the launcher sets `VELES_SUPERVISOR=veles-gw` | any non-zero exit, 3 s later |
//! | Android's GatewayService      | the Android build                       | any exit but 75, 1 s later |
//!
//! Anything else is "no supervisor known", and a node that knows of
//! none SAYS so and stays up: a gateway started by hand, by the desktop,
//! or in a container whose restart policy it cannot see would not come
//! back. A systemd unit is asked for its `Restart=` rather than assumed,
//! for the same reason.

use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

/// The exit code of a restart the gateway asked for. Not 0 (a clean
/// stop ends `veles-gw` and is not restarted by `Restart=on-failure`)
/// and not 75 (the phone's idle stop, which GatewayService leaves
/// stopped).
pub const RESTART_EXIT_CODE: i32 = 76;

/// The environment variable a supervisor that cannot be detected
/// declares itself with: `veles-gw`, `container`, `android`, or `none`.
pub const ENV: &str = "VELES_SUPERVISOR";

/// What restarts this process when it exits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Supervisor {
    /// A systemd unit whose `Restart=` covers a failed exit.
    Systemd { unit: String, user: bool },
    /// A container whose unit restarts it (declared).
    Container,
    /// The `veles-gw` launcher (the Windows logon task).
    Launcher,
    /// Android's GatewayService.
    AndroidService,
    /// Nothing known restarts it: why.
    None(String),
}

/// What [`classify`] decides on, gathered by [`detect`].
#[derive(Debug, Clone, Default)]
pub struct Facts {
    /// `VELES_SUPERVISOR`, when set.
    pub declared: Option<String>,
    /// The Android build.
    pub android: bool,
    /// `INVOCATION_ID` is set: systemd started this process.
    pub invocation: bool,
    /// `/proc/self/cgroup`.
    pub cgroup: Option<String>,
    /// `/run/.containerenv` or `/.dockerenv` exists.
    pub container: bool,
    /// This process's id.
    pub pid: u32,
    /// The parent process's name, where it can be read (`/proc`).
    pub parent: Option<String>,
}

/// The systemd unit a cgroup file names, and whether it runs under a
/// user manager (`…/user@1000.service/app.slice/veles-gateway.service`).
pub fn unit_of(cgroup: &str) -> Option<(String, bool)> {
    let line = cgroup
        .lines()
        .find(|l| l.starts_with("0::"))
        .or_else(|| cgroup.lines().find(|l| l.contains(".service")))?;
    let path = line.rsplit(':').next()?;
    let unit = path
        .split('/')
        .rev()
        .find(|seg| seg.ends_with(".service"))?;
    // The user manager itself is `user@N.service`: a unit UNDER it is
    // the one that runs us; that segment alone is not.
    if unit.starts_with("user@") {
        return None;
    }
    Some((unit.to_string(), path.contains("/user@")))
}

/// Decide from `facts`; `unit_state(unit, user)` reads a unit's
/// `Restart=` and `MainPID` (`systemctl show`).
pub fn classify(
    facts: &Facts,
    unit_state: impl Fn(&str, bool) -> Option<(String, u32)>,
) -> Supervisor {
    if let Some(d) = facts
        .declared
        .as_deref()
        .map(str::trim)
        .filter(|d| !d.is_empty())
    {
        return match d {
            // A variable children inherit: a gateway started by hand from
            // a shell the supervised one spawned would read it too, so the
            // launcher must be the PARENT where that can be seen.
            "veles-gw" | "launcher" => match facts.parent.as_deref() {
                Some(p) if !p.starts_with("veles-gw") => Supervisor::None(format!(
                    "{ENV}={d} is inherited: this gateway's parent is {p}, not veles-gw"
                )),
                _ => Supervisor::Launcher,
            },
            "container" => Supervisor::Container,
            "android" => Supervisor::AndroidService,
            "none" => Supervisor::None(format!("{ENV}=none: this node is not to restart itself")),
            other => Supervisor::None(format!(
                "{ENV}={other:?} names no supervisor veles knows (veles-gw, container, android)"
            )),
        };
    }
    if facts.android {
        return Supervisor::AndroidService;
    }
    if facts.invocation {
        let Some((unit, user)) = facts.cgroup.as_deref().and_then(unit_of) else {
            return Supervisor::None(
                "started by systemd, but its unit could not be read from /proc/self/cgroup".into(),
            );
        };
        // Restart= acts when the unit's MAIN process exits: a gateway
        // started from a shell inside some unit (a terminal's) is not it.
        return match unit_state(&unit, user) {
            Some((_, main)) if main != facts.pid => Supervisor::None(format!(
                "the gateway runs inside systemd unit {unit} but is not its main process \
                 (started by hand from within it): its exit would not restart it"
            )),
            Some((p, _)) if matches!(p.trim(), "always" | "on-failure") => {
                Supervisor::Systemd { unit, user }
            }
            Some((p, _)) => Supervisor::None(format!(
                "systemd unit {unit} has Restart={p}: it would not start the gateway again"
            )),
            None => Supervisor::None(format!(
                "systemd unit {unit}: its Restart= policy could not be read"
            )),
        };
    }
    if facts.container {
        return Supervisor::None(format!(
            "runs in a container whose restart policy it cannot see; a unit that restarts \
             the container declares {ENV}=container"
        ));
    }
    Supervisor::None("no supervisor: the gateway was started by hand or by the desktop".into())
}

fn systemd_unit_state(unit: &str, user: bool) -> Option<(String, u32)> {
    let mut cmd = std::process::Command::new("systemctl");
    if user {
        cmd.arg("--user");
    }
    let out = cmd
        .args(["show", "-p", "Restart", "-p", "MainPID", unit])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let field = |name: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(name).map(str::to_string))
    };
    Some((field("Restart=")?, field("MainPID=")?.trim().parse().ok()?))
}

fn parent_name() -> Option<String> {
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    // `pid (comm) state ppid …`: the name may hold spaces and parens.
    let ppid = stat.rsplit_once(')')?.1.split_whitespace().nth(1)?;
    std::fs::read_to_string(format!("/proc/{ppid}/comm"))
        .ok()
        .map(|s| s.trim().to_string())
}

/// What supervises THIS process, read now.
pub fn detect() -> Supervisor {
    let facts = Facts {
        declared: std::env::var(ENV).ok(),
        android: cfg!(target_os = "android"),
        invocation: std::env::var_os("INVOCATION_ID").is_some_and(|v| !v.is_empty()),
        cgroup: std::fs::read_to_string("/proc/self/cgroup").ok(),
        container: std::path::Path::new("/run/.containerenv").exists()
            || std::path::Path::new("/.dockerenv").exists(),
        pid: std::process::id(),
        parent: parent_name(),
    };
    classify(&facts, systemd_unit_state)
}

impl Supervisor {
    pub fn can_restart(&self) -> bool {
        !matches!(self, Supervisor::None(_))
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Supervisor::Systemd { .. } => "systemd",
            Supervisor::Container => "container",
            Supervisor::Launcher => "veles-gw",
            Supervisor::AndroidService => "android",
            Supervisor::None(_) => "none",
        }
    }

    /// One sentence for the owner.
    pub fn detail(&self) -> String {
        match self {
            Supervisor::Systemd { unit, user } => format!(
                "systemd {} unit {unit} starts it again",
                if *user { "user" } else { "system" }
            ),
            Supervisor::Container => "its container's unit starts it again".into(),
            Supervisor::Launcher => "veles-gw (its launcher) starts it again".into(),
            Supervisor::AndroidService => "the app's gateway service starts it again".into(),
            Supervisor::None(why) => why.clone(),
        }
    }

    pub fn json(&self) -> Value {
        json!({"kind": self.kind(), "can_restart": self.can_restart(), "detail": self.detail()})
    }
}

static RESTART: AtomicBool = AtomicBool::new(false);

/// Stop the gateway the way a stop request does, and exit with
/// [`RESTART_EXIT_CODE`] once it has stopped (`veles gateway run`).
pub fn request_restart() {
    RESTART.store(true, Ordering::SeqCst);
    crate::lifecycle::request_shutdown();
}

/// Whether [`request_restart`] was called in this process.
pub fn restart_requested() -> bool {
    RESTART.load(Ordering::SeqCst)
}

/// This process's boot: a random id made once. A restart changes it,
/// which is how an asker sees the node come back, and how a node tells
/// a restart request meant for its previous boot (a replay) apart.
pub fn boot_id() -> &'static str {
    static BOOT: OnceLock<String> = OnceLock::new();
    BOOT.get_or_init(|| uuid::Uuid::new_v4().simple().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A unit whose main process is this one (pid 42) with policy `p`.
    fn policy(p: Option<&'static str>) -> impl Fn(&str, bool) -> Option<(String, u32)> {
        move |_, _| p.map(|p| (p.to_string(), 42))
    }

    #[test]
    fn a_systemd_unit_restarts_only_with_a_restart_policy_that_covers_it() {
        let user = Facts {
            invocation: true,
            cgroup: Some(
                "0::/user.slice/user-1000.slice/user@1000.service/app.slice/veles-gateway.service\n"
                    .into(),
            ),
            pid: 42,
            ..Facts::default()
        };
        assert_eq!(
            classify(&user, policy(Some("on-failure"))),
            Supervisor::Systemd {
                unit: "veles-gateway.service".into(),
                user: true
            }
        );
        assert!(classify(&user, policy(Some("always"))).can_restart());
        let no = classify(&user, policy(Some("no")));
        assert!(!no.can_restart());
        assert!(no.detail().contains("Restart=no"), "{}", no.detail());
        assert!(!classify(&user, policy(None)).can_restart());
        let system = Facts {
            invocation: true,
            cgroup: Some("0::/system.slice/veles.service\n".into()),
            pid: 42,
            ..Facts::default()
        };
        assert_eq!(
            classify(&system, policy(Some("always"))),
            Supervisor::Systemd {
                unit: "veles.service".into(),
                user: false
            }
        );
        // A shell in a user session is not a unit that restarts us.
        let shell = Facts {
            invocation: true,
            cgroup: Some("0::/user.slice/user-1000.slice/user@1000.service\n".into()),
            ..Facts::default()
        };
        assert!(!classify(&shell, policy(Some("always"))).can_restart());
        // Started by hand from a terminal that systemd runs: the unit is
        // the terminal's, whose main process is not this gateway — found
        // live, the scratch gateways inherited a terminal's INVOCATION_ID.
        let terminal = Facts {
            pid: 7,
            ..user.clone()
        };
        let t = classify(&terminal, policy(Some("always")));
        assert!(!t.can_restart());
        assert!(
            t.detail().contains("not its main process"),
            "{}",
            t.detail()
        );
    }

    #[test]
    fn a_declared_supervisor_wins_and_nothing_known_says_why() {
        let declared = |d: &str| Facts {
            declared: Some(d.into()),
            container: true,
            ..Facts::default()
        };
        assert_eq!(
            classify(&declared("container"), policy(None)),
            Supervisor::Container
        );
        assert_eq!(
            classify(&declared("veles-gw"), policy(None)),
            Supervisor::Launcher
        );
        let launched = Facts {
            parent: Some("veles-gw".into()),
            ..declared("veles-gw")
        };
        assert_eq!(classify(&launched, policy(None)), Supervisor::Launcher);
        // Inherited by a gateway started from a shell the supervised one
        // spawned: its parent is the shell.
        let inherited = Facts {
            parent: Some("bash".into()),
            ..declared("veles-gw")
        };
        let i = classify(&inherited, policy(None));
        assert!(!i.can_restart());
        assert!(i.detail().contains("inherited"), "{}", i.detail());
        assert_eq!(
            classify(&declared("android"), policy(None)),
            Supervisor::AndroidService
        );
        assert!(!classify(&declared("none"), policy(None)).can_restart());
        assert!(!classify(&declared("runit"), policy(None)).can_restart());
        let android = Facts {
            android: true,
            ..Facts::default()
        };
        assert_eq!(classify(&android, policy(None)), Supervisor::AndroidService);
        // A container that did not declare its unit restarts it.
        let container = Facts {
            container: true,
            ..Facts::default()
        };
        let c = classify(&container, policy(Some("always")));
        assert!(!c.can_restart());
        assert!(c.detail().contains(ENV), "{}", c.detail());
        let bare = classify(&Facts::default(), policy(Some("always")));
        assert!(!bare.can_restart());
        assert_eq!(bare.json()["kind"], "none");
        assert_eq!(bare.json()["can_restart"], false);
    }

    #[test]
    fn the_restart_code_is_neither_a_clean_stop_nor_the_idle_stop() {
        assert_ne!(RESTART_EXIT_CODE, 0);
        assert_ne!(RESTART_EXIT_CODE, crate::idle::EXIT_CODE);
        assert_eq!(boot_id(), boot_id());
        assert_eq!(boot_id().len(), 32);
    }

    /// `veles-gw` links nothing of core, so it keeps its own copy of
    /// the code, and tells a requested restart from a death by it.
    #[test]
    fn the_launcher_knows_the_restart_code() {
        let launcher = include_str!("../../veles-launcher/src/main.rs");
        let line = format!("const RESTART_EXIT_CODE: i32 = {RESTART_EXIT_CODE};");
        assert!(launcher.contains(&line), "veles-gw lacks `{line}`");
    }
}
