//! Updating the launcher itself.
//!
//! A release tagged `launcher-latest` carries `launcher.json` plus one binary per
//! platform. The launcher reads that file on startup, and if it names a newer version
//! than its own, fetches the binary for this platform and swaps itself for it.
//!
//! The swap is the only awkward part. A running executable cannot be overwritten on
//! Windows, but it *can* be renamed, so: move the current one aside, write the new one in
//! its place, start it, and exit. The leftover is deleted on the next startup, when
//! nothing is running from it.
//!
//! Every download is checked against the SHA-256 in `launcher.json` before anything is
//! moved. A launcher that replaces itself with an unverified binary is a launcher that
//! can be made to run anything.

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

const FEED: &str = concat!(
    "https://github.com/seanSD231/Lord-of-the-Mysteries-Craft-Launcher",
    "/releases/download/launcher-latest/launcher.json"
);

#[derive(Deserialize)]
struct Feed {
    version: String,
    #[serde(default)]
    notes: String,
    builds: Vec<Build>,
}

#[derive(Deserialize)]
struct Build {
    /// `windows-x64`, `macos-arm64`, `linux-x64`.
    target: String,
    url: String,
    sha256: String,
    #[serde(default)]
    size: u64,
}

#[derive(serde::Serialize)]
pub struct Available {
    pub version: String,
    pub notes: String,
}

fn target() -> &'static str {
    if cfg!(target_os = "windows") {
        "windows-x64"
    } else if cfg!(target_os = "macos") {
        if cfg!(target_arch = "aarch64") {
            "macos-arm64"
        } else {
            "macos-x64"
        }
    } else {
        "linux-x64"
    }
}

/// Compares dotted numeric versions. Anything unparseable sorts as zero, so a malformed
/// feed cannot talk the launcher into "upgrading" backwards.
fn newer(remote: &str, local: &str) -> bool {
    let parts = |s: &str| -> Vec<u64> {
        s.split(['.', '-', '+'])
            .map(|p| p.parse::<u64>().unwrap_or(0))
            .collect()
    };
    let (a, b) = (parts(remote), parts(local));
    for i in 0..a.len().max(b.len()) {
        let (x, y) = (a.get(i).copied().unwrap_or(0), b.get(i).copied().unwrap_or(0));
        if x != y {
            return x > y;
        }
    }
    false
}

/// Removes the previous binary left beside us by an earlier update.
pub fn clean_old() {
    if let Ok(exe) = std::env::current_exe() {
        let old = old_path(&exe);
        if old.exists() {
            let _ = fs::remove_file(&old);
        }
    }
}

fn old_path(exe: &Path) -> PathBuf {
    let mut name = exe.file_name().unwrap_or_default().to_os_string();
    name.push(".old");
    exe.with_file_name(name)
}

/// Returns the newer version if there is one. Never an error the window has to show:
/// a launcher that will not start because its update check failed is worse than one
/// running a version behind.
pub fn check() -> Option<Available> {
    let bytes = crate::fetch::get_bytes(FEED, 256 << 10).ok()?;
    let feed: Feed = serde_json::from_slice(&bytes).ok()?;
    if !newer(&feed.version, env!("CARGO_PKG_VERSION")) {
        return None;
    }
    feed.builds.iter().find(|b| b.target == target())?;
    Some(Available {
        version: feed.version,
        notes: feed.notes,
    })
}

/// Downloads the new binary, verifies it, and puts it in place.
///
/// Returns the path to run. The caller starts it and exits; this function deliberately
/// does neither, so the window can close itself tidily first.
pub fn apply(mut on_progress: impl FnMut(u64, u64)) -> Result<PathBuf> {
    let bytes = crate::fetch::get_bytes(FEED, 256 << 10).context("fetching the update feed")?;
    let feed: Feed = serde_json::from_slice(&bytes).context("the update feed is malformed")?;
    if !newer(&feed.version, env!("CARGO_PKG_VERSION")) {
        bail!("already up to date");
    }
    let build = feed
        .builds
        .into_iter()
        .find(|b| b.target == target())
        .ok_or_else(|| anyhow!("the update has no build for {}", target()))?;

    let exe = std::env::current_exe().context("finding the running launcher")?;
    let dir = exe.parent().unwrap_or(Path::new(".")).to_path_buf();
    let staged = dir.join(format!(".lotmcraft-update-{}", feed.version));

    crate::fetch::download_unverified(&build.url, &staged, build.size, &mut on_progress)
        .context("downloading the update")?;

    let got = sha256_of(&staged)?;
    if !got.eq_ignore_ascii_case(&build.sha256) {
        let _ = fs::remove_file(&staged);
        bail!("the update did not match its published checksum; nothing was replaced");
    }

    // On unix the new file needs to be executable; the zip/raw asset may not be.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&staged, fs::Permissions::from_mode(0o755));
    }

    let old = old_path(&exe);
    let _ = fs::remove_file(&old);
    fs::rename(&exe, &old).context("moving the running launcher aside")?;
    if let Err(e) = fs::rename(&staged, &exe) {
        // put it back rather than leave the player with no launcher at all
        let _ = fs::rename(&old, &exe);
        return Err(e).context("putting the new launcher in place");
    }
    Ok(exe)
}

fn sha256_of(path: &Path) -> Result<String> {
    let mut f = fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_comparison() {
        assert!(newer("0.2.0", "0.1.9"));
        assert!(newer("1.0.0", "0.9.9"));
        assert!(newer("0.1.10", "0.1.9"));
        assert!(!newer("0.1.0", "0.1.0"));
        assert!(!newer("0.1.0", "0.2.0"));
        // a malformed feed must not look newer
        assert!(!newer("rubbish", "0.1.0"));
    }

    #[test]
    fn old_path_sits_beside_the_exe() {
        assert_eq!(
            old_path(Path::new("/a/b/lotmcraft-gui")),
            PathBuf::from("/a/b/lotmcraft-gui.old")
        );
    }
}
