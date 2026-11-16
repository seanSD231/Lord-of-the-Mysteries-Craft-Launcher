//! Installing vanilla Minecraft from Mojang's own metadata.
//!
//! Three things have to be on disk before the game will start: `client.jar`, the set of
//! libraries that apply to this platform, and the assets (sounds, language files,
//! textures that live outside the jar). Everything is content-addressed by SHA-1, so a
//! file that is already correct is never fetched twice, and a bad one is caught.
//!
//! Layout follows the usual Minecraft shape so the files are recognisable and could be
//! shared with another launcher later:
//!
//! ```text
//! .lotmcraft/
//!   versions/1.21.1/1.21.1.jar, 1.21.1.json
//!   libraries/<maven path>/...
//!   assets/indexes/<id>.json
//!   assets/objects/<first two chars>/<sha1>
//! ```

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::fetch;

const VERSION_MANIFEST: &str = "https://piston-meta.mojang.com/mc/game/version_manifest_v2.json";
const RESOURCES: &str = "https://resources.download.minecraft.net";

#[derive(Deserialize)]
struct VersionManifest {
    versions: Vec<VersionStub>,
}

#[derive(Deserialize)]
struct VersionStub {
    id: String,
    url: String,
    #[serde(default)]
    sha1: String,
}

#[derive(Deserialize)]
pub struct VersionJson {
    pub id: String,
    #[serde(rename = "mainClass")]
    pub main_class: String,
    #[serde(rename = "assetIndex")]
    pub asset_index: AssetIndexRef,
    pub downloads: Downloads,
    pub libraries: Vec<Library>,
    #[serde(default)]
    pub arguments: Option<serde_json::Value>,
    #[serde(default, rename = "minecraftArguments")]
    pub legacy_arguments: Option<String>,
}

#[derive(Deserialize)]
pub struct AssetIndexRef {
    pub id: String,
    pub url: String,
    pub sha1: String,
    #[serde(default)]
    pub size: u64,
}

#[derive(Deserialize)]
pub struct Downloads {
    pub client: Artifact,
}

#[derive(Deserialize, Clone)]
pub struct Artifact {
    pub url: String,
    pub sha1: String,
    #[serde(default)]
    pub size: u64,
    /// Present on library artifacts: the path under `libraries/`.
    #[serde(default)]
    pub path: Option<String>,
}

#[derive(Deserialize)]
pub struct Library {
    pub name: String,
    #[serde(default)]
    pub downloads: Option<LibraryDownloads>,
    #[serde(default)]
    pub rules: Vec<Rule>,
    /// Older format: maps an OS to a key in `classifiers`.
    #[serde(default)]
    pub natives: Option<BTreeMap<String, String>>,
}

#[derive(Deserialize)]
pub struct LibraryDownloads {
    #[serde(default)]
    pub artifact: Option<Artifact>,
    #[serde(default)]
    pub classifiers: Option<BTreeMap<String, Artifact>>,
}

#[derive(Deserialize)]
pub struct Rule {
    pub action: String,
    #[serde(default)]
    pub os: Option<OsRule>,
    /// `is_demo`, `has_custom_resolution` and friends. None of them are set here, so a
    /// rule carrying features never applies.
    #[serde(default)]
    pub features: Option<serde_json::Value>,
}

#[derive(Deserialize)]
pub struct OsRule {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub arch: Option<String>,
}

/// Mojang's name for the running platform.
pub fn os_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "windows"
    } else if cfg!(target_os = "macos") {
        "osx"
    } else {
        "linux"
    }
}

fn arch_name() -> &'static str {
    if cfg!(target_arch = "aarch64") {
        "arm64"
    } else if cfg!(target_arch = "x86") {
        "x86"
    } else {
        "x86_64"
    }
}

/// Mojang's rules are evaluated in order and the last match wins, which is why this is
/// not simply "any allow and no disallow".
pub fn rules_allow(rules: &[Rule]) -> bool {
    if rules.is_empty() {
        return true;
    }
    let mut allowed = false;
    for r in rules {
        if r.features.is_some() {
            continue; // no features are enabled, so the rule cannot match
        }
        let matches = match &r.os {
            None => true,
            Some(os) => {
                os.name.as_deref().map(|n| n == os_name()).unwrap_or(true)
                    && os.arch.as_deref().map(|a| a == arch_name()).unwrap_or(true)
            }
        };
        if matches {
            allowed = r.action == "allow";
        }
    }
    allowed
}

pub struct Installed {
    pub version_json: PathBuf,
    pub client_jar: PathBuf,
    pub libraries: Vec<PathBuf>,
    pub natives: Vec<PathBuf>,
    pub assets_root: PathBuf,
    pub asset_index_id: String,
    pub main_class: String,
}

/// Where everything lives under the launcher's data folder.
pub struct Paths {
    pub base: PathBuf,
}

impl Paths {
    pub fn new(base: &Path) -> Self {
        Self {
            base: base.to_path_buf(),
        }
    }
    pub fn version_dir(&self, id: &str) -> PathBuf {
        self.base.join("versions").join(id)
    }
    pub fn libraries(&self) -> PathBuf {
        self.base.join("libraries")
    }
    pub fn assets(&self) -> PathBuf {
        self.base.join("assets")
    }
}

pub fn install(
    base: &Path,
    mc_version: &str,
    mut on_step: impl FnMut(&str),
    mut on_progress: impl FnMut(u64, u64),
) -> Result<Installed> {
    let paths = Paths::new(base);

    on_step("fetching the version manifest");
    let body = fetch::get_bytes(VERSION_MANIFEST, 8 << 20)
        .context("fetching Mojang's version manifest")?;
    let vm: VersionManifest = serde_json::from_slice(&body)?;
    let stub = vm
        .versions
        .into_iter()
        .find(|v| v.id == mc_version)
        .ok_or_else(|| anyhow!("Mojang does not list Minecraft {mc_version}"))?;

    // the version json, cached by its own hash
    let vdir = paths.version_dir(&stub.id);
    std::fs::create_dir_all(&vdir)?;
    let vjson_path = vdir.join(format!("{}.json", stub.id));
    let vjson_bytes = cached(&vjson_path, &stub.sha1, || {
        fetch::get_bytes(&stub.url, 8 << 20)
    })
    .with_context(|| format!("fetching the version json for {mc_version}"))?;
    let v: VersionJson = serde_json::from_slice(&vjson_bytes)
        .with_context(|| format!("parsing the version json for {mc_version}"))?;

    // client.jar
    on_step("downloading client.jar");
    let client_jar = vdir.join(format!("{}.jar", v.id));
    fetch::download_if_needed(
        &v.downloads.client.url,
        &client_jar,
        &v.downloads.client.sha1,
        v.downloads.client.size,
        &mut on_progress,
    )
    .context("downloading client.jar")?;

    // libraries, and the native ones that have to be unpacked beside the game
    let (lib_jobs, natives) = library_jobs(&v, &paths)?;
    if !lib_jobs.is_empty() {
        on_step(&format!("downloading {} libraries", lib_jobs.len()));
        fetch::download_all(&lib_jobs, 6, &mut on_progress)
            .context("downloading libraries")?;
    }
    let libraries = lib_jobs.iter().map(|j| j.dest.clone()).collect();

    // the asset index, then every object it names
    on_step("fetching the asset index");
    let idx_path = paths
        .assets()
        .join("indexes")
        .join(format!("{}.json", v.asset_index.id));
    let idx_bytes = cached(&idx_path, &v.asset_index.sha1, || {
        fetch::get_bytes(&v.asset_index.url, 32 << 20)
    })
    .context("fetching the asset index")?;

    let jobs = asset_jobs(&idx_bytes, &paths)?;
    if !jobs.is_empty() {
        on_step(&format!("downloading {} assets", jobs.len()));
        fetch::download_all(&jobs, 8, &mut on_progress).context("downloading assets")?;
    }

    Ok(Installed {
        version_json: vjson_path,
        client_jar,
        libraries,
        natives,
        assets_root: paths.assets(),
        asset_index_id: v.asset_index.id.clone(),
        main_class: v.main_class.clone(),
    })
}

/// Reads a cached file if its hash matches, otherwise fetches and stores it.
fn cached(path: &Path, sha1: &str, fetch_it: impl FnOnce() -> Result<Vec<u8>>) -> Result<Vec<u8>> {
    if path.is_file() {
        let bytes = std::fs::read(path)?;
        if sha1.is_empty() || fetch::sha1_of_bytes(&bytes).eq_ignore_ascii_case(sha1) {
            return Ok(bytes);
        }
    }
    let bytes = fetch_it()?;
    if !sha1.is_empty() && !fetch::sha1_of_bytes(&bytes).eq_ignore_ascii_case(sha1) {
        bail!("{} did not match its published hash", path.display());
    }
    fetch::write_atomic(path, &bytes)?;
    Ok(bytes)
}

fn library_jobs(v: &VersionJson, paths: &Paths) -> Result<(Vec<fetch::Job>, Vec<PathBuf>)> {
    let mut jobs = Vec::new();
    let mut natives = Vec::new();
    for lib in &v.libraries {
        if !rules_allow(&lib.rules) {
            continue;
        }
        let Some(dl) = &lib.downloads else { continue };

        if let Some(a) = &dl.artifact {
            let Some(rel) = &a.path else {
                bail!("{} has an artifact with no path", lib.name);
            };
            let dest = paths.libraries().join(rel);
            // 1.21 ships natives as ordinary artifacts whose name carries the platform
            if lib.name.contains(":natives-") {
                natives.push(dest.clone());
            }
            jobs.push(fetch::Job {
                url: a.url.clone(),
                dest,
                sha1: a.sha1.clone(),
                size: a.size,
            });
        }

        // older format: a separate classifier per platform
        if let (Some(map), Some(classifiers)) = (&lib.natives, &dl.classifiers) {
            if let Some(key) = map.get(os_name()) {
                let key = key.replace("${arch}", if cfg!(target_pointer_width = "64") { "64" } else { "32" });
                if let Some(a) = classifiers.get(&key) {
                    let rel = a
                        .path
                        .clone()
                        .ok_or_else(|| anyhow!("{} native has no path", lib.name))?;
                    let dest = paths.libraries().join(rel);
                    natives.push(dest.clone());
                    jobs.push(fetch::Job {
                        url: a.url.clone(),
                        dest,
                        sha1: a.sha1.clone(),
                        size: a.size,
                    });
                }
            }
        }
    }
    Ok((jobs, natives))
}

#[derive(Deserialize)]
struct AssetIndex {
    objects: BTreeMap<String, AssetObject>,
}

#[derive(Deserialize)]
struct AssetObject {
    hash: String,
    #[serde(default)]
    size: u64,
}

fn asset_jobs(index_bytes: &[u8], paths: &Paths) -> Result<Vec<fetch::Job>> {
    let idx: AssetIndex =
        serde_json::from_slice(index_bytes).context("parsing the asset index")?;
    let objects = paths.assets().join("objects");
    let mut jobs = Vec::with_capacity(idx.objects.len());
    for obj in idx.objects.values() {
        if obj.hash.len() < 2 || !obj.hash.chars().all(|c| c.is_ascii_hexdigit()) {
            bail!("the asset index contains a malformed hash");
        }
        let prefix = &obj.hash[..2];
        jobs.push(fetch::Job {
            url: format!("{RESOURCES}/{prefix}/{}", obj.hash),
            dest: objects.join(prefix).join(&obj.hash),
            sha1: obj.hash.clone(),
            size: obj.size,
        });
    }
    Ok(jobs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(action: &str, os: Option<&str>) -> Rule {
        Rule {
            action: action.to_string(),
            os: os.map(|n| OsRule {
                name: Some(n.to_string()),
                arch: None,
            }),
            features: None,
        }
    }

    #[test]
    fn no_rules_means_allowed() {
        assert!(rules_allow(&[]));
    }

    #[test]
    fn last_matching_rule_wins() {
        // allow everything, then disallow this platform
        let rules = vec![rule("allow", None), rule("disallow", Some(os_name()))];
        assert!(!rules_allow(&rules));

        // allow only this platform
        assert!(rules_allow(&[rule("allow", Some(os_name()))]));

        // allow only some other platform
        let other = if os_name() == "windows" { "linux" } else { "windows" };
        assert!(!rules_allow(&[rule("allow", Some(other))]));
    }

    #[test]
    fn feature_rules_never_match() {
        let r = Rule {
            action: "allow".into(),
            os: None,
            features: Some(serde_json::json!({"is_demo": true})),
        };
        assert!(!rules_allow(&[r]));
    }
}
