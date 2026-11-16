//! Working out what has to change, then changing it.
//!
//! Planning is separate from doing so that `--dry-run` reports exactly what a real run
//! would do, and so the plan can be shown to the player before anything is touched.

use anyhow::{Context, Result};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use crate::fetch::{self, sha1_of_file};
use crate::manifest::{resolve, safe_relative, Manifest};
use crate::policy::{is_managed_for_delete, policy_for, Policy, MANAGED_DELETE_DIRS};

#[derive(Debug, Default)]
pub struct Plan {
    /// Files to fetch from their own URL, with the reason, for reporting.
    pub download: Vec<(String, String)>,
    /// True when at least one bundled file is missing or (if enforced) has drifted.
    pub need_bundle: bool,
    /// Why the bundle is needed, for reporting. Empty when it is not.
    pub bundle_reasons: Vec<String>,
    /// Files inside managed folders that the manifest no longer lists.
    pub orphans: Vec<PathBuf>,
    /// Bytes that will be downloaded for the individual files.
    pub download_bytes: u64,
}

impl Plan {
    pub fn is_empty(&self) -> bool {
        self.download.is_empty() && !self.need_bundle && self.orphans.is_empty()
    }
}

pub fn plan(m: &Manifest, root: &Path) -> Result<Plan> {
    let mut p = Plan::default();

    for f in &m.files {
        let dest = resolve(root, &f.path)?;
        let exists = dest.is_file();

        // Hashing a gigabyte on every launch is slow, so a file of exactly the right
        // size is taken on trust and only the hash of a size mismatch is computed.
        // `--verify` forces the full check.
        let drifted = if !exists {
            true
        } else if f.size > 0 {
            fs::metadata(&dest)?.len() != f.size
        } else {
            !sha1_of_file(&dest)?.eq_ignore_ascii_case(&f.sha1)
        };

        if !drifted {
            continue;
        }

        if f.in_bundle() {
            // A seed file that has merely been edited is the player's business; only a
            // missing one has to be restored.
            let needed = match policy_for(&f.path) {
                Policy::Enforced => true,
                Policy::Seed => !exists,
            };
            if needed {
                p.need_bundle = true;
                if p.bundle_reasons.len() < 10 {
                    p.bundle_reasons.push(format!(
                        "{} ({})",
                        f.path,
                        if exists { "changed" } else { "missing" }
                    ));
                }
            }
        } else {
            p.download.push((
                f.path.clone(),
                if exists { "changed" } else { "missing" }.to_string(),
            ));
            p.download_bytes += f.size;
        }
    }

    p.orphans = find_orphans(m, root)?;
    Ok(p)
}

/// Files sitting in a managed folder that the manifest does not list.
fn find_orphans(m: &Manifest, root: &Path) -> Result<Vec<PathBuf>> {
    let known: BTreeSet<PathBuf> = m
        .files
        .iter()
        .filter_map(|f| safe_relative(&f.path).ok())
        .collect();

    let mut out = Vec::new();
    for dir in MANAGED_DELETE_DIRS {
        let base = root.join(dir);
        if !base.is_dir() {
            continue;
        }
        for entry in fs::read_dir(&base)? {
            let entry = entry?;
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let Ok(rel) = path.strip_prefix(root) else {
                continue;
            };
            // our own leftovers, not the player's files
            if path.extension().map(|e| e == "part").unwrap_or(false) {
                out.push(path.clone());
                continue;
            }
            if !known.contains(rel) {
                out.push(path);
            }
        }
    }
    out.sort();
    Ok(out)
}

pub struct Progress<'a> {
    pub on_file: &'a mut dyn FnMut(&str, usize, usize),
    pub on_bytes: &'a mut dyn FnMut(u64, u64),
}

pub fn apply(m: &Manifest, root: &Path, p: &Plan, progress: &mut Progress) -> Result<()> {
    let total = p.download.len();
    for (i, (path, _)) in p.download.iter().enumerate() {
        let f = m
            .files
            .iter()
            .find(|f| &f.path == path)
            .context("plan referred to a file that is not in the manifest")?;
        let url = f.url.as_deref().unwrap_or_default();
        let dest = resolve(root, &f.path)?;
        (progress.on_file)(&f.path, i + 1, total);
        fetch::download_verified(url, &dest, &f.sha1, f.size, |a, b| {
            (progress.on_bytes)(a, b)
        })
        .with_context(|| format!("downloading {}", f.path))?;
    }

    if p.need_bundle {
        let bundle = m
            .bundle
            .as_ref()
            .context("bundled files are needed but the manifest declares no bundle")?;
        (progress.on_file)(&bundle.file, total, total);
        let tmp = root.join(format!(".{}", bundle.file));
        fetch::download_verified(&bundle.url, &tmp, &bundle.sha1, bundle.size, |a, b| {
            (progress.on_bytes)(a, b)
        })
        .context("downloading the overrides bundle")?;
        let res = extract_bundle(&tmp, root);
        let _ = fs::remove_file(&tmp);
        res?;
    }

    for orphan in &p.orphans {
        fs::remove_file(orphan)
            .with_context(|| format!("removing {}", orphan.display()))?;
    }

    Ok(())
}

/// Unpacks `overrides.zip`, honouring the enforced/seed split.
///
/// Entry names come off the network like everything else, so each one goes through the
/// same path check as the manifest before it is joined to the root.
pub fn extract_bundle(zip_path: &Path, root: &Path) -> Result<(usize, usize)> {
    let file = fs::File::open(zip_path)
        .with_context(|| format!("opening {}", zip_path.display()))?;
    let mut zip = zip::ZipArchive::new(file).context("overrides.zip is not a valid zip")?;

    let (mut written, mut kept) = (0usize, 0usize);
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i)?;
        if entry.is_dir() {
            continue;
        }
        let name = entry.name().to_string();
        let rel = match safe_relative(&name) {
            Ok(r) => r,
            Err(e) => {
                // refuse the file rather than the whole sync, but say so loudly
                eprintln!("  ! skipping {name}: {e}");
                continue;
            }
        };
        let dest = root.join(&rel);

        if policy_for(&name) == Policy::Seed && dest.exists() {
            kept += 1;
            continue;
        }

        let mut buf = Vec::with_capacity(entry.size() as usize);
        std::io::copy(&mut entry, &mut buf)?;
        fetch::write_atomic(&dest, &buf)
            .with_context(|| format!("writing {}", dest.display()))?;
        written += 1;
    }
    Ok((written, kept))
}

/// A full hash check of every file, for `--verify`. Returns the paths that do not match.
pub fn verify(m: &Manifest, root: &Path) -> Result<Vec<String>> {
    let mut bad = Vec::new();
    for f in &m.files {
        let dest = resolve(root, &f.path)?;
        if !dest.is_file() {
            bad.push(format!("{} (missing)", f.path));
            continue;
        }
        let got = sha1_of_file(&dest)?;
        if !got.eq_ignore_ascii_case(&f.sha1) {
            let note = match policy_for(&f.path) {
                Policy::Seed => "edited, allowed",
                Policy::Enforced => "does not match",
            };
            bad.push(format!("{} ({note})", f.path));
        }
    }
    Ok(bad)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest_with(path: &str, sha1: &str, source: &str) -> Manifest {
        let json = format!(
            r#"{{"name":"t","version":"1","minecraft":"1.21.1",
                 "bundle":{{"file":"overrides.zip","size":1,"sha1":"{h}","url":"https://e/o.zip","count":1}},
                 "files":[{{"path":"{path}","size":0,"sha1":"{sha1}","url":"https://example.invalid/f","source":"{source}"}}]}}"#,
            h = "0".repeat(40)
        );
        Manifest::parse(json.as_bytes()).unwrap()
    }

    #[test]
    fn an_edited_seed_file_does_not_pull_the_bundle() {
        let dir = std::env::temp_dir().join(format!("lotm-test-{}", std::process::id()));
        let cfg = dir.join("config");
        fs::create_dir_all(&cfg).unwrap();
        fs::write(cfg.join("mod-client.toml"), b"player edited this").unwrap();

        let m = manifest_with("config/mod-client.toml", &"a".repeat(40), "bundle");
        let p = plan(&m, &dir).unwrap();
        assert!(!p.need_bundle, "a seed file that exists should be left alone");

        fs::remove_file(cfg.join("mod-client.toml")).unwrap();
        let p = plan(&m, &dir).unwrap();
        assert!(p.need_bundle, "a missing seed file should be restored");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn orphans_are_only_found_in_managed_folders() {
        let dir = std::env::temp_dir().join(format!("lotm-orph-{}", std::process::id()));
        fs::create_dir_all(dir.join("mods")).unwrap();
        fs::create_dir_all(dir.join("saves")).unwrap();
        fs::write(dir.join("mods/stale.jar"), b"x").unwrap();
        fs::write(dir.join("saves/level.dat"), b"x").unwrap();

        let m = manifest_with("mods/wanted.jar", &"b".repeat(40), "curseforge");
        let orphans = find_orphans(&m, &dir).unwrap();
        assert_eq!(orphans, vec![dir.join("mods/stale.jar")]);

        let _ = fs::remove_dir_all(&dir);
    }
}
