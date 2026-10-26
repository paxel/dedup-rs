//! dedup-core
//! Domain logic for file hashing, directory scanning, and indexing.

#![forbid(unsafe_code)]
// Shipped code may not panic: no unwrap, expect or panic, no unreachable, no
// indexing or slicing that can go out of bounds, no arithmetic that can overflow.
// Unit tests are exempt via `cfg(test)`; integration tests are separate crates.
#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::todo,
        clippy::unimplemented,
        clippy::unreachable,
        clippy::indexing_slicing,
        clippy::string_slice,
        clippy::arithmetic_side_effects
    )
)]

pub mod align;
pub mod archive;
pub mod diff;
pub mod dupes;
pub mod eta;
pub mod filter;
pub mod fingerprint;
pub mod groom;
pub mod logging;
pub mod organize;
pub mod render;
pub mod report;
pub mod scan;
pub mod secret;
pub mod similar;
pub mod store;
pub mod strings;
pub mod sync_group;
pub mod thumbnail;
pub mod update;

pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
