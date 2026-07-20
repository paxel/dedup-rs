//! dedup-core
//! Domain logic for file hashing, directory scanning, and indexing.

pub mod archive;
pub mod diff;
pub mod dupes;
pub mod eta;
pub mod filter;
pub mod fingerprint;
pub mod groom;
pub mod logging;
pub mod organize;
pub mod report;
pub mod scan;
pub mod similar;
pub mod store;
pub mod sync_group;
pub mod thumbnail;
pub mod update;

pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
