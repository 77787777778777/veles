//! Real remote-podman lifecycle proof, deliberately outside the unit suite.
//!
//! Run explicitly with a cached image and the opt-in flag:
//!
//! ```sh
//! VELES_USER_ENV_LIFECYCLE_TEST=1 \
//! VELES_LIFECYCLE_TEST_IMAGE=veles-sandbox:latest \
//! cargo test -p veles-core --test user_env_lifecycle_live -- --ignored --nocapture
//! ```
//!
//! A private podman service gets its own root, runroot and Unix socket.
//! Nothing here uses the developer's normal containers, volumes, or home.

#![cfg(target_os = "linux")]

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const NODE_LABEL: &str = "online.7778777.veles.user-env-node";
const HOME_LABEL: &str = "online.7778777.veles.user-env-home";
const GENERATION_LABEL: &str = "online.7778777.veles.user-env-incarnation";

struct Fixture {
    _home: tempfile::TempDir,
    root: PathBuf,
    socket: PathBuf,
    service: Child,
    saved: Vec<(&'static str, Option<String>)>,
}

impl Fixture {
    fn start(image: &str) -> Self {
        let home = tempfile::tempdir().expect("scratch home");
        let root = home.path().to_path_buf();
        let store = root.join("fixture/store");
        let run = root.join("fixture/run");
        std::fs::create_dir_all(&store).unwrap();
        std::fs::create_dir_all(&run).unwrap();
        let socket = root.join("fixture/podman.sock");
        let uri = format!("unix://{}", socket.display());
        let service = Command::new("podman")
            .arg("--root")
            .arg(&store)
            .arg("--runroot")
            .arg(&run)
            .args(["system", "service", "--time=0", &uri])
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("start disposable podman service");
        let saved = ["VELES_HOME", "CONTAINER_HOST", "VELES_PODMAN_BIN"]
            .into_iter()
            .map(|key| (key, std::env::var(key).ok()))
            .collect();
        std::env::set_var("VELES_HOME", &root);
        std::env::set_var("CONTAINER_HOST", &uri);
        std::env::set_var("VELES_PODMAN_BIN", "podman");
        let mut fixture = Self {
            _home: home,
            root,
            socket,
            service,
            saved,
        };
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(status) = fixture.service.try_wait().expect("service status") {
                panic!("disposable podman service exited during startup: {status}");
            }
            if fixture.remote_status(&["info"]) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "private podman socket did not start"
            );
            std::thread::sleep(Duration::from_millis(100));
        }
        fixture.load_cached_image(image);
        fixture
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new("podman");
        command
            .env(
                "CONTAINER_HOST",
                format!("unix://{}", self.socket.display()),
            )
            .arg("--url")
            .arg(format!("unix://{}", self.socket.display()))
            .args(args);
        command
    }

    fn remote(&self, args: &[&str]) -> String {
        let output = self
            .command(args)
            .output()
            .unwrap_or_else(|error| panic!("remote podman {args:?}: {error}"));
        assert!(
            output.status.success(),
            "remote podman {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn remote_status(&self, args: &[&str]) -> bool {
        self.command(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    fn load_cached_image(&self, image: &str) {
        // This is a LOCAL read of the cached image. The fixture's
        // CONTAINER_HOST deliberately points every normal podman call at
        // the private service, which is empty until this load completes.
        let local = |args: &[&str]| {
            let mut command = Command::new("podman");
            command
                .arg("--remote=false")
                .args(args)
                .env_remove("CONTAINER_HOST");
            command
        };
        assert!(
            local(&["image", "exists", image])
                .status()
                .is_ok_and(|status| status.success()),
            "cached image {image} is required; set VELES_LIFECYCLE_TEST_IMAGE"
        );
        // Stream the host's cached image into the private service. No
        // image is built and the host's normal store is not modified.
        let mut save = local(&["save", "--format", "docker-archive", image])
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("export cached image");
        let mut load = self.command(&["load"]);
        load.stdin(Stdio::from(save.stdout.take().expect("image stream")));
        let loaded = load.output().expect("load into private service");
        let saved = save.wait().expect("finish image export");
        assert!(
            saved.success() && loaded.status.success(),
            "private image load failed: {saved}; {}",
            String::from_utf8_lossy(&loaded.stderr)
        );
        self.remote(&["tag", image, "veles-sandbox:latest"]);
    }

    fn config(&self) {
        std::fs::write(
            self.root.join("config.toml"),
            "[sandbox]\nbackend = \"podman\"\nimage = \"veles-sandbox:latest\"\nnetwork = false\n",
        )
        .unwrap();
        veles_core::config::invalidate_live();
    }

    fn recreate_home(&self) {
        for entry in std::fs::read_dir(&self.root).unwrap() {
            let path = entry.unwrap().path();
            let keep = path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| matches!(name, "node.toml" | "config.toml" | "fixture"));
            if keep {
                continue;
            }
            if path.is_dir() {
                std::fs::remove_dir_all(path).unwrap();
            } else {
                std::fs::remove_file(path).unwrap();
            }
        }
        self.config();
    }

    /// Only conmons whose command line names this fixture's runroot.
    /// Never select by container prefix: the developer's normal podman
    /// may have containers with the same names.
    fn conmon_pids(&self) -> Vec<libc::pid_t> {
        let runroot = self.root.join("fixture/run");
        let needle = runroot.to_string_lossy().into_owned();
        let mut found = Vec::new();
        for entry in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<libc::pid_t>().ok())
            else {
                continue;
            };
            let Ok(cmdline) = std::fs::read(entry.path().join("cmdline")) else {
                continue;
            };
            let args: Vec<_> = cmdline
                .split(|byte| *byte == 0)
                .filter_map(|arg| std::str::from_utf8(arg).ok())
                .collect();
            if args
                .first()
                .is_some_and(|arg| arg.ends_with("/conmon") || *arg == "conmon")
                && args.iter().any(|arg| arg.contains(&needle))
            {
                found.push(pid);
            }
        }
        found
    }

    fn labels(&self, kind: &str, name: &str) -> Option<serde_json::Map<String, serde_json::Value>> {
        let output = self.command(&[kind, "inspect", name]).output().ok()?;
        if !output.status.success() {
            return None;
        }
        let json: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
        let object = json.as_array()?.first()?;
        object
            .pointer("/Config/Labels")
            .or_else(|| object.get("Labels"))?
            .as_object()
            .cloned()
    }

    fn labelled_container(
        &self,
        name: &str,
        volume: &str,
        node: &str,
        home: &Path,
        generation: &str,
    ) {
        self.remote(&[
            "create",
            "--pull=never",
            "--name",
            name,
            "--network",
            "none",
            "--hostname",
            "veles-user-env",
            "--label",
            &format!("{NODE_LABEL}={node}"),
            "--label",
            &format!("{HOME_LABEL}={}", home.display()),
            "--label",
            &format!("{GENERATION_LABEL}={generation}"),
            "-v",
            &format!("{volume}:/work"),
            "-w",
            "/work",
            "veles-sandbox:latest",
            "sleep",
            "infinity",
        ]);
        self.remote(&["start", name]);
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // Remove through the SAME private remote service that owns the
        // containers. A local `rm -af` can erase its store records while
        // remote-owned conmons are still running, leaving leaked processes
        // whose working directories then vanish with the scratch store.
        let removed = self
            .command(&["rm", "-a", "-f", "-t", "0"])
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .status();
        assert!(
            removed.is_ok_and(|status| status.success()),
            "private podman container cleanup failed"
        );
        let remaining = self.remote(&["ps", "-a", "--format", "{{.ID}}"]);
        assert!(
            remaining.is_empty(),
            "private podman containers survived teardown: {remaining}"
        );
        let _ = self
            .command(&["volume", "rm", "-af"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        // Let conmon's SIGTERM path reap its container and exit while
        // the service is still alive to service cleanup requests.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while !self.conmon_pids().is_empty() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        for pid in self.conmon_pids() {
            // Still fixture-scoped; no name- or user-wide process kill.
            unsafe { libc::kill(pid, libc::SIGTERM) };
        }
        while !self.conmon_pids().is_empty() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        for pid in self.conmon_pids() {
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
        let mut remaining = Vec::new();
        while std::time::Instant::now() < deadline {
            remaining = self.conmon_pids();
            if remaining.is_empty() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        let _ = self.service.kill();
        let _ = self.service.wait();
        assert!(
            remaining.is_empty(),
            "private conmons survived teardown: {remaining:?}"
        );
        for (key, old) in self.saved.drain(..) {
            match old {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
        // Mapped UIDs can own files in a rootless store. The store and
        // runroot are unique to this fixture, never the normal store.
        // Unmount the private root first: Podman's overlay cleanup can
        // still be walking its own mounts when rm starts, producing a
        // misleading fts_read ENOENT under the directory being removed.
        let store = self.root.join("fixture/store");
        let run = self.root.join("fixture/run");
        let unmounted = Command::new("podman")
            .env_remove("CONTAINER_HOST")
            .arg("--root")
            .arg(&store)
            .arg("--runroot")
            .arg(&run)
            .arg("unmount")
            .arg("--all")
            .arg("--force")
            .output();
        let mut failures = Vec::new();
        match unmounted {
            Ok(output) if output.status.success() => {}
            Ok(output) => failures.push(format!(
                "private store unmount: status={} stderr={}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            )),
            Err(error) => failures.push(format!("private store unmount: {error}")),
        }
        // Let Podman remove its OWN overlay bookkeeping after unmount.
        // A raw rm races the storage driver's lazy layer/lock cleanup
        // and intermittently leaves `overlay/l` or a lock behind. Both
        // flags below point at this fixture only, never the normal store.
        let reset = Command::new("podman")
            .env_remove("CONTAINER_HOST")
            .arg("--root")
            .arg(&store)
            .arg("--runroot")
            .arg(&run)
            .args(["system", "reset", "--force"])
            .output();
        match reset {
            Ok(output) if output.status.success() => {}
            Ok(output) => failures.push(format!(
                "private store reset: status={} stdout={} stderr={}",
                output.status,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )),
            Err(error) => failures.push(format!("private store reset: {error}")),
        }
        // Reset handles the mapped overlay files. Finish only the empty
        // root/runroot and any remaining lockfiles in the user namespace.
        let cleanup = Command::new("podman")
            .env_remove("CONTAINER_HOST")
            .arg("--remote=false")
            .arg("unshare")
            .arg("rm")
            .arg("-rf")
            .arg(&store)
            .arg(&run)
            .output();
        match cleanup {
            Ok(output) if output.status.success() => {}
            Ok(output) => failures.push(format!(
                "private store rm: status={} stdout={} stderr={}",
                output.status,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )),
            Err(error) => failures.push(format!("private store rm: {error}")),
        }
        if store.exists() || run.exists() {
            failures.push("private store or runroot still exists".into());
        }
        assert!(
            self.service
                .try_wait()
                .expect("private service status")
                .is_some(),
            "private podman service survived teardown"
        );
        if !failures.is_empty() {
            if std::thread::panicking() {
                eprintln!(
                    "fixture teardown after earlier failure: {}",
                    failures.join("; ")
                );
            } else {
                panic!("fixture teardown failed: {}", failures.join("; "));
            }
        }
    }
}

#[allow(clippy::await_holding_lock)] // one test binary, one env-changing test
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "opt-in: real podman, private service, cached sandbox image"]
async fn home_recreation_cleans_only_its_own_old_generation() {
    if std::env::var("VELES_USER_ENV_LIFECYCLE_TEST").as_deref() != Ok("1") {
        eprintln!("SKIP: set VELES_USER_ENV_LIFECYCLE_TEST=1 for the real podman lifecycle test");
        return;
    }
    let image = std::env::var("VELES_LIFECYCLE_TEST_IMAGE")
        .unwrap_or_else(|_| "veles-sandbox:latest".into());
    let fixture = Fixture::start(&image);
    fixture.config();
    std::fs::write(
        fixture.root.join("node.toml"),
        "id = \"lifecycle-old-node\"\nname = \"lifecycle-test\"\n",
    )
    .unwrap();
    std::fs::write(
        fixture.root.join(".veles-env-incarnation"),
        "old-generation",
    )
    .unwrap();

    let old_key = veles_core::user_envs::key_for("same-room");
    let old_name = veles_core::user_envs::container_name(&old_key);
    let old_volume = veles_core::user_envs::volume_for(&old_key);
    veles_core::user_envs::ensure(&old_key)
        .await
        .expect("old box");
    veles_core::user_envs::exec_shell(&old_key, "printf old > /work/keep", 30)
        .await
        .expect("old write");

    // A second node can see the same visible path through its own mount.
    // Its node identity, not that path, is the deletion boundary.
    let foreign_name = "veles-user-env-lifecycle-foreign";
    let foreign_volume = "veles-env-lifecycle-foreign";
    fixture.labelled_container(
        foreign_name,
        foreign_volume,
        "another-node",
        &fixture.root,
        "another-generation",
    );
    fixture.remote(&[
        "exec",
        foreign_name,
        "sh",
        "-c",
        "printf foreign > /work/keep",
    ]);
    let labels = fixture
        .labels("container", foreign_name)
        .expect("foreign labels");
    assert_eq!(
        labels.get(NODE_LABEL).and_then(|v| v.as_str()),
        Some("another-node")
    );

    // An orphaned volume from the old generation is reclaimed even if
    // its container has already gone; an unlabelled legacy volume is not.
    let old_orphan = "veles-env-lifecycle-old-orphan";
    fixture.remote(&[
        "volume",
        "create",
        "--label",
        &format!("{NODE_LABEL}=lifecycle-old-node"),
        "--label",
        &format!("{HOME_LABEL}={}", fixture.root.display()),
        "--label",
        &format!("{GENERATION_LABEL}=old-generation"),
        old_orphan,
    ]);
    let legacy_orphan = "veles-env-lifecycle-legacy-orphan";
    fixture.remote(&["volume", "create", legacy_orphan]);

    fixture.recreate_home();
    let new_key = veles_core::user_envs::key_for("same-room");
    assert_ne!(old_key, new_key, "a recreated home must get a new key");
    veles_core::user_envs::ensure(&new_key)
        .await
        .expect("new box after recreation");
    assert!(!fixture.remote_status(&["container", "inspect", &old_name]));
    assert!(!fixture.remote_status(&["volume", "inspect", &old_volume]));
    assert!(!fixture.remote_status(&["volume", "inspect", old_orphan]));
    assert!(fixture.remote_status(&["volume", "inspect", legacy_orphan]));

    assert!(fixture.remote_status(&["container", "inspect", foreign_name]));
    assert!(fixture.remote_status(&["volume", "inspect", foreign_volume]));
    assert_eq!(
        fixture.remote(&["exec", foreign_name, "cat", "/work/keep"]),
        "foreign"
    );
    let new_name = veles_core::user_envs::container_name(&new_key);
    let labels = fixture
        .labels("container", &new_name)
        .expect("new ownership labels");
    assert_eq!(
        labels.get(NODE_LABEL).and_then(|v| v.as_str()),
        Some("lifecycle-old-node")
    );
    assert_eq!(
        labels.get(GENERATION_LABEL).and_then(|v| v.as_str()),
        Some(
            std::fs::read_to_string(fixture.root.join(".veles-env-incarnation"))
                .expect("new generation")
                .trim(),
        )
    );

    // A current labelled container may still carry its pre-label volume.
    // Reusing that exact attached pair does not adopt an orphan volume.
    let mixed_key = veles_core::user_envs::key_for("mixed-reuse");
    let mixed_name = veles_core::user_envs::container_name(&mixed_key);
    let mixed_volume = veles_core::user_envs::volume_for(&mixed_key);
    fixture.remote(&["volume", "create", &mixed_volume]);
    let generation = std::fs::read_to_string(fixture.root.join(".veles-env-incarnation"))
        .expect("current generation")
        .trim()
        .to_string();
    fixture.labelled_container(
        &mixed_name,
        &mixed_volume,
        "lifecycle-old-node",
        &fixture.root,
        &generation,
    );
    fixture.remote(&["exec", &mixed_name, "sh", "-c", "printf mixed > /work/keep"]);
    veles_core::user_envs::ensure(&mixed_key)
        .await
        .expect("current container with its canonical legacy volume is reusable");
    assert_eq!(
        fixture.remote(&["exec", &mixed_name, "cat", "/work/keep"]),
        "mixed"
    );
    veles_core::user_envs::purge(&mixed_key, false)
        .await
        .expect("partially labelled pair can retain work through a purge");
    veles_core::user_envs::ensure(&mixed_key)
        .await
        .expect("partially labelled volume was migrated and is reusable");
    assert_eq!(
        fixture.remote(&["exec", &mixed_name, "cat", "/work/keep"]),
        "mixed"
    );

    // A healthy pre-label box remains reusable: compatibility does not
    // mean silently discarding its existing workspace.
    let legacy_key = veles_core::user_envs::key_for("legacy-reuse");
    let legacy_name = veles_core::user_envs::container_name(&legacy_key);
    let legacy_volume = veles_core::user_envs::volume_for(&legacy_key);
    fixture.remote(&["volume", "create", &legacy_volume]);
    fixture.remote(&[
        "create",
        "--pull=never",
        "--name",
        &legacy_name,
        "--network",
        "none",
        "-v",
        &format!("{legacy_volume}:/work"),
        "-w",
        "/work",
        "veles-sandbox:latest",
        "sleep",
        "infinity",
    ]);
    fixture.remote(&["start", &legacy_name]);
    fixture.remote(&[
        "exec",
        &legacy_name,
        "sh",
        "-c",
        "printf legacy > /work/keep",
    ]);
    veles_core::user_envs::ensure(&legacy_key)
        .await
        .expect("healthy legacy box remains reusable");
    assert_eq!(
        fixture.remote(&["exec", &legacy_name, "cat", "/work/keep"]),
        "legacy"
    );

    // The documented explicit purge keeps work. It must also be the
    // bounded migration point for a pre-label volume, not strand it at
    // the first new-code sandbox call.
    veles_core::user_envs::purge(&legacy_key, false)
        .await
        .expect("purge legacy container, retain work");
    assert!(fixture.remote_status(&["volume", "inspect", &legacy_volume]));
    let legacy_labels = fixture
        .labels("volume", &legacy_volume)
        .expect("retained volume carries ownership labels");
    assert_eq!(
        legacy_labels.get(NODE_LABEL).and_then(|v| v.as_str()),
        Some("lifecycle-old-node"),
        "a silent migration no-op must fail here: {}",
        fixture.remote(&["volume", "inspect", &legacy_volume])
    );
    assert_eq!(
        legacy_labels.get(HOME_LABEL).and_then(|v| v.as_str()),
        Some(fixture.root.to_str().unwrap())
    );
    let staged = fixture.remote(&[
        "volume",
        "ls",
        "--filter",
        "name=veles-userenv-recovery-",
        "--format",
        "{{.Name}}",
    ]);
    assert!(staged.is_empty(), "migration staging leaked: {staged}");
    veles_core::user_envs::ensure(&legacy_key)
        .await
        .expect("legacy retained work is reusable after its container is purged");
    assert_eq!(
        fixture.remote(&["exec", &legacy_name, "cat", "/work/keep"]),
        "legacy"
    );

    // An unlabelled orphan volume is not implicitly adopted either.
    let broken_key = veles_core::user_envs::key_for("legacy-broken");
    let broken_volume = veles_core::user_envs::volume_for(&broken_key);
    fixture.remote(&["volume", "create", &broken_volume]);
    let error = veles_core::user_envs::ensure(&broken_key)
        .await
        .expect_err("legacy orphan volume must be preserved, not adopted")
        .to_string();
    assert!(error.contains("no ownership labels"), "{error}");
    assert!(fixture.remote_status(&["volume", "inspect", &broken_volume]));

    veles_core::user_envs::purge(&new_key, true)
        .await
        .expect("new purge");
    veles_core::user_envs::purge(&legacy_key, true)
        .await
        .expect("legacy explicit purge");
    std::io::stderr().flush().ok();
}
