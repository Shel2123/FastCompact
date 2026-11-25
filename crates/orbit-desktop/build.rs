//! Embeds `orbit.exe.manifest` into orbit.exe on Windows (MSVC), so the
//! process is Per-Monitor v2 DPI aware before any code runs.

use std::env;
use std::path::Path;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=orbit.exe.manifest");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    if env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("msvc") {
        // orbit-windows still asks for the DPI mode at startup.
        println!("cargo:warning=orbit.exe is built without its manifest on this target");
        return;
    }
    let dir = env::var("CARGO_MANIFEST_DIR").unwrap_or_default();
    let manifest = Path::new(&dir).join("orbit.exe.manifest");
    println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
    println!(
        "cargo:rustc-link-arg-bins=/MANIFESTINPUT:{}",
        manifest.display()
    );
}
