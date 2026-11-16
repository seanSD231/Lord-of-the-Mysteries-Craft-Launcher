//! The manifest, as produced by `tools/manifest.py`.
//!
//! Everything here comes off the network, so nothing in it is trusted. In particular
//! `path` is checked before it is ever joined onto the instance folder: a manifest that
//! said `../../../.ssh/authorized_keys` would otherwise write outside the instance.

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Deserialize)]
pub struct Manifest {
    pub name: String,
    pub version: String,
    pub minecraft: String,
    #[serde(default)]
    pub neoforge: String,
    #[serde(default)]
    pub bundle: Option<Bundle>,
    pub files: Vec<FileEntry>,
}

#[derive(Debug, Deserialize)]
pub struct Bundle {
    pub file: String,
    pub size: u64,
    pub sha1: String,
    pub url: String,
    #[serde(default)]
    #[allow(dead_code)] // reported in the UI
    pub count: usize,
}

#[derive(Debug, Deserialize, Clone)]
pub struct FileEntry {
    pub path: String,
    #[serde(default)]
    pub size: u64,
    pub sha1: String,
    #[serde(default)]
    pub url: Option<String>,
    pub source: String,
}

impl FileEntry {
    /// True for files that arrive inside `overrides.zip` rather than by their own URL.
    pub fn in_bundle(&self) -> bool {
        self.source == "bundle"
    }
}

impl Manifest {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let m: Manifest = serde_json::from_slice(bytes).context("manifest is not valid JSON")?;
        m.validate()?;
        Ok(m)
    }

    fn validate(&self) -> Result<()> {
        if self.files.is_empty() {
            return Err(anyhow!("manifest lists no files"));
        }
        for f in &self.files {
            safe_relative(&f.path)
                .with_context(|| format!("manifest contains an unsafe path: {}", f.path))?;
            if f.sha1.len() != 40 || !f.sha1.chars().all(|c| c.is_ascii_hexdigit()) {
                return Err(anyhow!("{}: sha1 is not a 40-character hex digest", f.path));
            }
            if !f.in_bundle() && f.url.as_deref().unwrap_or("").is_empty() {
                return Err(anyhow!("{}: no url and not in the bundle", f.path));
            }
        }
        if let Some(b) = &self.bundle {
            if b.sha1.len() != 40 {
                return Err(anyhow!("bundle sha1 is not a 40-character hex digest"));
            }
        } else if self.files.iter().any(|f| f.in_bundle()) {
            return Err(anyhow!("files are marked as bundled but no bundle is declared"));
        }
        Ok(())
    }
}

/// Resolves a manifest path against the instance root, refusing anything that escapes it.
///
/// Rejects absolute paths, Windows drive prefixes, UNC paths and any `..` component.
/// This runs on every path from the manifest and every entry name inside the zip.
pub fn safe_relative(rel: &str) -> Result<PathBuf> {
    if rel.is_empty() {
        return Err(anyhow!("empty path"));
    }
    if rel.contains('\0') {
        return Err(anyhow!("path contains a null byte"));
    }
    let p = Path::new(rel);
    for c in p.components() {
        match c {
            Component::Normal(_) => {}
            Component::CurDir => {}
            Component::ParentDir => return Err(anyhow!("path escapes the instance folder")),
            Component::RootDir | Component::Prefix(_) => {
                return Err(anyhow!("path is absolute"))
            }
        }
    }
    Ok(p.to_path_buf())
}

/// Joins a manifest path onto the instance root. Always use this, never `root.join(raw)`.
pub fn resolve(root: &Path, rel: &str) -> Result<PathBuf> {
    Ok(root.join(safe_relative(rel)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_traversal() {
        assert!(safe_relative("../evil").is_err());
        assert!(safe_relative("mods/../../evil").is_err());
        assert!(safe_relative("/etc/passwd").is_err());
        assert!(safe_relative("").is_err());
    }

    #[test]
    fn accepts_ordinary_paths() {
        assert!(safe_relative("mods/citadel-1.21.1-2.7.6.jar").is_ok());
        assert!(safe_relative("config/foo/bar.toml").is_ok());
    }

    #[test]
    fn rejects_short_hashes() {
        let json = br#"{"name":"x","version":"1","minecraft":"1.21.1","files":
            [{"path":"mods/a.jar","size":1,"sha1":"abc","url":"https://e/a","source":"curseforge"}]}"#;
        assert!(Manifest::parse(json).is_err());
    }
}
