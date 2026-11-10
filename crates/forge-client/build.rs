//! The `gstn` binary's main-thread stack: 8 MiB, as Linux gives a main
//! thread. Windows linkers reserve 1 MiB by default, and clap's derived
//! command tree needs close to that in a debug build (a ~300 KiB frame per
//! subcommand level, growing with every command): gstn crashed with
//! STATUS_STACK_OVERFLOW on every command (tests/stack.rs). A link-time
//! reserve adds no failure path at run time, unlike moving `run` onto a
//! spawned thread. Only the gstn binary is affected, not the library.
fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if os != "windows" {
        return;
    }
    const STACK: u32 = 8 * 1024 * 1024;
    if env == "msvc" {
        println!("cargo:rustc-link-arg-bin=gstn=/STACK:{STACK}");
    } else {
        // windows-gnu / gnullvm: GNU ld or lld in MinGW mode.
        println!("cargo:rustc-link-arg-bin=gstn=-Wl,--stack,{STACK}");
    }
}
