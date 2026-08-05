# Auto-update configuration

Boundless ships a self-hosted auto-updater (lakemind-style, ported to GPUI -
see `src/updater.rs`). On launch it polls a `latest.json` manifest, and if a
newer version exists it downloads the platform artifact, verifies a minisign
signature, swaps the running binary / `.app` bundle in place, and restarts.

This doc covers the **one-time setup** the maintainer must do before auto-update
works end-to-end. The client code is already in place; only config + secrets +
a signing keypair are needed.

## 1. Generate a minisign keypair

The updater verifies each download against a minisign **public key** baked into
the app; CI signs each release with the matching **secret key**. Generate the
keypair with the bundled helper (no need to install the `minisign` CLI - whose
GitHub download is unreachable from some networks anyway):

```sh
cargo run --example gen_minisign_keypair -- "<your-password>"
```

This writes `minisign.pub` + `minisign.key` in the current directory and prints
the public key's base64 line. (The helper uses the `minisign` crate; CI signs
with the same crate via `examples/sign_release.rs`, so the key KDF + signature
format match exactly. Don't mix in the reference `minisign` CLI - it uses a
different secret-key KDF and may not read this key.)

## 2. Bake the public key into the app

In `src/updater.rs`, set:

```rust
pub const MINISIGN_PUBKEY: &str = "<base64 line from minisign.pub>";
```

(Copy the **second line** of `minisign.pub` - the base64 string, not the
`untrusted comment:` line.)

## 3. Point the client at the manifest

In `src/updater.rs`, set `MANIFEST_URL` to where `latest.json` will be served:

```rust
pub const MANIFEST_URL: &str = "https://<your-cdn>/boundless-latest.json";
```

This must match the `CDN_BASE` repo variable below (`CDN_BASE` +
`/boundless-latest.json`).

## 4. Provision a CDN / R2 bucket

Create a Cloudflare R2 bucket (or any S3-compatible store) with a **public**
read URL, e.g. `https://cdn.example.com`. Uploads go to the bucket root; the
public URL serves them. The release workflow uploads:
- `boundless-<tag>-win-x64.zip` + `.sig`
- `boundless-<tag>-macos-arm64.app.tar.gz` + `.sig`
- `boundless-latest.json`

## 5. Set GitHub repository Variables + Secrets

In the GitHub repo **Settings → Secrets and variables → Actions**:

**Variables** (non-secret):
- `CDN_BASE` - the public CDN base URL, e.g. `https://cdn.example.com` (no
  trailing slash). Used to build the download URLs in `latest.json`.

**Secrets**:
- `R2_ENDPOINT` - R2 S3 endpoint, e.g. `https://<account>.r2.cloudflarestorage.com`
- `R2_BUCKET` - bucket name, e.g. `boundless`
- `R2_ACCESS_KEY_ID` / `R2_SECRET_ACCESS_KEY` - R2 API token with write access
- `MINISIGN_PRIVATE_KEY` - the **entire contents** of `minisign.key`
- `MINISIGN_PRIVATE_KEY_PASSWORD` - the password you set in step 1

## 6. Release flow (automatic)

Push a `v*` tag (`git tag v0.2.0 && git push github v0.2.0`). The
`release.yml` workflow:

1. **build** (Windows + macOS): `cargo build --release`, package the zip
   (manual download) + (macOS) the `.app.tar.gz` updater artifact, then sign
   the updater artifact with `cargo run --release --example sign_release`
   (same `minisign` crate as the keygen).
2. **release** (Linux): uploads artifacts + `.sig` + `boundless-latest.json`
   to R2, and creates the GitHub Release.

Running clients then see the new version on their next poll (30s after launch,
then every 4h), download + verify + swap + restart.

## Notes / caveats

- **macOS Gatekeeper**: the swapped `.app` is cleared of the `com.apple.quarantine`
  xattr (`xattr -cr`) before relaunch, but an unsigned bundle can still be
  blocked on some macOS versions. If so, sign/notarize the bundle (out of scope
  here) or have users right-click → Open once.
- **Windows exe replace**: uses the rename-aside trick (rename the running
  `boundless.exe` to `.old`, move the new one in). Requires write access to the
  exe's directory (fine for a per-user install; UAC if installed under
  `Program Files`).
- **Verification is mandatory**: until `MINISIGN_PUBKEY` is set (i.e. no longer
  the `REPLACE_ME` placeholder), `updater::verify` refuses to apply a download.
  Set the key before expecting updates to apply.
- **Manifest `notes`**: currently the release notes in `latest.json` fall back
  to `Boundless <version>`. For richer notes, edit the generate step in
  `release.yml` to read a hand-maintained `CHANGELOG.md` instead of the commit
  log.
