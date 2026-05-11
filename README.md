# applesauce

[![ci](https://github.com/zombodotcom/applesauce/actions/workflows/ci.yml/badge.svg)](https://github.com/zombodotcom/applesauce/actions/workflows/ci.yml)
[![release](https://github.com/zombodotcom/applesauce/actions/workflows/release.yml/badge.svg)](https://github.com/zombodotcom/applesauce/actions/workflows/release.yml)
[![license: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

Read Mac hard drives on Windows. Free and open source.

`applesauce` mounts HFS+ and APFS volumes as Windows drive letters so you
can browse, copy, and recover files using Windows Explorer (or any other
Windows tool). No kernel driver, no commercial license, no Mac required.

## Status

Early development. The project is being built in the open over ~6 weeks
toward a v1 release that handles HFS+ and APFS solidly on Windows 10 and
11. Read-only for now.

## Design

Workspace layout:

- **`block-source`** — `Read + Seek` over physical disks
  (`\\.\PhysicalDriveN`) or image files. GPT and APM partition parsing.
- **`fs-core`** — `MacFilesystem` trait + pure-Rust HFS+ reader
  (APFS reader is a stub).
- **`winfsp-bridge`** — adapts an `fs-core` reader to
  [WinFsp](https://winfsp.dev/) so volumes appear as drive letters.
- **`applesauce-cat`** — CLI: peek inside an HFS+ volume without
  mounting (`info` / `ls` / `cat`).
- **`applesauce-parts`** — CLI: dump GPT / APM partition tables.
- **`applesauce-mount`** — CLI: mount a Mac drive as a Windows drive
  letter via WinFsp.
- **`applesauce-gui`** — `egui` mount manager (scaffold).

## Requirements

- Windows 10 or 11 (x86_64)
- [WinFsp 2.0+](https://winfsp.dev/rel/) for mounting (not needed for
  `applesauce-cat` / `applesauce-parts`)
- Administrator privileges to read raw physical disks (not needed for
  image files)

## Install

Download the latest Windows zip from the
[Releases page](https://github.com/zombodotcom/applesauce/releases),
extract it, and run the binaries. WinFsp is required at runtime for
`applesauce-mount`; install it from <https://winfsp.dev/rel/>.

## Usage

Find your Mac drive (run as Administrator):

```powershell
applesauce-parts.exe --disk 0  # repeat for each disk number
```

Mac drives show GPT entries with type GUID `4846…ECAC` (HFS+) or
APM "Apple_HFS" entries.

Peek without mounting:

```powershell
applesauce-cat.exe --disk 4 info
applesauce-cat.exe --disk 4 ls /Users
applesauce-cat.exe --disk 4 cat "/System/Library/CoreServices/SystemVersion.plist"
```

Mount as a drive letter (Administrator + WinFsp):

```powershell
applesauce-mount.exe --disk 4 Z:
# Z:\ now browsable in Explorer. Ctrl-C to unmount.
```

Mount from an image instead:

```powershell
applesauce-mount.exe C:\path\to\mac.dmg Z:
```

## Build from source

```powershell
# Default lane (no WinFsp deps):
cargo build --release
# Includes: applesauce-cat, applesauce-parts, applesauce-gui

# Mount binary (needs WinFsp SDK + LLVM/libclang):
cargo build --release -p applesauce-mount
```

The `applesauce-mount` build pulls
[`winfsp-rs`](https://github.com/snowflakepowered/winfsp-rs), which
uses `bindgen` against the WinFsp SDK headers. Install
[LLVM](https://releases.llvm.org/) (for `libclang.dll`) and
[WinFsp](https://winfsp.dev/rel/) before building the mount binary.

## Changelog

See [CHANGELOG.md](CHANGELOG.md) for the running build/feature log.
Tagged releases produce a draft GitHub Release with a Windows x86_64
zip on the [Releases page](https://github.com/zombodotcom/applesauce/releases).

## License

Dual-licensed under either of:

- [MIT license](LICENSE-MIT)
- [Apache License, Version 2.0](LICENSE-APACHE)

at your option.

### Why two licenses?

This is the standard convention in the Rust ecosystem. It maximizes
compatibility with downstream projects.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in the work by you, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms
or conditions.
