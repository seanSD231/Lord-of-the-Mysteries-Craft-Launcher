# Lord of the Mysteries Craft Launcher

A launcher for the Lord of the Mysteries Craft modpack. It installs Minecraft and NeoForge,
syncs the pack from a manifest, and launches the game. Players never touch CurseForge.

Windows, macOS and Linux.

## How the pack is distributed

The launcher is driven by a **manifest**: a list of every file in the pack with its size and
SHA-1, plus where to get it. On launch it compares the manifest against what's on disk and
downloads only what differs.

Files come from three places:

| source | what | who hosts it |
|---|---|---|
| `curseforge` | the ~229 third-party mods | CurseForge's CDN |
| `external` | `astral_world.jar` | the astral-world release |
| `bundle` | configs, kubejs, defaultconfigs, resourcepacks | `overrides.zip` on a pack release here |

The bundle is one zip rather than ~750 loose assets, because at ~6 MB it isn't worth being
clever about. The manifest still records a hash per file inside it, so the launcher can tell
exactly what drifted; when anything mismatches it pulls the zip and extracts.

### Enforced vs. seed files

Not every file should be overwritten. Anything gameplay-affecting is **enforced** — always
written, player edits lost. Client preferences (keybinds, client-side mod settings) are
**seed** — written only if absent, so a player's tweaks survive a sync.

Orphan cleanup only ever deletes inside folders the launcher manages. It never touches
saves, screenshots, logs, or anything outside the instance.

## Cutting a pack release

From inside the modpack instance folder:

```bash
python3 tools/manifest.py \
  --root . \
  --base-url https://github.com/seanSD231/Lord-of-the-Mysteries-Craft-Launcher/releases/download/pack-vN/ \
  --version N \
  --extra "mods/astral_world-X.Y.Z.jar=https://github.com/seanSD231/astral-world/releases/download/vX.Y.Z/astral_world-X.Y.Z.jar"

gh release create pack-vN manifest.json overrides.zip \
  --repo seanSD231/Lord-of-the-Mysteries-Craft-Launcher \
  --title "Pack vN"
```

`--base-url` must match the tag you're about to create, since it's baked into the manifest.

### Retention

Keep the last 5 pack releases, and anything newer than 72 hours, whichever is more. That's
the rollback window — rolling back means pointing the launcher at an older tag, which only
works if the release is still there. Pure time-based retention has a weekend hole: ship
Friday, bug surfaces Monday, Saturday's rollback target is gone.

## Repo layout

```
tools/manifest.py    builds manifest.json + overrides.zip from a pack folder
```

Everything else is still to be written. Build order:

1. installer core (download, verify, extract, atomic replace)
2. vanilla + NeoForge install
3. launch
4. profiles (multi-account, for dev testing)
5. shell (the UI)
