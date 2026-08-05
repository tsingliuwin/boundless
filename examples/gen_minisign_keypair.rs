//! One-time minisign keypair generator for the auto-updater.
//!
//! Usage:
//!   cargo run --example gen_minisign_keypair -- "<password>"
//!
//! Writes `minisign.pub` and `minisign.key` in the current directory, then
//! prints the public key's base64 line - paste it into `src/updater.rs`'s
//! `MINISIGN_PUBKEY` constant.
//!
//! Then set the GitHub secrets:
//!   MINISIGN_PRIVATE_KEY          <- the entire contents of `minisign.key`
//!   MINISIGN_PRIVATE_KEY_PASSWORD <- the password you passed above
//!
//! CI signs releases with `examples/sign_release.rs` using the *same* `minisign`
//! crate, so the secret-key KDF + signature format match exactly. (The reference
//! `minisign` CLI uses a different secret-key KDF and may not read this key, so
//! we don't mix the CLI with this crate.)
//!
//! Why not just install the `minisign` CLI? It works too, but its GitHub release
//! download is unreachable from some networks; this helper needs only `cargo`
//! (which can reach crates.io).

use std::fs::File;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let password = std::env::args()
        .nth(1)
        .expect("usage: cargo run --example gen_minisign_keypair -- <password>");

    let pk = File::create("minisign.pub")?;
    let sk = File::create("minisign.key")?;
    minisign::KeyPair::generate_and_write_encrypted_keypair(
        pk,
        sk,
        Some("boundless updater key"),
        Some(password),
    )?;

    let pub_text = std::fs::read_to_string("minisign.pub")?;
    let base64 = pub_text.lines().nth(1).unwrap_or("");
    println!("\nWrote minisign.pub + minisign.key\n");
    println!("Paste this into src/updater.rs MINISIGN_PUBKEY:");
    println!("    {base64}\n");
    println!("CI secrets:");
    println!("  MINISIGN_PRIVATE_KEY          <- contents of minisign.key");
    println!("  MINISIGN_PRIVATE_KEY_PASSWORD <- the password you just used");
    Ok(())
}
