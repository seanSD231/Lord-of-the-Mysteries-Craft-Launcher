//! Command-line front end for the sync core.
//!
//! The eventual Tauri shell calls the same functions; this exists so the machinery can be
//! exercised and debugged without a UI in the way, and so a broken install can be fixed
//! over a support chat with one command.

use lotmcraft::{auth, fetch, java, launch, manifest, mojang, neoforge, sync};

use anyhow::{bail, Context, Result};
use manifest::Manifest;
use std::io::Write;
use std::path::PathBuf;

const MANIFEST_LIMIT: usize = 8 << 20; // 8 MiB; the real one is ~190 KB

fn usage() -> ! {
    eprintln!(
        "\
lotmcraft {version}

  lotmcraft sync   --root <dir> --manifest <url|file> [--dry-run] [--yes]
  lotmcraft verify --root <dir> --manifest <url|file>
  lotmcraft java   --root <dir>
  lotmcraft game   --root <dir> [--mc <version>]
  lotmcraft install --root <dir> --manifest <url|file>
  lotmcraft login  --root <dir>
  lotmcraft logout --root <dir>
  lotmcraft launch --root <dir> --manifest <url|file> [--username <name>]
                   [--memory <MB>] [--server <host:port>] [--print]

  --root      the instance folder, the one holding mods/
  --manifest  https://... or a local manifest.json
  --dry-run   say what would change, touch nothing
  --yes       do not ask before deleting files
",
        version = env!("CARGO_PKG_VERSION")
    );
    std::process::exit(2)
}

struct Args {
    cmd: String,
    root: PathBuf,
    manifest: String,
    dry_run: bool,
    yes: bool,
    mc: Option<String>,
    username: Option<String>,
    server: Option<String>,
    memory: Option<u32>,
    print_only: bool,
}

fn parse_args() -> Args {
    let mut it = std::env::args().skip(1);
    let cmd = it.next().unwrap_or_else(|| usage());
    let (mut root, mut manifest) = (None, None);
    let (mut dry_run, mut yes) = (false, false);
    let mut mc: Option<String> = None;
    let mut username: Option<String> = None;
    let mut server: Option<String> = None;
    let mut memory: Option<u32> = None;
    let mut print_only = false;
    while let Some(a) = it.next() {
        match a.as_str() {
            "--root" => root = it.next(),
            "--manifest" => manifest = it.next(),
            "--dry-run" => dry_run = true,
            "--yes" | "-y" => yes = true,
            "--mc" => mc = it.next(),
            "--username" => username = it.next(),
            "--memory" => memory = it.next().and_then(|v| v.parse().ok()),
            "--server" => server = it.next(),
            "--print" => print_only = true,
            "-h" | "--help" => usage(),
            other => {
                eprintln!("unknown argument: {other}");
                usage()
            }
        }
    }
    let needs_manifest = !matches!(cmd.as_str(), "java" | "game" | "login" | "logout");
    match (root, manifest) {
        (Some(r), m) if m.is_some() || !needs_manifest => Args {
            cmd,
            root: PathBuf::from(r),
            manifest: m.unwrap_or_default(),
            dry_run,
            yes,
            mc,
            username,
            server,
            memory,
            print_only,
        },
        _ => usage(),
    }
}

fn load_manifest(src: &str) -> Result<Manifest> {
    let bytes = if src.starts_with("http://") || src.starts_with("https://") {
        fetch::get_bytes(src, MANIFEST_LIMIT).context("fetching the manifest")?
    } else {
        std::fs::read(src).with_context(|| format!("reading {src}"))?
    };
    Manifest::parse(&bytes)
}

fn human(bytes: u64) -> String {
    const U: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{} {}", bytes, U[0])
    } else {
        format!("{v:.1} {}", U[i])
    }
}

fn confirm(question: &str) -> Result<bool> {
    print!("{question} [y/N] ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes"))
}

fn main() -> Result<()> {
    let args = parse_args();
    if !args.root.is_dir() {
        bail!("{} is not a folder", args.root.display());
    }

    if args.cmd == "logout" {
        auth::sign_out(&args.root.join(".lotmcraft"))?;
        println!("signed out");
        return Ok(());
    }

    if args.cmd == "login" {
        let base = args.root.join(".lotmcraft");
        let s = auth::sign_in(
            &base,
            |url, code| {
                println!("\n  Open {url}");
                println!("  and enter the code:  {code}\n");
                println!("  waiting for you to finish signing in...");
            },
            |status| println!("  {status}"),
        )?;
        println!("\nsigned in as {} ({})", s.username, s.uuid);
        return Ok(());
    }

    if args.cmd == "java" {
        let dir = args.root.join(".lotmcraft").join("runtime");
        println!("looking for a Java 21 runtime under {}", dir.display());
        let mut last = 0u64;
        let rt = java::ensure(&dir, |done, total| {
            // only report each whole percent, or the console scrolls forever
            let pct = (done * 100).checked_div(total).unwrap_or(0);
            if pct != last {
                last = pct;
                print!("\r  downloading Java... {pct}%");
                let _ = std::io::stdout().flush();
            }
        })?;
        println!("\r  ready: {}            ", rt.java.display());
        println!("{}", rt.version()?);
        return Ok(());
    }

    if args.cmd == "game" {
        let base = args.root.join(".lotmcraft");
        let version = args.mc.clone().unwrap_or_else(|| "1.21.1".to_string());
        println!("installing Minecraft {version} under {}\n", base.display());
        let mut last = u64::MAX;
        let installed = mojang::install(
            &base,
            &version,
            |step| println!("{step}"),
            |done, total| {
                let pct = (done * 100).checked_div(total).unwrap_or(0);
                if pct != last {
                    last = pct;
                    print!("\r  {pct}%   ");
                    let _ = std::io::stdout().flush();
                }
            },
        )?;
        println!("\r        ");
        println!("client:    {}", installed.client_jar.display());
        println!("libraries: {}", installed.libraries.len());
        println!("natives:   {}", installed.natives.len());
        println!("assets:    index {}", installed.asset_index_id);
        println!("mainClass: {}", installed.main_class);
        return Ok(());
    }

    let m = load_manifest(&args.manifest)?;
    println!(
        "{} pack v{}  (Minecraft {}, NeoForge {})",
        m.name,
        m.version,
        m.minecraft,
        if m.neoforge.is_empty() { "-" } else { &m.neoforge }
    );
    println!("{} files in the manifest\n", m.files.len());

    if args.cmd == "install" {
        let base = args.root.join(".lotmcraft");
        let mut last = u64::MAX;
        let mut pct = |done: u64, total: u64| {
            let p = (done * 100).checked_div(total).unwrap_or(0);
            if p != last {
                last = p;
                print!("\r  {p}%   ");
                let _ = std::io::stdout().flush();
            }
        };

        println!("1/4  Java runtime");
        let rt = java::ensure(&base.join("runtime"), &mut pct)?;
        println!("\r     {}", rt.version()?.lines().next().unwrap_or("ready"));

        println!("2/4  Minecraft {}", m.minecraft);
        let game = mojang::install(&base, &m.minecraft, |s| println!("     {s}"), &mut pct)?;
        println!("\r     client.jar and {} libraries", game.libraries.len());

        if m.neoforge.is_empty() {
            bail!("the manifest does not say which NeoForge version to install");
        }
        println!("3/4  NeoForge {}", m.neoforge);
        let nf = neoforge::install(&base, &rt, &m.neoforge, |s| println!("     {s}"), &mut pct)?;
        println!("\r     {}", nf.version_id);

        println!("4/4  pack files");
        let plan = sync::plan(&m, &args.root)?;
        if plan.is_empty() {
            println!("     already up to date");
        } else {
            let mut on_file = |path: &str, i: usize, n: usize| println!("     [{i}/{n}] {path}");
            let mut on_bytes = |_: u64, _: u64| {};
            let mut progress = sync::Progress {
                on_file: &mut on_file,
                on_bytes: &mut on_bytes,
            };
            sync::apply(&m, &args.root, &plan, &mut progress)?;
        }

        println!("\ninstalled. the game is not launchable yet -- that is the next piece.");
        return Ok(());
    }

    if args.cmd == "launch" {
        let base = args.root.join(".lotmcraft");

        let profile = match &args.username {
            Some(name) => {
                if !launch::offline_allowed(&base) {
                    bail!(
                        "offline profiles need a Microsoft sign-in on this installation \
                         first.\nRun `lotmcraft login --root .` once, then this works."
                    );
                }
                println!("offline profile: {name}");
                launch::Profile::Offline {
                    username: name.clone(),
                    uuid: launch::offline_uuid(name),
                }
            }
            None => {
                let s = auth::sign_in(
                    &base,
                    |url, code| {
                        println!("\n  Open {url} and enter:  {code}\n");
                    },
                    |st| println!("  {st}"),
                )?;
                println!("signed in as {}", s.username);
                launch::Profile::Online(s)
            }
        };

        // the runtime and the game must already be installed
        let rt = java::ensure(&base.join("runtime"), |_, _| {})?;
        let nf = neoforge::install(&base, &rt, &m.neoforge, |_| {}, |_, _| {})
            .context("NeoForge is not installed; run `lotmcraft install` first")?;

        // natives come from the vanilla metadata, which is already on disk
        let game = mojang::install(&base, &m.minecraft, |_| {}, |_, _| {})
            .context("Minecraft is not installed; run `lotmcraft install` first")?;

        let opts = launch::LaunchOptions {
            game_dir: &args.root,
            base: &base,
            version_id: &nf.version_id,
            java: &rt,
            profile: &profile,
            memory_mb: args.memory.unwrap_or(4096),
            extra_jvm: launch::dev_jvm_args(),
            server: args.server.clone(),
        };
        let mut cmd = launch::build_command(&opts, &game.natives)?;

        if args.print_only {
            println!("\n{:?}", cmd);
            return Ok(());
        }

        println!("\nstarting {} ...\n", nf.version_id);
        let status = cmd.status().context("starting the game")?;
        if !status.success() {
            bail!("the game exited with {status}");
        }
        return Ok(());
    }

    match args.cmd.as_str() {
        "verify" => {
            let bad = sync::verify(&m, &args.root)?;
            if bad.is_empty() {
                println!("everything matches");
            } else {
                println!("{} file(s) differ:", bad.len());
                for b in &bad {
                    println!("  {b}");
                }
            }
            Ok(())
        }
        "sync" => {
            let plan = sync::plan(&m, &args.root)?;
            if plan.is_empty() {
                println!("already up to date");
                return Ok(());
            }

            if !plan.download.is_empty() {
                println!(
                    "{} file(s) to download, {}:",
                    plan.download.len(),
                    human(plan.download_bytes)
                );
                for (path, why) in plan.download.iter().take(20) {
                    println!("  {path}  ({why})");
                }
                if plan.download.len() > 20 {
                    println!("  ... and {} more", plan.download.len() - 20);
                }
            }
            if plan.need_bundle {
                println!("overrides bundle needed, because:");
                for r in &plan.bundle_reasons {
                    println!("  {r}");
                }
            }
            if !plan.orphans.is_empty() {
                println!("{} file(s) to remove:", plan.orphans.len());
                for o in plan.orphans.iter().take(20) {
                    println!("  {}", o.display());
                }
            }

            if args.dry_run {
                println!("\n--dry-run: nothing was changed");
                return Ok(());
            }
            if !plan.orphans.is_empty() && !args.yes && !confirm("\nremove those files?")? {
                bail!("stopped at your request");
            }

            let mut last_line = String::new();
            let mut on_file = |path: &str, i: usize, n: usize| {
                last_line = format!("[{i}/{n}] {path}");
                println!("{last_line}");
            };
            let mut on_bytes = |_a: u64, _b: u64| {};
            let mut progress = sync::Progress {
                on_file: &mut on_file,
                on_bytes: &mut on_bytes,
            };
            sync::apply(&m, &args.root, &plan, &mut progress)?;
            println!("\ndone");
            Ok(())
        }
        other => {
            eprintln!("unknown command: {other}");
            usage()
        }
    }
}
