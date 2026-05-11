//! WinFsp glue layer.
//!
//! Adapts a `fs_core::MacFilesystem` to a WinFsp `FileSystemContext`,
//! letting Windows mount Mac volumes as drive letters.
//!
//! Stubbed for the initial scaffold; real implementation lands once
//! the HFS+ reader is end-to-end.

#![deny(rust_2018_idioms)]
