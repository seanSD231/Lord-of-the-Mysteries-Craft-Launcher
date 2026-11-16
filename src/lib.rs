//! The launcher's machinery, as a library.
//!
//! Two binaries use it: `lotmcraft` (the command line) and `lotmcraft-gui` (the window).
//! Keeping it here rather than inside one of them means the window and the command line
//! cannot drift apart -- they run the same install, the same sync, the same launch.

pub mod auth;
pub mod fetch;
pub mod java;
pub mod launch;
pub mod manifest;
pub mod mojang;
pub mod neoforge;
pub mod ping;
pub mod policy;
pub mod sync;
pub mod update;
