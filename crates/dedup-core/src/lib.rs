//! dedup-core
//! Domain logic for file hashing, directory scanning, and indexing.

pub mod store;
pub mod update;

pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
