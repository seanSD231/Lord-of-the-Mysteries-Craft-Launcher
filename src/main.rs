//! Command-line front end for the sync core.
//!
//! The eventual Tauri shell calls the same functions; this exists so the machinery can be
//! exercised and debugged without a UI in the way, and so a broken install can be fixed
//! over a support chat with one command.

mod fetch;
mod java;
mod manifest;
mod policy;
mod sync;

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
}

fn parse_args() -> Args {
    let mut it = std::env::args().skip(1);
    let cmd = it.next().unwrap_or_else(|| usage());
    let (mut root, mut manifest) = (None, None);
    let (mut dry_run, mut yes) = (false, false);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--root" => root = it.next(),
            "--manifest" => manifest = it.next(),
            "--dry-run" => dry_run = true,
            "--yes" | "-y" => yes = true,
            "-h" | "--help" => usage(),
            other => {
                eprintln!("unknown argument: {other}");
                usage()
            }
        }
    }
    let needs_manifest = cmd != "java";
    match (root, manifest) {
        (Some(r), m) if m.is_some() || !needs_manifest => Args {
            cmd,
            root: PathBuf::from(r),
            manifest: m.unwrap_or_default(),
            dry_run,
            yes,
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

    let m = load_manifest(&args.manifest)?;
    println!(
        "{} pack v{}  (Minecraft {}, NeoForge {})",
        m.name,
        m.version,
        m.minecraft,
        if m.neoforge.is_empty() { "-" } else { &m.neoforge }
    );
    println!("{} files in the manifest\n", m.files.len());

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
