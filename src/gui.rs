//! The window.
//!
//! Thin by design: every button calls the same functions the command line does, so the
//! two cannot drift apart. The long-running work happens on a worker thread and reports
//! back through events, because a Tauri command that blocks also freezes the webview.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use anyhow::{Context, Result};
use lotmcraft::{auth, fetch, java, launch, manifest::Manifest, mojang, neoforge, ping, sync};
use serde::Serialize;
use std::path::PathBuf;
use std::sync::Mutex;
use tauri::{AppHandle, Emitter, Manager, State};

/// Where the pack manifest is read from. Baked in so a player configures nothing.
const MANIFEST_URL: &str = concat!(
    "https://github.com/seanSD231/Lord-of-the-Mysteries-Craft-Launcher",
    "/releases/latest/download/manifest.json"
);
const MANIFEST_LIMIT: usize = 8 << 20;

struct Ctx {
    root: PathBuf,
    /// Cached so the window does not refetch on every button.
    manifest: Mutex<Option<Manifest>>,
}

impl Ctx {
    fn base(&self) -> PathBuf {
        self.root.join(".lotmcraft")
    }

    /// Reads the manifest, preferring the override a tester may have set.
    fn manifest(&self) -> Result<Manifest> {
        let src = std::env::var("LOTMCRAFT_MANIFEST").unwrap_or_else(|_| MANIFEST_URL.to_string());
        let bytes = if src.starts_with("http") {
            fetch::get_bytes(&src, MANIFEST_LIMIT).context("fetching the pack manifest")?
        } else {
            std::fs::read(&src).with_context(|| format!("reading {src}"))?
        };
        Manifest::parse(&bytes)
    }
}

#[derive(Serialize)]
struct Status {
    pack_name: String,
    pack_version: String,
    minecraft: String,
    neoforge: String,
    file_count: usize,
    root: String,
    installed: bool,
    offline_allowed: bool,
    username: Option<String>,
}

/// Errors cross into JavaScript as strings; anyhow's chain is what makes them useful.
fn fail(e: anyhow::Error) -> String {
    let mut s = e.to_string();
    for cause in e.chain().skip(1) {
        s.push_str(&format!("\n  caused by: {cause}"));
    }
    s
}

#[tauri::command]
fn status(ctx: State<'_, Ctx>) -> Result<Status, String> {
    let m = match ctx.manifest.lock() {
        Ok(mut slot) => {
            if slot.is_none() {
                *slot = Some(ctx.manifest().map_err(fail)?);
            }
            // cheap enough to clone the few fields we need
            let m = slot.as_ref().unwrap();
            Status {
                pack_name: m.name.clone(),
                pack_version: m.version.clone(),
                minecraft: m.minecraft.clone(),
                neoforge: m.neoforge.clone(),
                file_count: m.files.len(),
                root: ctx.root.display().to_string(),
                installed: ctx.base().join("versions").is_dir(),
                offline_allowed: launch::offline_allowed(&ctx.base()),
                username: saved_username(&ctx.base()),
            }
        }
        Err(_) => return Err("internal state was poisoned; restart the launcher".into()),
    };
    Ok(m)
}

/// The name shown on the account card. Absent until a sign-in has happened.
fn saved_username(base: &std::path::Path) -> Option<String> {
    let bytes = std::fs::read(base.join("profile.json")).ok()?;
    let v: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    v["username"].as_str().map(|s| s.to_string())
}

fn save_username(base: &std::path::Path, name: &str) {
    let v = serde_json::json!({ "username": name });
    let _ = fetch::write_atomic(&base.join("profile.json"), v.to_string().as_bytes());
}

#[tauri::command]
async fn login(app: AppHandle, ctx: State<'_, Ctx>) -> Result<(), String> {
    let base = ctx.base();
    let a = app.clone();
    let b = base.clone();
    // blocking work off the UI thread, or the window stops painting mid sign-in
    tauri::async_runtime::spawn_blocking(move || {
        auth::sign_in(
            &b,
            |url, code| {
                let _ = a.emit(
                    "device_code",
                    serde_json::json!({ "url": url, "code": code }),
                );
            },
            |st| {
                let _ = a.emit("status", st);
            },
        )
    })
    .await
    .map_err(|e| e.to_string())?
    .map(|s| save_username(&base, &s.username))
    .map_err(fail)
}

#[tauri::command]
fn logout(ctx: State<'_, Ctx>) -> Result<(), String> {
    let base = ctx.base();
    auth::sign_out(&base).map_err(fail)?;
    let _ = std::fs::remove_file(base.join("profile.json"));
    Ok(())
}

/// The server the pack is for. Shown on the front panel so a player can see whether it
/// is up before they spend two minutes launching.
const SERVER: &str = "37.114.51.51:25599";

#[tauri::command]
async fn server_status() -> Result<ping::ServerStatus, String> {
    tauri::async_runtime::spawn_blocking(|| ping::status(SERVER))
        .await
        .map_err(|e| e.to_string())
}

/// The pathway icons, read out of whichever jar in mods/ carries them.
///
/// They are the mod's own art, so they are not copied into this repository; the launcher
/// reads them at runtime from the jar the player has already downloaded. If the pack is
/// not installed yet there are none, and the window falls back to plain swatches.
#[tauri::command]
fn pathway_icons(ctx: State<'_, Ctx>) -> std::collections::BTreeMap<String, String> {
    const DIR: &str = "assets/lotm/textures/pathway/";
    let mut out = std::collections::BTreeMap::new();
    let Ok(entries) = std::fs::read_dir(ctx.root.join("mods")) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().map(|e| e != "jar").unwrap_or(true) {
            continue;
        }
        let Ok(file) = std::fs::File::open(&path) else { continue };
        let Ok(mut zip) = zip::ZipArchive::new(file) else { continue };
        let names: Vec<String> = (0..zip.len())
            .filter_map(|i| zip.name_for_index(i).map(|n| n.to_string()))
            .filter(|n| n.starts_with(DIR) && n.ends_with(".png"))
            .collect();
        if names.is_empty() {
            continue;
        }
        for name in names {
            let Ok(mut e) = zip.by_name(&name) else { continue };
            let mut buf = Vec::new();
            if std::io::copy(&mut e, &mut buf).is_err() {
                continue;
            }
            let id = name
                .trim_start_matches(DIR)
                .trim_end_matches(".png")
                .to_string();
            out.insert(id, format!("data:image/png;base64,{}", b64(&buf)));
        }
        if !out.is_empty() {
            break; // the first jar that has them is the one
        }
    }
    out
}

/// Small enough not to warrant a dependency.
fn b64(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(T[(n >> 18 & 63) as usize] as char);
        out.push(T[(n >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 { T[(n >> 6 & 63) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { T[(n & 63) as usize] as char } else { '=' });
    }
    out
}

/// Remembers window-side choices, such as the pathway whose colours are used.
#[tauri::command]
fn save_prefs(ctx: State<'_, Ctx>, prefs: serde_json::Value) -> Result<(), String> {
    fetch::write_atomic(&ctx.base().join("ui.json"), prefs.to_string().as_bytes())
        .map_err(fail)
}

#[tauri::command]
fn load_prefs(ctx: State<'_, Ctx>) -> serde_json::Value {
    std::fs::read(ctx.base().join("ui.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_else(|| serde_json::json!({}))
}

#[tauri::command]
fn quit(app: AppHandle) {
    app.exit(0);
}

#[tauri::command]
fn open_folder(ctx: State<'_, Ctx>) -> Result<(), String> {
    let p = &ctx.root;
    let cmd = if cfg!(windows) {
        "explorer"
    } else if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    std::process::Command::new(cmd)
        .arg(p)
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn verify_files(ctx: State<'_, Ctx>) -> Result<Vec<String>, String> {
    let root = ctx.root.clone();
    let m = ctx.manifest().map_err(fail)?;
    tauri::async_runtime::spawn_blocking(move || sync::verify(&m, &root))
        .await
        .map_err(|e| e.to_string())?
        .map_err(fail)
}

#[tauri::command]
async fn play(
    app: AppHandle,
    ctx: State<'_, Ctx>,
    memory_mb: u32,
    offline_name: Option<String>,
) -> Result<(), String> {
    let root = ctx.root.clone();
    let base = ctx.base();
    let m = ctx.manifest().map_err(fail)?;

    tauri::async_runtime::spawn_blocking(move || -> Result<()> {
        let say = |s: &str| {
            let _ = app.emit("status", s);
        };
        let tick = |done: u64, total: u64| {
            let _ = app.emit(
                "progress",
                serde_json::json!({ "done": done, "total": total }),
            );
        };

        say("checking the Java runtime");
        let rt = java::ensure(&base.join("runtime"), tick)?;

        say(&format!("installing Minecraft {}", m.minecraft));
        let game = mojang::install(&base, &m.minecraft, |s| say(s), tick)?;

        say(&format!("installing NeoForge {}", m.neoforge));
        let nf = neoforge::install(&base, &rt, &m.neoforge, |s| say(s), tick)?;

        say("checking the pack files");
        let plan = sync::plan(&m, &root)?;
        if !plan.is_empty() {
            let total = plan.download.len().max(1);
            let mut on_file = |path: &str, i: usize, _n: usize| {
                say(&format!("downloading {path}"));
                tick(i as u64, total as u64);
            };
            let mut on_bytes = |_: u64, _: u64| {};
            let mut progress = sync::Progress {
                on_file: &mut on_file,
                on_bytes: &mut on_bytes,
            };
            sync::apply(&m, &root, &plan, &mut progress)?;
        }
        tick(1, 1);

        let profile = match offline_name.as_deref().filter(|n| !n.is_empty()) {
            Some(name) if launch::offline_allowed(&base) => launch::Profile::Offline {
                username: name.to_string(),
                uuid: launch::offline_uuid(name),
            },
            _ => {
                say("signing in");
                let app2 = app.clone();
                let s = auth::sign_in(
                    &base,
                    |url, code| {
                        let _ = app2.emit(
                            "device_code",
                            serde_json::json!({ "url": url, "code": code }),
                        );
                    },
                    |st| say(st),
                )?;
                save_username(&base, &s.username);
                launch::Profile::Online(s)
            }
        };

        say("starting the game");
        let opts = launch::LaunchOptions {
            game_dir: &root,
            base: &base,
            version_id: &nf.version_id,
            java: &rt,
            profile: &profile,
            memory_mb,
            extra_jvm: Vec::new(),
            server: Some(SERVER.to_string()),
        };
        let mut cmd = launch::build_command(&opts, &game.natives)?;
        // spawn rather than wait: the launcher should not hang around blocking on the
        // game, and the player may want to close the window
        cmd.spawn().context("starting the game")?;
        Ok(())
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(fail)
}

fn main() {
    tauri::Builder::default()
        .setup(|app| {
            // the instance folder is where the launcher lives, so a player can move the
            // whole thing by moving one folder
            let root = std::env::var("LOTMCRAFT_ROOT")
                .map(PathBuf::from)
                .ok()
                .or_else(|| {
                    std::env::current_exe()
                        .ok()
                        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
                })
                .unwrap_or_else(|| PathBuf::from("."));
            app.manage(Ctx {
                root,
                manifest: Mutex::new(None),
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            status,
            login,
            logout,
            play,
            verify_files,
            open_folder,
            quit,
            server_status,
            save_prefs,
            load_prefs,
            pathway_icons
        ])
        .run(tauri::generate_context!())
        .expect("the launcher window could not start");
}
