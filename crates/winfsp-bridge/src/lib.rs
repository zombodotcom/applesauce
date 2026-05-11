//! WinFsp glue layer.
//!
//! Adapts a `fs_core::MacFilesystem` to a WinFsp `FileSystemContext`,
//! exposing the volume as a Windows drive letter.
//!
//! The actual bridge is gated behind the `mount` Cargo feature so that
//! the rest of the workspace builds without WinFsp installed.

#![deny(rust_2018_idioms)]

#[cfg(all(windows, feature = "mount"))]
mod bridge;

#[cfg(all(windows, feature = "mount"))]
pub use bridge::{mount, Bridge, MountedHost};
