//! Downloading and hashing.
//!
//! Two rules hold everywhere in here:
//!
//! 1. Nothing is believed until its SHA-1 matches. A truncated download, a captive portal
//!    serving a login page, a CDN handing back an error body -- all of them look like a
//!    successful HTTP response and all of them are caught by the hash.
//! 2. Nothing lands at its real path until it has been verified. Files are written to a
//!    temporary name in the same folder and renamed into place, so a crash or a pulled
//!    cable leaves the previous file intact rather than a half-written one.

use anyhow::{anyhow, bail, Context, Result};
use sha1::{Digest, Sha1};
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

const RETRIES: u32 = 3;
const CHUNK: usize = 1 << 16;

pub fn sha1_of_file(path: &Path) -> Result<String> {
    let mut f = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut h = Sha1::new();
    let mut buf = vec![0u8; CHUNK];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex(&h.finalize()))
}

#[allow(dead_code)] // used by the tests, and by the Tauri shell
pub fn sha1_of_bytes(bytes: &[u8]) -> String {
    let mut h = Sha1::new();
    h.update(bytes);
    hex(&h.finalize())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn agent() -> ureq::Agent {
    static AGENT: std::sync::OnceLock<ureq::Agent> = std::sync::OnceLock::new();
    AGENT.get_or_init(build_agent).clone()
}

fn build_agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(20))
        .timeout_read(Duration::from_secs(120))
        .user_agent(concat!("lotmcraft-launcher/", env!("CARGO_PKG_VERSION")))
        .build()
}

/// Fetches a small resource straight into memory. For the manifest, not for mods.
pub fn get_bytes(url: &str, limit: usize) -> Result<Vec<u8>> {
    let mut last = None;
    for attempt in 0..RETRIES {
        match try_get_bytes(url, limit) {
            Ok(b) => return Ok(b),
            Err(e) => {
                last = Some(e);
                backoff(attempt);
            }
        }
    }
    Err(last.unwrap_or_else(|| anyhow!("could not fetch {url}")))
}

fn try_get_bytes(url: &str, limit: usize) -> Result<Vec<u8>> {
    let resp = agent()
        .get(url)
        .call()
        .with_context(|| format!("requesting {url}"))?;
    let mut buf = Vec::new();
    resp.into_reader()
        .take(limit as u64 + 1)
        .read_to_end(&mut buf)?;
    if buf.len() > limit {
        bail!("{url} is larger than the {limit} byte limit");
    }
    Ok(buf)
}

/// Downloads `url` to `dest`, resuming a partial `.part` file if one is there, and only
/// moves it into place once the hash matches.
///
/// `on_progress` is called with (bytes so far, total) and may be called many times.
pub fn download_verified(
    url: &str,
    dest: &Path,
    expect_sha1: &str,
    expect_size: u64,
    mut on_progress: impl FnMut(u64, u64),
) -> Result<()> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let part = part_path(dest);

    let mut last_err = None;
    for attempt in 0..RETRIES {
        match attempt_download(url, &part, expect_size, &mut on_progress) {
            Ok(()) => {
                let got = sha1_of_file(&part)?;
                if got.eq_ignore_ascii_case(expect_sha1) {
                    // same folder, so this is a rename rather than a copy across devices
                    fs::rename(&part, dest).with_context(|| {
                        format!("moving {} into place", dest.display())
                    })?;
                    return Ok(());
                }
                // A resumed download can be built on a corrupt prefix, so the only safe
                // move is to throw the whole thing away and start clean.
                let _ = fs::remove_file(&part);
                last_err = Some(anyhow!(
                    "{}: expected sha1 {expect_sha1}, got {got}",
                    dest.display()
                ));
            }
            Err(e) => last_err = Some(e),
        }
        backoff(attempt);
    }
    let _ = fs::remove_file(&part);
    Err(last_err.unwrap_or_else(|| anyhow!("could not download {url}")))
}

fn attempt_download(
    url: &str,
    part: &Path,
    expect_size: u64,
    on_progress: &mut impl FnMut(u64, u64),
) -> Result<()> {
    let have = fs::metadata(part).map(|m| m.len()).unwrap_or(0);
    if expect_size > 0 && have > expect_size {
        // longer than it should be: the partial file is not what we think it is
        let _ = fs::remove_file(part);
        return attempt_download(url, part, expect_size, on_progress);
    }

    let mut req = agent().get(url);
    if have > 0 {
        req = req.set("Range", &format!("bytes={have}-"));
    }
    let resp = req.call().with_context(|| format!("requesting {url}"))?;
    let resumed = resp.status() == 206;

    let total = if expect_size > 0 {
        expect_size
    } else {
        resp.header("Content-Length")
            .and_then(|v| v.parse::<u64>().ok())
            .map(|n| n + if resumed { have } else { 0 })
            .unwrap_or(0)
    };

    let mut file = if resumed {
        let mut f = fs::OpenOptions::new().write(true).open(part)?;
        f.seek(SeekFrom::Start(have))?;
        f
    } else {
        // server ignored the range, or there was nothing to resume
        File::create(part)?
    };
    let mut done = if resumed { have } else { 0 };

    let mut reader = resp.into_reader();
    let mut buf = vec![0u8; CHUNK];
    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e).context("reading the response body"),
        };
        file.write_all(&buf[..n])?;
        done += n as u64;
        on_progress(done, total);
    }
    file.flush()?;
    file.sync_all()?;
    Ok(())
}

/// Downloads to `dest` with the same resume and retry behaviour, but leaves verification
/// to the caller. Only for places where the publisher uses a hash other than SHA-1, such
/// as Adoptium's SHA-256; everything with a manifest entry goes through
/// `download_verified` instead.
pub fn download_unverified(
    url: &str,
    dest: &Path,
    expect_size: u64,
    on_progress: &mut impl FnMut(u64, u64),
) -> Result<()> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    let part = part_path(dest);
    let mut last_err = None;
    for attempt in 0..RETRIES {
        match attempt_download(url, &part, expect_size, on_progress) {
            Ok(()) => {
                fs::rename(&part, dest)
                    .with_context(|| format!("moving {} into place", dest.display()))?;
                return Ok(());
            }
            Err(e) => last_err = Some(e),
        }
        backoff(attempt);
    }
    let _ = fs::remove_file(&part);
    Err(last_err.unwrap_or_else(|| anyhow!("could not download {url}")))
}

/// POSTs a form body and returns the response bytes, including for 4xx, because the
/// device-code endpoint signals "still waiting" with a 400 carrying a JSON error.
pub fn post_form_raw(url: &str, fields: &[(&str, &str)]) -> Result<Vec<u8>> {
    let body = fields
        .iter()
        .map(|(k, v)| format!("{}={}", urlencode(k), urlencode(v)))
        .collect::<Vec<_>>()
        .join("&");
    let resp = agent()
        .post(url)
        .set("Content-Type", "application/x-www-form-urlencoded")
        .send_string(&body);
    read_body(resp, url)
}

pub fn post_json_raw(
    url: &str,
    body: &serde_json::Value,
    bearer: Option<&str>,
) -> Result<Vec<u8>> {
    let mut req = agent()
        .post(url)
        .set("Content-Type", "application/json")
        .set("Accept", "application/json");
    if let Some(b) = bearer {
        req = req.set("Authorization", &format!("Bearer {b}"));
    }
    read_body(req.send_string(&body.to_string()), url)
}

pub fn get_with_bearer(url: &str, bearer: &str) -> Result<Vec<u8>> {
    let resp = agent()
        .get(url)
        .set("Authorization", &format!("Bearer {bearer}"))
        .set("Accept", "application/json")
        .call();
    read_body(resp, url)
}

fn read_body(
    resp: std::result::Result<ureq::Response, ureq::Error>,
    url: &str,
) -> Result<Vec<u8>> {
    let r = match resp {
        Ok(r) => r,
        // a 4xx carries the body we need, so it is not treated as a transport failure
        Err(ureq::Error::Status(_, r)) => r,
        Err(e) => return Err(anyhow!("{url}: {e}")),
    };
    let mut buf = Vec::new();
    r.into_reader().take(4 << 20).read_to_end(&mut buf)?;
    Ok(buf)
}

fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            b' ' => "+".to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// One file to fetch.
#[derive(Clone, Debug)]
pub struct Job {
    pub url: String,
    pub dest: PathBuf,
    pub sha1: String,
    pub size: u64,
}

/// Downloads `job` only if what is on disk does not already match.
///
/// Size is checked before hash: a file of exactly the right length is taken on trust,
/// because hashing hundreds of megabytes on every launch would make startup unusable.
pub fn download_if_needed(
    url: &str,
    dest: &Path,
    sha1: &str,
    size: u64,
    on_progress: &mut impl FnMut(u64, u64),
) -> Result<()> {
    if dest.is_file() {
        let len = fs::metadata(dest)?.len();
        let ok = if size > 0 {
            len == size
        } else {
            sha1_of_file(dest)?.eq_ignore_ascii_case(sha1)
        };
        if ok {
            return Ok(());
        }
    }
    download_verified(url, dest, sha1, size, on_progress)
}

/// Runs a batch of jobs across `threads` workers.
///
/// Minecraft's asset index is about three thousand small files; fetched one at a time
/// the round trips dominate and an install takes many minutes. Progress is reported as
/// files completed rather than bytes, because with several in flight a byte count jumps
/// around and means nothing.
pub fn download_all(
    jobs: &[Job],
    threads: usize,
    on_progress: &mut impl FnMut(u64, u64),
) -> Result<()> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    let total = jobs.len();
    if total == 0 {
        return Ok(());
    }
    let next = Arc::new(AtomicUsize::new(0));
    let done = Arc::new(AtomicUsize::new(0));
    let failure: Arc<Mutex<Option<anyhow::Error>>> = Arc::new(Mutex::new(None));
    let threads = threads.max(1).min(total);

    std::thread::scope(|scope| {
        for _ in 0..threads {
            let (next, done, failure) = (next.clone(), done.clone(), failure.clone());
            scope.spawn(move || loop {
                // stop handing out work as soon as anything has failed
                if failure.lock().map(|g| g.is_some()).unwrap_or(true) {
                    return;
                }
                let i = next.fetch_add(1, Ordering::SeqCst);
                let Some(job) = jobs.get(i) else { return };
                let mut noop = |_: u64, _: u64| {};
                if let Err(e) =
                    download_if_needed(&job.url, &job.dest, &job.sha1, job.size, &mut noop)
                {
                    if let Ok(mut slot) = failure.lock() {
                        if slot.is_none() {
                            *slot = Some(e.context(format!("fetching {}", job.url)));
                        }
                    }
                    return;
                }
                done.fetch_add(1, Ordering::SeqCst);
            });
        }

        // the calling thread reports progress while the workers run
        loop {
            let d = done.load(Ordering::SeqCst);
            on_progress(d as u64, total as u64);
            if d >= total || failure.lock().map(|g| g.is_some()).unwrap_or(true) {
                break;
            }
            std::thread::sleep(Duration::from_millis(120));
        }
    });

    if let Some(e) = failure.lock().ok().and_then(|mut g| g.take()) {
        return Err(e);
    }
    on_progress(total as u64, total as u64);
    Ok(())
}

fn part_path(dest: &Path) -> PathBuf {
    let mut name = dest.file_name().unwrap_or_default().to_os_string();
    name.push(".part");
    dest.with_file_name(name)
}

fn backoff(attempt: u32) {
    std::thread::sleep(Duration::from_millis(400 * (1 << attempt)));
}

/// Writes bytes to `dest` through a temporary file in the same folder.
pub fn write_atomic(dest: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = part_path(dest);
    {
        let mut f = File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, dest)
        .with_context(|| format!("moving {} into place", dest.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_match_known_values() {
        // sha1 of the empty string, and of "abc"
        assert_eq!(sha1_of_bytes(b""), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        assert_eq!(sha1_of_bytes(b"abc"), "a9993e364706816aba3e25717850c26c9cd0d89d");
    }

    #[test]
    fn part_path_sits_beside_its_target() {
        let p = part_path(Path::new("/a/b/mods/x.jar"));
        assert_eq!(p, PathBuf::from("/a/b/mods/x.jar.part"));
    }
}
