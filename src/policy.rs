//! Which files the launcher keeps in sync, and which belong to the player.
//!
//! Enforced files are rewritten whenever they differ from the manifest. Seed files are
//! written only when absent, so a player's edits survive.
//!
//! The split follows NeoForge's own config naming, which means it needs no per-mod list:
//!
//! - `*client.toml`  a client preference. Never synced between client and server, and
//!                   nothing depends on everyone agreeing. Seed.
//! - `*server.toml`  the server's own values win at runtime; the local copy only matters
//!                   in singleplayer. Seed.
//! - `*common.toml`  loaded independently on both sides and never synced, so a mismatch
//!                   diverges silently. Enforced.
//! - everything else Enforced. The failure mode is mild (a player loses a tweak) where
//!                   the other direction produces desync bugs that are miserable to find.

use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Policy {
    /// Always matched to the manifest.
    Enforced,
    /// Written only if missing.
    Seed,
}

/// Folders the launcher is allowed to delete out of. Nothing outside this list is ever
/// removed, so saves, screenshots, logs and anything the player put there are safe.
///
/// Only `mods` is listed. A stale jar genuinely breaks a join; a stale config file is
/// harmless, and mods write their own config files at runtime, so sweeping `config/`
/// would delete files that were never meant to be in the manifest.
pub const MANAGED_DELETE_DIRS: &[&str] = &["mods"];

pub fn policy_for(path: &str) -> Policy {
    let lower = path.to_ascii_lowercase().replace('\\', "/");

    // players enable and disable these themselves
    if lower.starts_with("resourcepacks/") || lower.starts_with("shaderpacks/") {
        return Policy::Seed;
    }

    let name = Path::new(&lower)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");

    // catches both `client.toml` and `somemod-client.toml`
    if name.ends_with("client.toml") || name.ends_with("server.toml") {
        return Policy::Seed;
    }

    Policy::Enforced
}

/// True if this path sits inside a folder the launcher may delete from.
pub fn is_managed_for_delete(rel: &str) -> bool {
    let lower = rel.replace('\\', "/").to_ascii_lowercase();
    MANAGED_DELETE_DIRS
        .iter()
        .any(|d| lower.starts_with(&format!("{d}/")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_and_server_configs_are_seeds() {
        assert_eq!(policy_for("config/lotm-client.toml"), Policy::Seed);
        assert_eq!(policy_for("config/lotm-server.toml"), Policy::Seed);
        assert_eq!(policy_for("config/client.toml"), Policy::Seed);
    }

    #[test]
    fn common_and_json_configs_are_enforced() {
        assert_eq!(policy_for("config/lotm-common.toml"), Policy::Enforced);
        assert_eq!(
            policy_for("config/attributefix/lotm/ability_power.json"),
            Policy::Enforced
        );
        assert_eq!(policy_for("kubejs/server_scripts/recipes.js"), Policy::Enforced);
    }

    #[test]
    fn only_mods_may_be_swept() {
        assert!(is_managed_for_delete("mods/old.jar"));
        assert!(!is_managed_for_delete("config/anything.toml"));
        assert!(!is_managed_for_delete("saves/world/level.dat"));
        assert!(!is_managed_for_delete("screenshots/a.png"));
    }
}
