//! Getting a JVM, without asking the player to install one.
//!
//! "Download Java first" is the biggest support burden a Minecraft launcher can carry,
//! and trusting whatever Java is already on the machine is worse: a stale Java 8 left
//! over from something else will fail in ways that look like our bug. So the launcher
//! fetches its own Temurin 21 from Adoptium and uses only that.
//!
//! The runtime lives under the instance folder, not in a system location, so uninstalling
//! is deleting a folder and nothing needs administrator rights.

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::fetch;

/// Minecraft 1.21.1 runs on Java 21, and NeoForge 21.1.x expects it.
const FEATURE_VERSION: u32 = 21;
const ADOPTIUM: &str = "https://api.adoptium.net/v3/assets/latest";

#[derive(Debug, Deserialize)]
struct Asset {
    binary: Binary,
    #[serde(default)]
    release_name: String,
}

#[derive(Debug, Deserialize)]
struct Binary {
    package: Package,
}

#[derive(Debug, Deserialize)]
struct Package {
    link: String,
    name: String,
    #[serde(default)]
    checksum: String,
    #[serde(default)]
    size: u64,
}

pub struct JavaRuntime {
    pub java: PathBuf,
    /// The JDK root. The NeoForge installer and the launch both need it.
    #[allow(dead_code)]
    pub home: PathBuf,
}

impl JavaRuntime {
    /// Runs `java -version` and returns what it prints. Confirms the binary actually
    /// runs on this machine, which catches a wrong-architecture download that extracted
    /// perfectly well.
    pub fn version(&self) -> Result<String> {
        let out = Command::new(&self.java)
            .arg("-version")
            .output()
            .with_context(|| format!("running {}", self.java.display()))?;
        // java -version writes to stderr, which surprises everyone once
        let text = String::from_utf8_lossy(&out.stderr).trim().to_string();
        if !out.status.success() {
            bail!("{} exited with {}", self.java.display(), out.status);
        }
        Ok(text)
    }
}

/// The OS and architecture names Adoptium uses, for this build.
fn platform() -> Result<(&'static str, &'static str, &'static str)> {
    let os = if cfg!(target_os = "windows") {
        "windows"
    } else if cfg!(target_os = "macos") {
        "mac"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else {
        bail!("unsupported operating system")
    };
    let arch = if cfg!(target_arch = "x86_64") {
        "x64"
    } else if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else {
        bail!("unsupported processor architecture")
    };
    // what the downloaded archive is
    let ext = if os == "windows" { "zip" } else { "tar.gz" };
    Ok((os, arch, ext))
}

/// Where the runtime lives, and where `java` sits inside it once extracted.
fn java_exe(home: &Path) -> PathBuf {
    if cfg!(target_os = "windows") {
        home.join("bin").join("java.exe")
    } else {
        home.join("bin").join("java")
    }
}

/// Adoptium's macOS archives put the JDK inside `Contents/Home`.
fn find_home(extracted: &Path) -> Result<PathBuf> {
    // the archive contains a single top-level folder, whose name carries the build
    let mut candidates: Vec<PathBuf> = fs::read_dir(extracted)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    candidates.sort();
    for base in candidates {
        for home in [base.clone(), base.join("Contents").join("Home")] {
            if java_exe(&home).is_file() {
                return Ok(home);
            }
        }
    }
    Err(anyhow!(
        "the archive extracted but no bin/java was found under {}",
        extracted.display()
    ))
}

/// Returns a usable Java 21, downloading it if it is not already there.
pub fn ensure(runtime_dir: &Path, mut on_progress: impl FnMut(u64, u64)) -> Result<JavaRuntime> {
    let marker = runtime_dir.join(format!("jre-{FEATURE_VERSION}"));

    // already installed, and it actually runs
    if marker.is_dir() {
        if let Ok(home) = find_home(&marker) {
            let rt = JavaRuntime {
                java: java_exe(&home),
                home,
            };
            if rt.version().is_ok() {
                return Ok(rt);
            }
            // extracted but broken: a half-finished download, or the wrong architecture
            fs::remove_dir_all(&marker).ok();
        } else {
            fs::remove_dir_all(&marker).ok();
        }
    }

    let (os, arch, ext) = platform()?;
    let url = format!(
        "{ADOPTIUM}/{FEATURE_VERSION}/hotspot\
         ?architecture={arch}&image_type=jre&os={os}&vendor=eclipse"
    );
    let body = fetch::get_bytes(&url, 4 << 20).context("asking Adoptium for a Java 21 build")?;
    let assets: Vec<Asset> =
        serde_json::from_slice(&body).context("Adoptium returned something unexpected")?;
    let asset = assets
        .into_iter()
        .find(|a| a.binary.package.name.ends_with(ext))
        .ok_or_else(|| anyhow!("Adoptium has no {ext} build for {os}/{arch}"))?;

    fs::create_dir_all(runtime_dir)?;
    let archive = runtime_dir.join(&asset.binary.package.name);
    download_sha256(
        &asset.binary.package.link,
        &archive,
        &asset.binary.package.checksum,
        asset.binary.package.size,
        &mut on_progress,
    )
    .with_context(|| format!("downloading {}", asset.release_name))?;

    // extract beside, then rename, so an interrupted extraction is never mistaken
    // for a finished one
    let staging = runtime_dir.join(format!(".jre-{FEATURE_VERSION}-staging"));
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging)?;
    if ext == "zip" {
        extract_zip(&archive, &staging)?;
    } else {
        extract_tar_gz(&archive, &staging)?;
    }
    let _ = fs::remove_file(&archive);

    let home_in_staging = find_home(&staging)?;
    let rel = home_in_staging
        .strip_prefix(&staging)
        .unwrap_or(Path::new(""))
        .to_path_buf();
    let _ = fs::remove_dir_all(&marker);
    fs::rename(&staging, &marker)
        .with_context(|| format!("moving the runtime into {}", marker.display()))?;

    let home = marker.join(rel);
    let rt = JavaRuntime {
        java: java_exe(&home),
        home,
    };
    rt.version()
        .context("the runtime downloaded but will not run on this machine")?;
    Ok(rt)
}

fn download_sha256(
    url: &str,
    dest: &Path,
    checksum: &str,
    size: u64,
    on_progress: &mut impl FnMut(u64, u64),
) -> Result<()> {
    // Adoptium publishes SHA-256, where everything else here is SHA-1, so this does not
    // go through fetch::download_verified.
    fetch::download_unverified(url, dest, size, on_progress)?;
    if checksum.is_empty() {
        return Ok(());
    }
    let mut f = File::open(dest)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    let got: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
    if !got.eq_ignore_ascii_case(checksum) {
        let _ = fs::remove_file(dest);
        bail!("the Java download did not match its published checksum");
    }
    Ok(())
}

fn extract_zip(archive: &Path, into: &Path) -> Result<()> {
    let file = File::open(archive)?;
    let mut zip = zip::ZipArchive::new(file).context("the Java archive is not a valid zip")?;
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i)?;
        let Some(rel) = entry.enclosed_name() else {
            bail!("the Java archive contains an unsafe path");
        };
        let out = into.join(rel);
        if entry.is_dir() {
            fs::create_dir_all(&out)?;
            continue;
        }
        if let Some(parent) = out.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut f = File::create(&out)?;
        std::io::copy(&mut entry, &mut f)?;
    }
    Ok(())
}

fn extract_tar_gz(archive: &Path, into: &Path) -> Result<()> {
    let file = File::open(archive)?;
    let gz = flate2::read::GzDecoder::new(file);
    let mut tar = tar::Archive::new(gz);
    // tar carries the executable bit, which `java` needs on mac and linux
    tar.set_preserve_permissions(true);
    tar.unpack(into)
        .context("unpacking the Java archive")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn this_platform_is_supported() {
        let (os, arch, ext) = platform().expect("CI runs on a supported platform");
        assert!(!os.is_empty() && !arch.is_empty());
        assert!(ext == "zip" || ext == "tar.gz");
    }

    #[test]
    fn java_exe_is_under_bin() {
        let p = java_exe(Path::new("/x/jre"));
        assert!(p.starts_with("/x/jre/bin") || p.starts_with("\\x\\jre\\bin"));
    }
}
