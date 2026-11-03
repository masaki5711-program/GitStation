//! `sqlx::migrate!` embeds `migrations/` at compile time, but cargo does not
//! know a new migration file is an input: without this, an artifact built
//! before a migration was added keeps the old list (found in M3 R6: the
//! workspace's `forge` binary missed 20261112100200 while the per-crate
//! build had it, and doctor refused the schema).
fn main() {
    println!("cargo:rerun-if-changed=migrations");
}
