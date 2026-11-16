#!/usr/bin/env python3
"""
Builds a launcher manifest from a Minecraft pack folder.

Walks the folders you name, records each file's size and SHA-1, then asks Modrinth which of them it already
hosts. Anything Modrinth knows gets its CDN url, so you do not host it; anything left over is marked
"self" and you upload only those.

    python3 manifest.py --root ~/curseforge/minecraft/Instances/AstralWorld \
        --base-url https://github.com/you/astral-pack/releases/download/v12/

If the folder holds a CurseForge minecraftinstance.json it is read for the mods, because CurseForge has already
recorded every file's name, size, SHA-1 and download url there. Everything else (config, kubejs, resourcepacks)
is hashed off disk, and anything Modrinth happens to host is linked rather than self-hosted.

The base url is where YOUR files will live. Linked files ignore it.
"""
import argparse, hashlib, json, os, sys, urllib.request, zipfile

# what belongs in a client pack. config and kubejs matter: a client with the wrong configs desyncs or fails to join.
INCLUDE = ["mods", "config", "defaultconfigs", "kubejs", "resourcepacks", "shaderpacks", "schematics"]
# never ship these: they are the player's own, or the server's
SKIP_NAMES = {"options.txt", "servers.dat", "usercache.json", "usernamecache.json", ".DS_Store", "Thumbs.db"}
SKIP_SUFFIX = (".log", ".log.gz", ".lck", ".tmp", ".bak")

MODRINTH_LOOKUP = "https://api.modrinth.com/v2/version_files"
UA = "astral-launcher-manifest/1.0 (contact: your-email-here)"


def sha1_of(path):
    h = hashlib.sha1()
    with open(path, "rb") as f:
        for block in iter(lambda: f.read(1 << 20), b""):
            h.update(block)
    return h.hexdigest()


def from_curseforge(path):
    """Reads a CurseForge instance. Returns (files, blocked) where blocked could not be linked."""
    with open(path, encoding="utf-8-sig") as fh:
        inst = json.load(fh)
    files, blocked = [], []
    for addon in inst.get("installedAddons", []):
        f = addon.get("installedFile") or {}
        name = f.get("fileName")
        if not name:
            continue
        sha1 = ""
        for h in f.get("hashes", []):
            if h.get("algo") == 1:                      # 1 = SHA-1, 2 = MD5
                sha1 = h.get("value", "")
        entry = {"path": "mods/" + name, "size": f.get("fileLength", 0), "sha1": sha1}
        url = f.get("downloadUrl") or ""
        if url:
            entry["url"] = url
            entry["source"] = "curseforge"
            files.append(entry)
        else:
            # the author turned off third-party distribution: no launcher may fetch it, so you host it yourself
            entry["source"] = "blocked"
            entry["project"] = addon.get("addonID")
            entry["file"] = f.get("id")
            blocked.append(entry)
    ml = (inst.get("baseModLoader") or {}).get("name", "")
    return files, blocked, inst.get("gameVersion", ""), ml


def walk(root, folders):
    out = []
    for folder in folders:
        base = os.path.join(root, folder)
        if not os.path.isdir(base):
            print(f"  (no {folder}/, skipping)", file=sys.stderr)
            continue
        for dirpath, _dirnames, filenames in os.walk(base):
            for name in sorted(filenames):
                if name in SKIP_NAMES or name.endswith(SKIP_SUFFIX) or name.startswith("."):
                    continue
                full = os.path.join(dirpath, name)
                rel = os.path.relpath(full, root).replace(os.sep, "/")
                out.append({"path": rel, "size": os.path.getsize(full), "sha1": sha1_of(full)})
                print(f"  {rel}", file=sys.stderr)
    return out


def ask_modrinth(files, batch=256):
    """Modrinth can look a file up by its hash. Whatever it knows, we link instead of hosting."""
    found = {}
    hashes = [f["sha1"] for f in files]
    for i in range(0, len(hashes), batch):
        body = json.dumps({"hashes": hashes[i:i + batch], "algorithm": "sha1"}).encode()
        req = urllib.request.Request(MODRINTH_LOOKUP, data=body,
                                     headers={"Content-Type": "application/json", "User-Agent": UA})
        try:
            with urllib.request.urlopen(req, timeout=60) as r:
                data = json.load(r)
        except Exception as e:                       # offline, rate limited, whatever: everything becomes self-hosted
            print(f"  ! Modrinth lookup failed ({e}); the rest will be marked self-hosted", file=sys.stderr)
            return found
        for sha, version in data.items():
            for vf in version.get("files", []):
                if vf.get("hashes", {}).get("sha1") == sha:
                    found[sha] = vf["url"]
                    break
        print(f"  Modrinth knew {len(found)} of {min(i + batch, len(hashes))} so far", file=sys.stderr)
    return found


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--root", required=True, help="the pack folder (the one holding mods/)")
    ap.add_argument("--out", default="manifest.json")
    ap.add_argument("--base-url", default="", help="where your own files will be served from, ending in /")
    ap.add_argument("--version", default="1", help="pack version, shown in the launcher")
    ap.add_argument("--minecraft", default="1.21.1")
    ap.add_argument("--neoforge", default="", help="e.g. 21.1.73")
    ap.add_argument("--no-modrinth", action="store_true", help="skip the lookup and host everything yourself")
    ap.add_argument("--folders", default=",".join(INCLUDE))
    ap.add_argument("--no-curseforge", action="store_true", help="ignore minecraftinstance.json and hash mods/ instead")
    ap.add_argument("--bundle", default="overrides.zip",
                    help="write the self-hosted files into this zip; '' to list them individually instead")
    ap.add_argument("--extra", action="append", default=[], metavar="PATH=URL",
                    help="a file you host yourself that is not in the pack folder, e.g. "
                         "mods/astral_world-0.2.256.jar=https://github.com/you/astral-world/releases/download/v0.2.256/astral_world-0.2.256.jar "
                         "(repeatable; the path is resolved under --root for hashing)")
    ap.add_argument("--name", default="Lord of the Mysteries Craft")
    a = ap.parse_args()
    a.root = os.path.expanduser(a.root)

    cf, blocked, cf_mc, cf_loader = [], [], "", ""
    inst = os.path.join(a.root, "minecraftinstance.json")
    if os.path.isfile(inst) and not a.no_curseforge:
        print(f"Reading {inst}", file=sys.stderr)
        cf, blocked, cf_mc, cf_loader = from_curseforge(inst)
        print(f"  {len(cf)} mods linked from CurseForge, {len(blocked)} with distribution turned off", file=sys.stderr)

    folders = [f.strip() for f in a.folders.split(",") if f.strip()]
    if cf:
        folders = [f for f in folders if f != "mods"]      # CurseForge already told us about mods/

    print("Hashing:", file=sys.stderr)
    files = walk(a.root, folders)
    if not files and not cf:
        sys.exit("nothing found; is --root the instance folder (the one holding mods/) ?")

    # anything the author blocked has to come from you, so it is hashed off disk like your own files
    for b in blocked:
        local = os.path.join(a.root, b["path"])
        if os.path.isfile(local):
            b["size"] = os.path.getsize(local)
            b["sha1"] = sha1_of(local)
            files.append({"path": b["path"], "size": b["size"], "sha1": b["sha1"]})
        else:
            print(f"  ! {b['path']} is blocked and not on disk", file=sys.stderr)

    for spec in a.extra:
        rel, _, url = spec.partition("=")
        rel = rel.strip().replace(os.sep, "/")
        if not url:
            sys.exit(f"--extra {rel}: needs PATH=URL")
        local = os.path.join(a.root, rel)
        if os.path.isfile(local):
            size, sha1 = os.path.getsize(local), sha1_of(local)
            print(f"  external: {rel} (hashed off disk)", file=sys.stderr)
        else:
            # not in the pack folder. The release asset is what players get, so hash that.
            print(f"  external: {rel} not on disk, hashing {url}", file=sys.stderr)
            try:
                req = urllib.request.Request(url, headers={"User-Agent": UA})
                h, size = hashlib.sha1(), 0
                with urllib.request.urlopen(req, timeout=120) as r:
                    for block in iter(lambda: r.read(1 << 20), b""):
                        h.update(block); size += len(block)
                sha1 = h.hexdigest()
            except Exception as e:
                sys.exit(f"--extra {rel}: not at {local}, and could not fetch {url}\n  {e}")
            if size == 0:
                sys.exit(f"--extra {rel}: {url} returned an empty file")
            print(f"    {size} bytes, sha1 {sha1}", file=sys.stderr)
        cf.append({"path": rel, "size": size, "sha1": sha1, "url": url, "source": "external"})

    # an --extra may also have been hashed off disk; the explicit url wins
    extra_paths = {e.partition("=")[0].strip().replace(os.sep, "/") for e in a.extra}
    if extra_paths:
        files = [f for f in files if f["path"] not in extra_paths]

    linked = {} if a.no_modrinth else ask_modrinth(files)

    mine = 0
    for f in files:
        url = linked.get(f["sha1"])
        if url:
            f["url"] = url
            f["source"] = "modrinth"
        else:
            f["url"] = a.base_url + f["path"]
            f["source"] = "self"
            mine += 1

    files = cf + files

    bundle = None
    if a.bundle:
        mine_files = [f for f in files if f["source"] == "self"]
        if mine_files:
            out_zip = os.path.join(os.path.dirname(os.path.abspath(a.out)) or ".", a.bundle)
            # deterministic: fixed timestamps and sorted order, so an unchanged pack hashes the same twice
            with zipfile.ZipFile(out_zip, "w", zipfile.ZIP_DEFLATED) as z:
                for f in sorted(mine_files, key=lambda x: x["path"]):
                    info = zipfile.ZipInfo(f["path"], date_time=(1980, 1, 1, 0, 0, 0))
                    info.compress_type = zipfile.ZIP_DEFLATED
                    info.external_attr = 0o644 << 16
                    with open(os.path.join(a.root, f["path"]), "rb") as fh:
                        z.writestr(info, fh.read())
            for f in mine_files:
                f["source"] = "bundle"
                f.pop("url", None)
            bundle = {"file": a.bundle, "size": os.path.getsize(out_zip), "sha1": sha1_of(out_zip),
                      "url": a.base_url + a.bundle, "count": len(mine_files)}
            print(f"\nwrote {out_zip}  ({bundle['size'] / 1e6:.1f} MB, {len(mine_files)} files)", file=sys.stderr)

    manifest = {
        "name": a.name,
        "version": a.version,
        "minecraft": cf_mc or a.minecraft,
        "neoforge": a.neoforge or cf_loader.replace("neoforge-", ""),
        "bundle": bundle,
        "files": files,
    }
    with open(a.out, "w", encoding="utf-8") as fh:
        json.dump(manifest, fh, indent=2)

    total = sum(f["size"] for f in files)
    by = {}
    for f in files:
        by[f["source"]] = by.get(f["source"], 0) + 1
    self_bytes = sum(f["size"] for f in files if f["source"] in ("self", "bundle"))
    print(f"\nwrote {a.out}", file=sys.stderr)
    print(f"  {len(files)} files, {total / 1e9:.2f} GB total", file=sys.stderr)
    for k in sorted(by):
        print(f"  {by[k]:4d} {k}", file=sys.stderr)
    print(f"  you host {self_bytes / 1e6:.0f} MB", file=sys.stderr)
    if blocked:
        print(f"\n  {len(blocked)} mod(s) have third-party distribution turned off by their author.", file=sys.stderr)
        print("  No launcher may fetch these from CurseForge; they are in the 'self' list and you serve them:", file=sys.stderr)
        for b in blocked[:20]:
            print(f"    {b['path']}", file=sys.stderr)
    if bundle:
        print(f"\nUpload {a.out} and {a.bundle} to the release at your base url. Nothing else.", file=sys.stderr)
    elif mine:
        print("\nUpload only the 'self' files, keeping their paths, under your base url.", file=sys.stderr)
        print("List them with:  jq -r '.files[]|select(.source==\"self\")|.path' " + a.out, file=sys.stderr)


if __name__ == "__main__":
    main()
