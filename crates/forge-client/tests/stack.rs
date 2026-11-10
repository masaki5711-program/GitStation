#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)] // tests may fail loudly
//! The `gstn` binary's main-thread stack. On Windows the linker reserves
//! 1 MiB for it unless told otherwise; clap's derived command tree alone
//! needs close to that in a debug build (one stack frame of ~300 KiB per
//! subcommand level), and `gstn` crashed with STATUS_STACK_OVERFLOW
//! (exit -1073741571) on every command, `--help` included. build.rs
//! reserves the same 8 MiB Linux gives a main thread.

/// SizeOfStackReserve of a PE32+ image (Windows x64 executable).
#[cfg(windows)]
fn stack_reserve(image: &[u8]) -> u64 {
    let u32_at = |at: usize| u32::from_le_bytes(image[at..at + 4].try_into().unwrap());
    let pe = u32_at(0x3c) as usize;
    assert_eq!(&image[pe..pe + 4], b"PE\0\0", "not a PE image");
    let optional = pe + 4 + 20;
    let magic = u16::from_le_bytes(image[optional..optional + 2].try_into().unwrap());
    assert_eq!(magic, 0x20b, "not PE32+");
    u64::from_le_bytes(image[optional + 72..optional + 80].try_into().unwrap())
}

#[cfg(windows)]
#[test]
fn gstn_reserves_a_main_thread_stack_as_large_as_linux_gives() {
    let image = std::fs::read(env!("CARGO_BIN_EXE_gstn")).expect("gstn image");
    assert!(
        stack_reserve(&image) >= 8 * 1024 * 1024,
        "reserve {} bytes",
        stack_reserve(&image)
    );
}

#[test]
fn gstn_help_runs_without_overflowing_its_stack() {
    let o = std::process::Command::new(env!("CARGO_BIN_EXE_gstn"))
        .arg("--help")
        .env_remove("GSTN_TOKEN")
        .output()
        .expect("gstn");
    assert_eq!(
        o.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
}
