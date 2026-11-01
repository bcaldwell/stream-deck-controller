//! Self-update: download the client binary the server offers and replace the running one.
//!
//! On `ServerMessage::Update` the session calls [`apply`], which downloads over HTTP from the
//! same host:port as the WebSocket, checks the sha256, and atomically renames the new file
//! over the current executable. The process then exits and the service manager (systemd
//! `Restart=always`) starts the new version; a clean exit 0 is used so it is not a failure.

use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use sdc_protocol::sha256_hex;

/// Sanity cap on the download (the static binary is ~8 MB).
const MAX_BINARY_BYTES: u64 = 128 * 1024 * 1024;
const HTTP_TIMEOUT: Duration = Duration::from_secs(120);

/// Hex sha256 of the file at `path`.
pub fn file_sha256(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(sha256_hex(&bytes))
}

/// Hex sha256 of the running executable.
pub fn exe_sha256() -> Result<String> {
    file_sha256(&std::env::current_exe().context("locating current executable")?)
}

/// `ws://host:7700/ws` -> `http://host:7700` (`wss` -> `https`; http(s) URLs pass through).
pub fn http_base(server: &str) -> Result<String> {
    let (scheme, rest) = server
        .split_once("://")
        .ok_or_else(|| anyhow!("server URL {server:?} has no scheme"))?;
    let scheme = match scheme {
        "ws" | "http" => "http",
        "wss" | "https" => "https",
        other => bail!("unsupported server URL scheme {other:?}"),
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.is_empty() {
        bail!("server URL {server:?} has no host");
    }
    Ok(format!("{scheme}://{authority}"))
}

/// Download `path` from the server, verify it hashes to `sha256` and replace the running
/// executable with it. Blocking: call from `spawn_blocking`.
pub fn apply(server: &str, path: &str, sha256: &str) -> Result<()> {
    let url = format!("{}{path}", http_base(server)?);
    let exe = std::env::current_exe().context("locating current executable")?;
    let dir = exe
        .parent()
        .ok_or_else(|| anyhow!("executable has no directory"))?;
    check_writable(dir)?;

    let resp = ureq::AgentBuilder::new()
        .timeout(HTTP_TIMEOUT)
        .build()
        .get(&url)
        .call()
        .with_context(|| format!("downloading {url}"))?;
    let mut bytes = Vec::new();
    resp.into_reader()
        .take(MAX_BINARY_BYTES)
        .read_to_end(&mut bytes)
        .with_context(|| format!("reading {url}"))?;
    replace_exe(&exe, &bytes, sha256)
}

/// Fail early with a clear message instead of downloading into a read-only location.
fn check_writable(dir: &Path) -> Result<()> {
    let probe = dir.join(format!(".sdc-client.probe.{}", std::process::id()));
    std::fs::File::create(&probe)
        .map_err(|e| anyhow!("{} is not writable ({e})", dir.display()))?;
    let _ = std::fs::remove_file(&probe);
    Ok(())
}

/// Verify `bytes` against `sha256` and atomically replace the file at `exe` with them.
/// The temp file lives in the same directory so the final rename never crosses filesystems.
pub fn replace_exe(exe: &Path, bytes: &[u8], sha256: &str) -> Result<()> {
    let got = sha256_hex(bytes);
    if !got.eq_ignore_ascii_case(sha256) {
        bail!("checksum mismatch: server said {sha256}, downloaded {got}");
    }
    let dir = exe
        .parent()
        .ok_or_else(|| anyhow!("executable has no directory"))?;
    let tmp: PathBuf = dir.join(format!(".sdc-client.update.{}", std::process::id()));
    let result = (|| -> Result<()> {
        let mut f =
            std::fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
        f.write_all(bytes)?;
        f.set_permissions(std::fs::Permissions::from_mode(0o755))?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, exe).with_context(|| format!("replacing {}", exe.display()))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("sdc-upd-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn derives_http_base() {
        assert_eq!(
            http_base("ws://10.0.0.1:7700/ws").unwrap(),
            "http://10.0.0.1:7700"
        );
        assert_eq!(
            http_base("wss://deck.example.com/ws").unwrap(),
            "https://deck.example.com"
        );
        assert_eq!(http_base("ws://h:1").unwrap(), "http://h:1");
        assert_eq!(http_base("http://h:2/x?y").unwrap(), "http://h:2");
        assert!(http_base("h:7700/ws").is_err());
        assert!(http_base("ftp://h/ws").is_err());
        assert!(http_base("ws:///ws").is_err());
    }

    #[test]
    fn hashes_files() {
        let d = tempdir("hash");
        let f = d.join("f");
        std::fs::write(&f, b"abc").unwrap();
        assert_eq!(
            file_sha256(&f).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn replaces_file_atomically() {
        let d = tempdir("replace");
        let exe = d.join("sdc-client");
        std::fs::write(&exe, b"old").unwrap();
        let new = b"new binary";
        replace_exe(&exe, new, &sha256_hex(new)).unwrap();
        assert_eq!(std::fs::read(&exe).unwrap(), new);
        assert_eq!(
            std::fs::metadata(&exe).unwrap().permissions().mode() & 0o777,
            0o755
        );
        // only the replaced file is left behind
        assert_eq!(std::fs::read_dir(&d).unwrap().count(), 1);
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn rejects_bad_checksum_and_keeps_old_file() {
        let d = tempdir("badsum");
        let exe = d.join("sdc-client");
        std::fs::write(&exe, b"old").unwrap();
        assert!(replace_exe(&exe, b"new", &sha256_hex(b"other")).is_err());
        assert_eq!(std::fs::read(&exe).unwrap(), b"old");
        assert_eq!(std::fs::read_dir(&d).unwrap().count(), 1);
        std::fs::remove_dir_all(d).unwrap();
    }
}
