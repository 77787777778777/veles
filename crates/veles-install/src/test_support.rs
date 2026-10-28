//! Hermetic signed-release inputs shared by CLI and desktop tests. This
//! module is test-only and uses a Unix-socket loopback, never HTTP in prod.
#![cfg(unix)]
use std::io::Write as _;
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};

pub fn fixture(home: &Path, tamper: bool, reject_cosign: bool) -> (String, PathBuf) {
    fixture_with_info(home, tamper, reject_cosign, "1.0.0", "1.0.0", "abcdef1")
}

pub fn fixture_with_info(
    home: &Path,
    tamper: bool,
    reject_cosign: bool,
    manifest_version: &str,
    bundle_version: &str,
    bundle_revision: &str,
) -> (String, PathBuf) {
    let arch = std::env::consts::ARCH;
    let system = format!("{arch}-linux");
    let source = home.join("bundle-source");
    std::fs::create_dir_all(source.join("bin")).unwrap();
    std::fs::write(source.join("BUNDLE-INFO.json"), format!(r#"{{"version":{bundle_version:?},"revision":{bundle_revision:?},"architecture":{arch:?}}}"#)).unwrap();
    std::fs::write(source.join("bin/veles"), "signed fixture\n").unwrap();
    let archive = home.join("fixture.tar.zst");
    let encoder =
        zstd::stream::write::Encoder::new(std::fs::File::create(&archive).unwrap(), 0).unwrap();
    let mut tar = tar::Builder::new(encoder);
    tar.append_dir_all(".", &source).unwrap();
    tar.into_inner().unwrap().finish().unwrap();
    let archive_bytes = std::fs::read(&archive).unwrap();
    let digest = <sha2::Sha256 as sha2::Digest>::digest(&archive_bytes);
    let digest = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let commit = "abcdef1234567890abcdef1234567890abcdef12";
    let name = format!("veles-{manifest_version}-abcdef1-linux-{arch}.tar.zst");
    let manifest = serde_json::json!({
        "schemaVersion": 1, "repository": "77787777778777/veles",
        "tag": "v1.0.0", "commit": commit, "version": manifest_version,
        "systems": ["x86_64-linux", "aarch64-linux", system],
        "source": format!("git+https://github.com/77787777778777/veles?rev={commit}"),
        "signing": {"files": "sigstore-keyless-cosign", "oci": "cosign-by-digest", "android": "explicit-upload-key"},
        "artifacts": [{"name": name, "sha256": digest, "size": archive_bytes.len(), "kind": "archive"}]
    });
    let cosign = home.join("cosign");
    let log = home.join("cosign-calls");
    std::fs::write(&cosign, format!("#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\n[ \"$1\" = verify-blob ] || exit 9\ncase \" $* \" in\n  *' --certificate-identity https://github.com/77787777778777/veles/.github/workflows/release.yml@refs/tags/v1.0.0 '* ) ;;\n  * ) echo wrong-identity >&2; exit 8 ;;\nesac\nif [ '{}' = 1 ] && [ $# -gt 0 ]; then\n  for last; do :; done\n  case \"$last\" in *.tar.zst) echo rejected >&2; exit 7;; esac\nfi\n", log.display(), usize::from(tamper || reject_cosign))).unwrap();
    std::fs::set_permissions(&cosign, std::fs::Permissions::from_mode(0o700)).unwrap();
    let socket = home.join("release.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let root = format!("http://localhost/77787777778777/veles/releases/download/v1.0.0/{name}");
    let base = root.strip_suffix(&name).unwrap().to_owned();
    let manifest = serde_json::to_vec(&manifest).unwrap();
    let archive_bytes = if tamper {
        let mut changed = archive_bytes;
        *changed.last_mut().unwrap() ^= 1;
        changed
    } else {
        archive_bytes
    };
    let files = [
        ("release-manifest.json".to_owned(), manifest),
        (
            "release-manifest.json.sigstore.json".to_owned(),
            b"{\"fixture\":true}\n".to_vec(),
        ),
        (name.to_owned(), archive_bytes),
        (
            format!("{name}.sigstore.json"),
            b"{\"fixture\":true}\n".to_vec(),
        ),
    ];
    let request_log = home.join("release-requests");
    std::thread::spawn(move || loop {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = Vec::new();
        let mut buffer = [0u8; 4096];
        loop {
            let n = std::io::Read::read(&mut stream, &mut buffer).unwrap();
            request.extend_from_slice(&buffer[..n]);
            if request.windows(4).any(|part| part == b"\r\n\r\n") {
                break;
            }
        }
        let request = String::from_utf8_lossy(&request);
        let path = request.split_whitespace().nth(1).unwrap();
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&request_log)
            .unwrap()
            .write_all(format!("{path}\n").as_bytes())
            .unwrap();
        let path = path.strip_prefix("http://localhost").unwrap_or(path);
        let file = path
            .strip_prefix("/77787777778777/veles/releases/download/v1.0.0/")
            .unwrap_or_else(|| panic!("unexpected release request {path}"));
        let body = files
            .iter()
            .find(|(name, _)| name == file)
            .map(|(_, body)| body)
            .unwrap_or_else(|| panic!("unexpected release request {path}"));
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .unwrap();
        stream.write_all(body).unwrap();
    });
    (base, socket)
}
