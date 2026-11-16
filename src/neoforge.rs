//! Installing NeoForge on top of a vanilla installation.
//!
//! NeoForge ships an installer jar that knows how to do this properly: it fetches the
//! extra libraries, runs the binary patcher over the vanilla client, and writes a version
//! json that inherits from vanilla. Reimplementing that here would mean reimplementing
//! their processor pipeline, which changes between versions and would quietly rot.
//!
//! So the installer is run as intended, with the Java the launcher provisioned. That is
//! the real reason Java came first: this step cannot happen without a JVM.

use anyhow::{anyhow, bail, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::fetch;
use crate::java::JavaRuntime;

const MAVEN: &str = "https://maven.neoforged.net/releases/net/neoforged/neoforge";

pub struct NeoForge {
    pub version_id: String,
    /// Read by the launch step, which merges it with the vanilla json it inherits from.
    #[allow(dead_code)]
    pub version_json: PathBuf,
}

/// Resolves against the current directory without canonicalising, which on Windows would
/// produce a \\?\ prefix that some Java code handles badly.
fn absolute(p: &Path) -> Result<PathBuf> {
    Ok(if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir()
            .context("reading the current directory")?
            .join(p)
    })
}

/// The installer refuses to run without this file, because it expects to be writing into
/// a real Mojang launcher installation. An empty profile list is enough to satisfy it.
fn ensure_launcher_profiles(base: &Path) -> Result<()> {
    let path = base.join("launcher_profiles.json");
    if path.is_file() {
        return Ok(());
    }
    let stub = br#"{"profiles":{},"settings":{},"version":3}"#;
    fetch::write_atomic(&path, stub).context("writing launcher_profiles.json")?;
    Ok(())
}

/// Already installed? The installer is slow, so skip it when the version json is there.
fn existing(base: &Path, version: &str) -> Option<NeoForge> {
    let versions = base.join("versions");
    let entries = fs::read_dir(&versions).ok()?;
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if !name.contains(version) || !name.to_lowercase().contains("neoforge") {
            continue;
        }
        let json = e.path().join(format!("{name}.json"));
        if json.is_file() {
            return Some(NeoForge {
                version_id: name,
                version_json: json,
            });
        }
    }
    None
}

pub fn install(
    base: &Path,
    java: &JavaRuntime,
    version: &str,
    mut on_step: impl FnMut(&str),
    mut on_progress: impl FnMut(u64, u64),
) -> Result<NeoForge> {
    if let Some(found) = existing(base, version) {
        on_step(&format!("NeoForge {version} is already installed"));
        return Ok(found);
    }

    ensure_launcher_profiles(base)?;

    let jar_name = format!("neoforge-{version}-installer.jar");
    let url = format!("{MAVEN}/{version}/{jar_name}");
    let installer = base.join("installers").join(&jar_name);

    // Maven publishes a .sha1 beside every artifact, so the download is verifiable
    // rather than merely successful.
    on_step(&format!("downloading the NeoForge {version} installer"));
    let sha1 = fetch::get_bytes(&format!("{url}.sha1"), 1024)
        .ok()
        .map(|b| String::from_utf8_lossy(&b).trim().to_string())
        .filter(|s| s.len() == 40 && s.chars().all(|c| c.is_ascii_hexdigit()));

    match &sha1 {
        Some(h) => fetch::download_if_needed(&url, &installer, h, 0, &mut on_progress)
            .with_context(|| format!("downloading {jar_name}"))?,
        None => {
            // no published hash: still fetch, but say so rather than pretend
            on_step("  (maven published no sha1 for the installer)");
            fetch::download_unverified(&url, &installer, 0, &mut on_progress)
                .with_context(|| format!("downloading {jar_name}"))?;
        }
    }

    on_step("running the NeoForge installer (this takes a minute)");
    // The child runs with its working directory set to base, so every path handed to it
    // must be absolute. Passing the relative ones made Java look for the jar at
    // .lotmcraft/.lotmcraft/installers/...
    let abs_base = absolute(base)?;
    let abs_installer = absolute(&installer)?;
    let out = Command::new(absolute(&java.java)?)
        .arg("-jar")
        .arg(&abs_installer)
        .arg("--install-client")
        .arg(&abs_base)
        .current_dir(&abs_base)
        .output()
        .with_context(|| format!("running {}", java.java.display()))?;

    if !out.status.success() {
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        let tail: Vec<&str> = stdout
            .lines()
            .chain(stderr.lines())
            .rev()
            .take(25)
            .collect();
        let tail: Vec<&str> = tail.into_iter().rev().collect();
        bail!(
            "the NeoForge installer failed ({}):\n{}",
            out.status,
            tail.join("\n")
        );
    }

    existing(base, version).ok_or_else(|| {
        anyhow!(
            "the installer reported success but no NeoForge version json appeared \
             under {}",
            base.join("versions").display()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_an_existing_install() {
        let dir = std::env::temp_dir().join(format!("lotm-nf-{}", std::process::id()));
        let v = dir.join("versions").join("neoforge-21.1.264");
        fs::create_dir_all(&v).unwrap();
        fs::write(v.join("neoforge-21.1.264.json"), b"{}").unwrap();

        let found = existing(&dir, "21.1.264").expect("should find it");
        assert_eq!(found.version_id, "neoforge-21.1.264");

        assert!(existing(&dir, "21.1.999").is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn writes_a_profiles_stub_once() {
        let dir = std::env::temp_dir().join(format!("lotm-lp-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        ensure_launcher_profiles(&dir).unwrap();
        let path = dir.join("launcher_profiles.json");
        assert!(path.is_file());

        fs::write(&path, b"{\"profiles\":{\"mine\":1}}").unwrap();
        ensure_launcher_profiles(&dir).unwrap();
        // an existing file is left alone
        assert!(fs::read_to_string(&path).unwrap().contains("mine"));
        let _ = fs::remove_dir_all(&dir);
    }
}
