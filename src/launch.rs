//! Assembling and running the game command.
//!
//! NeoForge's version json does not stand alone: it names `inheritsFrom`, and the real
//! launch is the merge of it with the vanilla json underneath. Child libraries win over
//! parent ones with the same group:artifact, the child's mainClass wins, and the two
//! argument lists are concatenated parent-first.
//!
//! Every placeholder Mojang and NeoForge use (`${classpath}`, `${natives_directory}`,
//! `${library_directory}` and the rest) is substituted here. A missing one is left as
//! written rather than silently blanked, because a blank argument tends to produce a
//! crash a long way from its cause.

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::auth;
use crate::java::JavaRuntime;
use crate::mojang::{self, Paths};

pub const LAUNCHER_NAME: &str = "lotmcraft";

/// Who is playing.
pub enum Profile {
    /// A real Microsoft account that owns the game.
    Online(auth::Session),
    /// A local name, for testing against an offline-mode server. See `offline_allowed`.
    Offline { username: String, uuid: String },
}

impl Profile {
    fn username(&self) -> &str {
        match self {
            Profile::Online(s) => &s.username,
            Profile::Offline { username, .. } => username,
        }
    }
    fn uuid(&self) -> &str {
        match self {
            Profile::Online(s) => &s.uuid,
            Profile::Offline { uuid, .. } => uuid,
        }
    }
    fn token(&self) -> &str {
        match self {
            Profile::Online(s) => &s.access_token,
            // the server is not asked to validate this, and an offline-mode server
            // does not look at it
            Profile::Offline { .. } => "0",
        }
    }
    fn user_type(&self) -> &str {
        match self {
            Profile::Online(_) => "msa",
            Profile::Offline { .. } => "legacy",
        }
    }
}

/// The UUID an offline-mode server derives from a name: version 3, MD5 of
/// `OfflinePlayer:<name>`. Matching it means `dev1` keeps the same identity, and its
/// inventory, between sessions, and any authenticator mod sees a stable player.
pub fn offline_uuid(username: &str) -> String {
    use md5::{Digest, Md5};
    let mut h = Md5::new();
    h.update(format!("OfflinePlayer:{username}").as_bytes());
    let mut b = h.finalize();
    b[6] = (b[6] & 0x0f) | 0x30; // version 3
    b[8] = (b[8] & 0x3f) | 0x80; // RFC 4122 variant
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    hex
}

/// Whether an offline profile may be used on this installation.
///
/// The rule is that a real Microsoft sign-in must have happened here at least once: the
/// launcher is for people who own the game, and offline profiles exist so those people
/// can run several clients against their own server, not as a way around buying it.
///
/// The `dev-offline` feature waives this. It is off by default, so the binaries players
/// receive do not contain the path at all -- which is the only kind of gate that means
/// anything in a program the player is running.
pub fn offline_allowed(base: &Path) -> bool {
    if cfg!(feature = "dev-offline") {
        return true;
    }
    base.join("auth.json").is_file()
}

/// Reads a version json and everything it inherits from, nearest first.
fn chain(paths: &Paths, version_id: &str) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    let mut id = version_id.to_string();
    for _ in 0..8 {
        let p = paths.version_dir(&id).join(format!("{id}.json"));
        let bytes = std::fs::read(&p)
            .with_context(|| format!("reading {}", p.display()))?;
        let v: Value = serde_json::from_slice(&bytes)
            .with_context(|| format!("parsing {}", p.display()))?;
        let parent = v
            .get("inheritsFrom")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string());
        out.push(v);
        match parent {
            Some(p) if p != id => id = p,
            _ => return Ok(out),
        }
    }
    bail!("version json inheritance went more than eight deep; refusing to follow it")
}

/// group:artifact, ignoring the version, so a child library replaces the parent's.
fn lib_key(name: &str) -> String {
    let parts: Vec<&str> = name.split(':').collect();
    if parts.len() >= 2 {
        format!("{}:{}", parts[0], parts[1])
    } else {
        name.to_string()
    }
}

struct Merged {
    main_class: String,
    asset_index: String,
    version_id: String,
    libraries: Vec<Value>,
    jvm_args: Vec<Value>,
    game_args: Vec<Value>,
}

fn merge(chain: &[Value]) -> Result<Merged> {
    let child = chain.first().ok_or_else(|| anyhow!("no version json"))?;
    let root = chain.last().unwrap();

    let main_class = chain
        .iter()
        .find_map(|v| v.get("mainClass").and_then(|x| x.as_str()))
        .ok_or_else(|| anyhow!("no mainClass in the version chain"))?
        .to_string();

    let asset_index = chain
        .iter()
        .find_map(|v| v["assetIndex"]["id"].as_str())
        .or_else(|| root.get("assets").and_then(|x| x.as_str()))
        .ok_or_else(|| anyhow!("no asset index in the version chain"))?
        .to_string();

    let version_id = child
        .get("id")
        .and_then(|x| x.as_str())
        .unwrap_or_default()
        .to_string();

    // nearest wins, so walk child-first and keep the first of each group:artifact
    let mut seen = BTreeMap::new();
    let mut libraries = Vec::new();
    for v in chain {
        for lib in v["libraries"].as_array().into_iter().flatten() {
            let Some(name) = lib.get("name").and_then(|x| x.as_str()) else {
                continue;
            };
            let key = lib_key(name);
            if seen.contains_key(&key) {
                continue;
            }
            seen.insert(key, ());
            libraries.push(lib.clone());
        }
    }

    // arguments concatenate parent-first, so the child's additions come last
    let mut jvm_args = Vec::new();
    let mut game_args = Vec::new();
    for v in chain.iter().rev() {
        if let Some(a) = v["arguments"]["jvm"].as_array() {
            jvm_args.extend(a.iter().cloned());
        }
        if let Some(a) = v["arguments"]["game"].as_array() {
            game_args.extend(a.iter().cloned());
        }
        // pre-1.13 style, kept because --mc can point at an older version
        if let Some(s) = v.get("minecraftArguments").and_then(|x| x.as_str()) {
            game_args.extend(s.split_whitespace().map(|w| Value::String(w.to_string())));
        }
    }

    Ok(Merged {
        main_class,
        asset_index,
        version_id,
        libraries,
        jvm_args,
        game_args,
    })
}

/// Turns an argument entry into the strings it contributes, honouring its rules.
fn expand(entry: &Value, vars: &BTreeMap<String, String>) -> Vec<String> {
    match entry {
        Value::String(s) => vec![substitute(s, vars)],
        Value::Object(o) => {
            let rules: Vec<mojang::Rule> = o
                .get("rules")
                .and_then(|r| serde_json::from_value(r.clone()).ok())
                .unwrap_or_default();
            if !mojang::rules_allow(&rules) {
                return Vec::new();
            }
            match o.get("value") {
                Some(Value::String(s)) => vec![substitute(s, vars)],
                Some(Value::Array(a)) => a
                    .iter()
                    .filter_map(|x| x.as_str())
                    .map(|s| substitute(s, vars))
                    .collect(),
                _ => Vec::new(),
            }
        }
        _ => Vec::new(),
    }
}

/// Replaces `${name}` where we have a value, and leaves it alone where we do not.
fn substitute(s: &str, vars: &BTreeMap<String, String>) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' && i + 1 < bytes.len() && bytes[i + 1] == b'{' {
            if let Some(end) = s[i + 2..].find('}') {
                let name = &s[i + 2..i + 2 + end];
                match vars.get(name) {
                    Some(v) => out.push_str(v),
                    // unknown placeholder: keep it, so the cause is visible in the
                    // command line rather than appearing as an empty argument
                    None => out.push_str(&s[i..i + 3 + end]),
                }
                i += 3 + end;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

/// Unpacks native libraries next to the version, skipping signatures and directories.
fn extract_natives(natives: &[PathBuf], into: &Path) -> Result<()> {
    std::fs::create_dir_all(into)?;
    for jar in natives {
        if !jar.is_file() {
            continue;
        }
        let f = std::fs::File::open(jar)?;
        let Ok(mut zip) = zip::ZipArchive::new(f) else {
            continue; // not a zip: nothing to unpack
        };
        for i in 0..zip.len() {
            let mut e = zip.by_index(i)?;
            let Some(rel) = e.enclosed_name() else { continue };
            let name = rel.to_string_lossy().to_string();
            if e.is_dir() || name.starts_with("META-INF/") {
                continue;
            }
            // natives are flat: only the file name matters
            let Some(file_name) = rel.file_name() else {
                continue;
            };
            let out = into.join(file_name);
            if out.exists() {
                continue;
            }
            let mut buf = Vec::new();
            std::io::copy(&mut e, &mut buf)?;
            crate::fetch::write_atomic(&out, &buf)?;
        }
    }
    Ok(())
}

pub struct LaunchOptions<'a> {
    pub game_dir: &'a Path,
    pub base: &'a Path,
    pub version_id: &'a str,
    pub java: &'a JavaRuntime,
    pub profile: &'a Profile,
    pub memory_mb: u32,
    pub extra_jvm: Vec<String>,
    /// Join this server straight from the menu, as `host:port`.
    pub server: Option<String>,
}

/// Builds the full command without running it, so it can be printed and inspected.
pub fn build_command(opts: &LaunchOptions, natives: &[PathBuf]) -> Result<Command> {
    let paths = Paths::new(opts.base);
    let chain = chain(&paths, opts.version_id)?;
    let m = merge(&chain)?;

    let sep = if cfg!(windows) { ";" } else { ":" };
    let natives_dir = paths.version_dir(&m.version_id).join("natives");
    extract_natives(natives, &natives_dir).context("unpacking native libraries")?;

    // classpath: every applicable library, then the vanilla client jar.
    // Filtered below: anything NeoForge puts on the module path must not also be here.
    let mut cp: Vec<String> = Vec::new();
    for lib in &m.libraries {
        let rules: Vec<mojang::Rule> = lib
            .get("rules")
            .and_then(|r| serde_json::from_value(r.clone()).ok())
            .unwrap_or_default();
        if !mojang::rules_allow(&rules) {
            continue;
        }
        let name = lib["name"].as_str().unwrap_or_default();
        if name.contains(":natives-") {
            continue; // unpacked above, not on the classpath
        }
        let rel = lib["downloads"]["artifact"]["path"]
            .as_str()
            .map(|s| s.to_string())
            .or_else(|| maven_path(name));
        if let Some(rel) = rel {
            let p = paths.libraries().join(rel);
            if p.is_file() {
                cp.push(p.to_string_lossy().to_string());
            }
        }
    }
    let root_id = chain
        .last()
        .and_then(|v| v.get("id"))
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    let client_jar = paths.version_dir(&root_id).join(format!("{root_id}.jar"));
    if client_jar.is_file() {
        cp.push(client_jar.to_string_lossy().to_string());
    }

    let mut vars: BTreeMap<String, String> = BTreeMap::new();
    vars.insert("auth_player_name".into(), opts.profile.username().into());
    vars.insert("auth_uuid".into(), opts.profile.uuid().into());
    vars.insert("auth_access_token".into(), opts.profile.token().into());
    vars.insert("auth_xuid".into(), String::new());
    vars.insert("clientid".into(), String::new());
    vars.insert("user_type".into(), opts.profile.user_type().into());
    vars.insert("version_name".into(), m.version_id.clone());
    vars.insert("version_type".into(), "release".into());
    vars.insert(
        "game_directory".into(),
        opts.game_dir.to_string_lossy().to_string(),
    );
    vars.insert(
        "assets_root".into(),
        paths.assets().to_string_lossy().to_string(),
    );
    vars.insert(
        "game_assets".into(),
        paths.assets().to_string_lossy().to_string(),
    );
    vars.insert("assets_index_name".into(), m.asset_index.clone());
    vars.insert(
        "natives_directory".into(),
        natives_dir.to_string_lossy().to_string(),
    );
    vars.insert(
        "library_directory".into(),
        paths.libraries().to_string_lossy().to_string(),
    );
    vars.insert("classpath_separator".into(), sep.into());
    vars.insert("launcher_name".into(), LAUNCHER_NAME.into());
    vars.insert("launcher_version".into(), env!("CARGO_PKG_VERSION").into());

    // Expand the jvm arguments before the classpath exists, because they are what say
    // which jars go on the module path. ${classpath} has no value yet and is left as
    // written, to be filled in once the filtering is done.
    let jvm_raw: Vec<String> = m.jvm_args.iter().flat_map(|e| expand(e, &vars)).collect();

    // A jar on the module path must not also be on the class path: the JVM rejects it
    // with "module X was already on the module path but class-path contains it".
    let on_module_path = module_path_entries(&jvm_raw, sep);
    if !on_module_path.is_empty() {
        cp.retain(|entry| {
            Path::new(entry)
                .file_name()
                .map(|n| !on_module_path.contains(&n.to_string_lossy().to_string()))
                .unwrap_or(true)
        });
    }
    vars.insert("classpath".into(), cp.join(sep));

    let mut cmd = Command::new(&opts.java.java);
    cmd.arg(format!("-Xmx{}M", opts.memory_mb));
    cmd.arg("-Xss1M");
    for a in &opts.extra_jvm {
        cmd.arg(a);
    }

    let mut jvm: Vec<String> = jvm_raw
        .iter()
        .map(|a| substitute(a, &vars))
        .collect();
    if jvm.is_empty() {
        // a version json with no jvm arguments at all predates 1.13
        jvm.push(format!("-Djava.library.path={}", vars["natives_directory"]));
        jvm.push("-cp".into());
        jvm.push(vars["classpath"].clone());
    }
    for a in jvm {
        cmd.arg(a);
    }

    cmd.arg(&m.main_class);
    for a in m.game_args.iter().flat_map(|e| expand(e, &vars)) {
        cmd.arg(a);
    }
    if let Some(s) = &opts.server {
        let (host, port) = match s.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), p.to_string()),
            None => (s.clone(), "25565".to_string()),
        };
        // 1.20+ takes one argument; older takes two. --quickPlayMultiplayer is the
        // current one and is ignored harmlessly by anything that does not know it.
        cmd.arg("--quickPlayMultiplayer").arg(format!("{host}:{port}"));
    }

    cmd.current_dir(opts.game_dir);
    cmd.stdout(Stdio::inherit()).stderr(Stdio::inherit());
    Ok(cmd)
}

/// The file names listed after `-p` / `--module-path` in the expanded jvm arguments.
///
/// NeoForge builds that value from ${library_directory}, so it is already a real list of
/// paths by this point. Matching on file name rather than full path avoids separator and
/// case differences on Windows.
fn module_path_entries(jvm: &[String], sep: &str) -> std::collections::BTreeSet<String> {
    let mut out = std::collections::BTreeSet::new();
    let mut i = 0;
    while i < jvm.len() {
        let a = jvm[i].as_str();
        let value = if a == "-p" || a == "--module-path" {
            i += 1;
            jvm.get(i).map(|s| s.as_str())
        } else if let Some(rest) = a.strip_prefix("--module-path=") {
            Some(rest)
        } else {
            None
        };
        if let Some(v) = value {
            for part in v.split(sep) {
                if let Some(name) = Path::new(part).file_name() {
                    out.insert(name.to_string_lossy().to_string());
                }
            }
        }
        i += 1;
    }
    out
}

/// `group:artifact:version[:classifier]` to the path Maven would store it at.
fn maven_path(name: &str) -> Option<String> {
    let parts: Vec<&str> = name.split(':').collect();
    if parts.len() < 3 {
        return None;
    }
    let (group, artifact, version) = (parts[0], parts[1], parts[2]);
    let classifier = parts.get(3).map(|c| format!("-{c}")).unwrap_or_default();
    Some(format!(
        "{}/{artifact}/{version}/{artifact}-{version}{classifier}.jar",
        group.replace('.', "/")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offline_uuid_is_stable_and_version_3() {
        let a = offline_uuid("dev1");
        assert_eq!(a, offline_uuid("dev1"));
        assert_ne!(a, offline_uuid("dev2"));
        assert_eq!(a.len(), 32);
        // the version nibble lives at the 13th hex character
        assert_eq!(&a[12..13], "3");
    }

    #[test]
    fn unknown_placeholders_are_left_visible() {
        let mut vars = BTreeMap::new();
        vars.insert("known".to_string(), "yes".to_string());
        assert_eq!(substitute("a ${known} b", &vars), "a yes b");
        assert_eq!(substitute("a ${missing} b", &vars), "a ${missing} b");
    }

    #[test]
    fn child_libraries_replace_parent_ones() {
        let child = serde_json::json!({
            "id": "neoforge-21.1.264",
            "inheritsFrom": "1.21.1",
            "mainClass": "cpw.mods.bootstraplauncher.BootstrapLauncher",
            "libraries": [{"name": "com.google.guava:guava:33.0.0"}]
        });
        let parent = serde_json::json!({
            "id": "1.21.1",
            "mainClass": "net.minecraft.client.main.Main",
            "assetIndex": {"id": "17"},
            "libraries": [
                {"name": "com.google.guava:guava:21.0.0"},
                {"name": "org.lwjgl:lwjgl:3.3.3"}
            ]
        });
        let m = merge(&[child, parent]).unwrap();
        assert_eq!(m.main_class, "cpw.mods.bootstraplauncher.BootstrapLauncher");
        assert_eq!(m.asset_index, "17");
        assert_eq!(m.libraries.len(), 2);
        assert_eq!(m.libraries[0]["name"], "com.google.guava:guava:33.0.0");
    }

    #[test]
    fn module_path_entries_are_found_in_both_forms() {
        let sep = ";";
        let jvm = vec![
            "-p".to_string(),
            "C:/l/bootstraplauncher-2.0.2.jar;C:/l/securejarhandler-3.0.8.jar".to_string(),
            "--module-path=C:/l/asm-9.7.jar".to_string(),
        ];
        let set = module_path_entries(&jvm, sep);
        assert!(set.contains("bootstraplauncher-2.0.2.jar"));
        assert!(set.contains("securejarhandler-3.0.8.jar"));
        assert!(set.contains("asm-9.7.jar"));
        assert_eq!(set.len(), 3);
    }

    #[test]
    fn maven_paths_include_classifiers() {
        assert_eq!(
            maven_path("org.lwjgl:lwjgl:3.3.3").unwrap(),
            "org/lwjgl/lwjgl/3.3.3/lwjgl-3.3.3.jar"
        );
        assert_eq!(
            maven_path("org.lwjgl:lwjgl:3.3.3:natives-windows").unwrap(),
            "org/lwjgl/lwjgl/3.3.3/lwjgl-3.3.3-natives-windows.jar"
        );
    }
}
