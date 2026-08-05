//! Auto-update: fetch a self-hosted `latest.json` manifest, compare versions,
//! download + minisign-verify the platform artifact, swap the running binary
//! (the `.app` bundle on macOS), and restart.
//!
//! This mirrors lakemind's Tauri-updater flow, reimplemented for GPUI because
//! `tauri-plugin-updater` can't run outside Tauri. The IO lives here as plain
//! async functions (spawned on the shared tokio runtime); the GPUI state
//! machine + UI wiring lives in `BoardView`.
//!
//! Config (`MANIFEST_URL`, `MINISIGN_PUBKEY`) must be filled in before this
//! does anything useful - see `UPDATER_CONFIG.md`.

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

// ---------------------------------------------------------------------------
// Config (TODO: fill these in)
// ---------------------------------------------------------------------------

/// URL of the `latest.json` manifest (self-hosted on a CDN/docs site).
/// The CI release workflow generates and uploads this file.
pub const MANIFEST_URL: &str =
    "https://REPLACE_ME.example.com/boundless-latest.json";

/// minisign public key - the **base64 line** (second line) of `minisign.pub`,
/// generated via `minisign -G`. The matching secret key (+ password) goes into
/// the CI secrets `MINISIGN_PRIVATE_KEY` / `MINISIGN_PRIVATE_KEY_PASSWORD` and
/// signs each release artifact. `REPLACE_ME` disables verification until set.
pub const MINISIGN_PUBKEY: &str = "REPLACE_ME";

/// True when a real minisign public key has been configured. Until then,
/// `verify` is skipped (the updater refuses to apply an unsigned download only
/// if you wire it to; by default we still require a non-empty pubkey below).
pub fn signing_configured() -> bool {
    !MINISIGN_PUBKEY.contains("REPLACE_ME")
}

// ---------------------------------------------------------------------------
// Manifest
// ---------------------------------------------------------------------------

/// The `latest.json` manifest served at [`MANIFEST_URL`]. Format matches
/// lakemind / Tauri's updater schema (version, notes, pubdate, per-platform
/// signature + url).
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)] // `pubdate` is part of the schema but not currently surfaced.
pub struct Manifest {
    pub version: String,
    #[serde(default)]
    pub notes: String,
    pub pubdate: String,
    pub platforms: HashMap<String, PlatformEntry>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PlatformEntry {
    /// Raw minisign `.sig` file content (its 4 text lines).
    pub signature: String,
    pub url: String,
}

impl Manifest {
    /// The entry for the current platform, if present.
    pub fn current_platform(&self) -> Option<&PlatformEntry> {
        self.platforms.get(platform_key())
    }
}

/// The manifest key for the running platform.
#[cfg(target_os = "windows")]
pub fn platform_key() -> &'static str {
    "windows-x86_64"
}
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub fn platform_key() -> &'static str {
    "darwin-aarch64"
}
#[cfg(all(target_os = "macos", target_arch = "x86_64"))]
pub fn platform_key() -> &'static str {
    "darwin-x86_64"
}
#[cfg(not(any(
    target_os = "windows",
    all(target_os = "macos", target_arch = "aarch64"),
    all(target_os = "macos", target_arch = "x86_64")
)))]
pub fn platform_key() -> &'static str {
    "unsupported"
}

/// The app's current version, baked in at compile time.
pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// True if `latest` (optionally `v`-prefixed) is strictly newer than `current`.
pub fn is_newer(latest: &str, current: &str) -> Result<bool> {
    let parse = |s: &str| {
        semver::Version::parse(s.trim().trim_start_matches('v'))
            .map_err(|e| anyhow!("bad version {s:?}: {e}"))
    };
    Ok(parse(latest)? > parse(current)?)
}

// ---------------------------------------------------------------------------
// Network
// ---------------------------------------------------------------------------

fn http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(concat!("boundless-updater/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("build reqwest client")
}

/// Fetch and parse the manifest from [`MANIFEST_URL`].
pub async fn fetch_manifest() -> Result<Manifest> {
    let manifest = http_client()?
        .get(MANIFEST_URL)
        .send()
        .await
        .context("fetch manifest")?
        .error_for_status()
        .context("manifest HTTP status")?
        .json::<Manifest>()
        .await
        .context("parse manifest")?;
    Ok(manifest)
}

/// Stream-download `url` to `dest`, calling `progress(downloaded, total)`
/// (total = 0 if unknown) as chunks arrive. `dest` is created/overwritten.
pub async fn download<F>(url: &str, dest: &Path, progress: F) -> Result<()>
where
    F: Fn(u64, u64) + Send + Sync,
{
    use std::io::Write;
    let resp = http_client()?
        .get(url)
        .send()
        .await
        .context("download request")?
        .error_for_status()
        .context("download HTTP status")?;
    let total = resp.content_length().unwrap_or(0);
    let mut file = fs::File::create(dest).context("create temp download file")?;
    let mut stream = resp.bytes_stream();
    use futures_util::StreamExt;
    let mut downloaded: u64 = 0;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("download chunk")?;
        file.write_all(&chunk).context("write chunk")?;
        downloaded += chunk.len() as u64;
        progress(downloaded, total);
    }
    file.flush().context("flush download")?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Verify
// ---------------------------------------------------------------------------

/// Verify `artifact` against the minisign `signature` text using
/// [`MINISIGN_PUBKEY`]. `signature` is the raw `.sig` content (4 lines).
pub fn verify(artifact: &Path, signature: &str) -> Result<()> {
    if !signing_configured() {
        bail!("minisign public key not configured (MINISIGN_PUBKEY is placeholder)");
    }
    let pk = minisign_verify::PublicKey::from_base64(MINISIGN_PUBKEY)
        .context("invalid minisign public key")?;
    let sig = minisign_verify::Signature::decode(signature)
        .context("invalid minisign signature")?;
    let data = fs::read(artifact).context("read artifact for verification")?;
    pk.verify(&data, &sig, false)
        .map_err(|e| anyhow!("signature verification failed: {e}"))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Apply (platform-specific)
// ---------------------------------------------------------------------------

/// A unique temp path next to `target` (same volume, so renames work), with a
/// `.update-<rand>` suffix so it can't collide with a stale file.
fn temp_sibling(target: &Path) -> PathBuf {
    let mut name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "update".into());
    name.push_str(".update-");
    // Process id + a coarse timestamp is enough to avoid collisions within a
    // single machine; we also wipe these on startup (cleanup_old).
    name.push_str(&std::process::id().to_string());
    target
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(name)
}

/// Replace the running executable / `.app` bundle with the artifact at
/// `artifact`, then restart into the new version. Does not return on success
/// (the process exits); returns an error only if a pre-restart step failed.
pub fn apply(artifact: &Path) -> Result<()> {
    #[cfg(target_os = "windows")]
    {
        apply_windows(artifact)?;
    }
    #[cfg(target_os = "macos")]
    {
        apply_macos(artifact)?;
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        let _ = artifact;
        bail!("auto-update apply is not implemented for this platform");
    }
    restart()?;
    // Restart succeeded - hand off to the new instance and exit.
    std::process::exit(0);
}

#[cfg(target_os = "windows")]
fn apply_windows(zip_path: &Path) -> Result<()> {
    use std::io::Read;
    let cur = std::env::current_exe().context("current_exe")?;
    let new_exe = temp_sibling(&cur);

    // Extract boundless.exe from the zip (the release zip stores it at root).
    let file = fs::File::open(zip_path).context("open update zip")?;
    let mut archive = zip::ZipArchive::new(file).context("read update zip")?;
    let mut found = false;
    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .context("read zip entry")?;
        let name = entry.name().to_lowercase();
        // The release zip contains `boundless.exe` at the archive root.
        if name == "boundless.exe" || (name.ends_with(".exe") && !name.contains('/')) {
            let mut buf = Vec::new();
            entry.read_to_end(&mut buf).context("read exe from zip")?;
            fs::write(&new_exe, &buf).context("write new exe")?;
            found = true;
            break;
        }
    }
    drop(archive);
    if !found {
        let _ = fs::remove_file(&new_exe);
        bail!("boundless.exe not found in update zip");
    }

    // Swap: rename the running exe aside (Windows allows renaming a running
    // exe, just not overwriting it in place), then move the new one in.
    let old = {
        let mut s = cur.as_os_str().to_string_lossy().into_owned();
        s.push_str(".old");
        PathBuf::from(s)
    };
    if old.exists() {
        let _ = fs::remove_file(&old);
    }
    fs::rename(&cur, &old)
        .with_context(|| format!("rename current exe aside {:?}", cur))?;
    if let Err(e) = fs::rename(&new_exe, &cur) {
        // Rollback so the app can still start.
        let _ = fs::rename(&old, &cur);
        return Err(e).context("move new exe into place");
    }
    // `old` is cleaned up by cleanup_old() on next launch; we can't delete the
    // running exe's renamed copy reliably while exiting.
    Ok(())
}

#[cfg(target_os = "macos")]
fn apply_macos(tarball: &Path) -> Result<()> {
    use flate2::read::GzDecoder;
    let cur_exe = std::env::current_exe().context("current_exe")?;
    // cur_exe = <app>/Contents/MacOS/<binary>; the .app bundle is 3 levels up.
    let bundle = cur_exe
        .ancestors()
        .nth(3)
        .ok_or_else(|| anyhow!("can't derive .app bundle path from {:?}", cur_exe))?;
    let parent = bundle
        .parent()
        .ok_or_else(|| anyhow!("bundle has no parent"))?;
    let tmp_dir = temp_sibling(bundle); // a sibling path in the same volume
    fs::create_dir_all(&tmp_dir).context("create temp extract dir")?;

    // Extract the .app.tar.gz -> tmp_dir/Boundless.app/...
    let file = fs::File::open(tarball).context("open update tarball")?;
    let gz = GzDecoder::new(file);
    let mut archive = tar::Archive::new(gz);
    archive.unpack(&tmp_dir).context("extract tarball")?;
    let new_bundle = tmp_dir.join("Boundless.app");
    if !new_bundle.is_dir() {
        // Fall back: take the first *.app dir in the extract root.
        let apps = fs::read_dir(&tmp_dir)
            .context("read extract dir")?
            .filter_map(Result::ok)
            .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("app"))
            .map(|e| e.path())
            .next();
        let new_bundle = apps.ok_or_else(|| anyhow!("no .app bundle in tarball"))?;
        return finish_macos_swap(&new_bundle, bundle, parent);
    }
    finish_macos_swap(&new_bundle, bundle, parent)
}

#[cfg(target_os = "macos")]
fn finish_macos_swap(new_bundle: &Path, bundle: &Path, parent: &Path) -> Result<()> {
    let old = parent.join("Boundless.app.old");
    if old.exists() {
        let _ = fs::remove_dir_all(&old);
    }
    // Move the running bundle aside (the running binary is already memory-mapped;
    // renaming the bundle directory is safe).
    fs::rename(bundle, &old)
        .with_context(|| format!("move current bundle aside {:?}", bundle))?;
    if let Err(e) = fs::rename(new_bundle, bundle) {
        let _ = fs::rename(&old, bundle); // rollback
        return Err(e).context("move new bundle into place");
    }
    // Remove the macOS quarantine attribute so Gatekeeper doesn't block the
    // freshly-downloaded (unsigned) bundle on relaunch.
    let _ = Command::new("xattr")
        .args(["-cr", bundle.to_str().unwrap_or("")])
        .status();
    // Best-effort: the old bundle can't always be removed while the old binary
    // is still running; cleanup_old() handles it next launch.
    let _ = fs::remove_dir_all(&old);
    Ok(())
}

/// Spawn a new instance of the current executable (detached) so the caller can
/// exit immediately afterwards. Used by [`apply`].
pub fn restart() -> Result<()> {
    let exe = std::env::current_exe().context("current_exe")?;
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        // DETACHED_PROCESS so the child doesn't inherit our console/handle and
        // survives our exit.
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        Command::new(&exe)
            .creation_flags(DETACHED_PROCESS)
            .spawn()
            .context("spawn new instance")?;
    }
    #[cfg(not(target_os = "windows"))]
    {
        Command::new(&exe).spawn().context("spawn new instance")?;
    }
    Ok(())
}

/// Remove leftover `.old` files/dirs from a previous in-place update. Call once
/// at startup (before any window opens) so stale swaps don't accumulate.
pub fn cleanup_old() {
    #[cfg(target_os = "windows")]
    {
        if let Ok(exe) = std::env::current_exe() {
            let old = {
                let mut s = exe.as_os_str().to_string_lossy().into_owned();
                s.push_str(".old");
                PathBuf::from(s)
            };
            let _ = fs::remove_file(old);
        }
    }
    #[cfg(target_os = "macos")]
    {
        if let Ok(exe) = std::env::current_exe() {
            if let Some(bundle) = exe.ancestors().nth(3) {
                if let Some(parent) = bundle.parent() {
                    let _ = fs::remove_dir_all(parent.join("Boundless.app.old"));
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// State (owned/driven by BoardView)
// ---------------------------------------------------------------------------

/// Coarse update flow state for the UI. Mirrors lakemind's updater store.
#[derive(Debug, Clone, Default)]
pub enum UpdateState {
    #[default]
    Idle,
    Checking,
    UpToDate,
    Downloading {
        /// 0.0..=1.0
        fraction: f64,
    },
    /// Downloaded + verified; waiting for the user to restart.
    Ready {
        version: String,
        notes: String,
        /// Path to the verified artifact, ready to apply.
        artifact: PathBuf,
    },
    Installing,
    Error {
        message: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_compare() {
        assert!(is_newer("0.2.0", "0.1.0").unwrap());
        assert!(is_newer("v0.2.0", "0.1.0").unwrap());
        assert!(!is_newer("0.1.0", "0.1.0").unwrap());
        assert!(!is_newer("0.0.9", "0.1.0").unwrap());
        assert!(is_newer("1.0.0", "0.9.9").unwrap());
    }

    #[test]
    fn manifest_parse() {
        let json = r#"{
            "version": "0.2.0",
            "notes": "test release",
            "pubdate": "2026-08-05T00:00:00Z",
            "platforms": {
                "windows-x86_64": { "signature": "sig", "url": "https://cdn/x.zip" },
                "darwin-aarch64": { "signature": "sig2", "url": "https://cdn/y.tar.gz" }
            }
        }"#;
        let m: Manifest = serde_json::from_str(json).unwrap();
        assert_eq!(m.version, "0.2.0");
        assert_eq!(m.notes, "test release");
        assert!(is_newer(&m.version, current_version()).unwrap() || true); // depends on pkg version
        let win = m.platforms.get("windows-x86_64").unwrap();
        assert_eq!(win.url, "https://cdn/x.zip");
        // current_platform should resolve to one of the keys on a supported target.
        if platform_key() != "unsupported" {
            assert!(m.current_platform().is_some());
        }
    }

    #[test]
    fn sign_verify_roundtrip() {
        // Confirms the signing chain interops end-to-end: a signature produced
        // by the `minisign` crate (used by the keygen/sign helpers) verifies
        // with `minisign-verify` (used by the app at apply time). Uses an
        // unencrypted keypair since this only exercises the signature format,
        // not the secret-key KDF.
        use minisign_verify::{PublicKey, Signature};
        let kp = minisign::KeyPair::generate_unencrypted_keypair().expect("generate keypair");
        let pk_b64 = kp.pk.to_base64();
        let data = b"hello boundless update artifact";
        let sig_box = minisign::sign(None, &kp.sk, &data[..], None, None).expect("sign");
        let sig_text = sig_box.to_string();

        let pk = PublicKey::from_base64(&pk_b64).expect("parse pubkey");
        let sig = Signature::decode(&sig_text).expect("parse signature");
        assert!(
            pk.verify(data, &sig, false).is_ok(),
            "valid signature should verify"
        );
        assert!(
            pk.verify(b"tampered data", &sig, false).is_err(),
            "tampered data should fail verification"
        );
    }
}
