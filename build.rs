//! Build script.
//!
//! On Windows, embed `assets/icon.rc` so the executable ships with an
//! application icon under resource id 1 — the id GPUI's Windows backend
//! requests via `LoadImageW` for the taskbar / title-bar / alt-tab icon.

fn main() {
    #[cfg(target_os = "windows")]
    {
        let rc = "assets/icon.rc";
        println!("cargo:rerun-if-changed={rc}");
        println!("cargo:rerun-if-changed=assets/icon.ico");
        embed_resource::compile(rc, embed_resource::NONE).manifest_optional().ok();
    }
}
