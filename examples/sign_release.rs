//! Sign a release artifact with the minisign secret key. Used by CI
//! (release.yml) to produce the `<artifact>.sig` files embedded in
//! `latest.json`.
//!
//! Usage:
//!   cargo run --release --example sign_release -- <file> <minisign.key> <password>
//!
//! Writes `<file>.sig` next to the input. Uses the same `minisign` crate as the
//! keypair generator (`examples/gen_minisign_keypair.rs`), so the secret-key KDF
//! matches. The signature itself is standard minisign, which the app verifies
//! with the `minisign-verify` crate.

use std::fs::File;
use std::io::Write;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let file = args
        .next()
        .expect("usage: sign_release <file> <minisign.key> <password>");
    let key_path = args.next().expect("missing <minisign.key> path");
    let password = args.next().expect("missing <password>");

    let sk = minisign::SecretKey::from_file(&key_path, Some(password))?;
    let mut data = File::open(&file)?;
    let sig_box = minisign::sign(None, &sk, &mut data, None, None)?;

    let sig_path = format!("{file}.sig");
    let mut out = File::create(&sig_path)?;
    write!(out, "{}", sig_box.to_string())?;
    println!("wrote {sig_path}");
    Ok(())
}
