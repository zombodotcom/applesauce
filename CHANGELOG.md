# Changelog

All notable changes to **applesauce** are recorded here. Format roughly
follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the
project uses [Semantic Versioning](https://semver.org/). Until v1.0.0 the
API and on-disk-format surface may shift between minor versions.

## [Unreleased]

## [0.1.0-pre.6] — 2026-05-11

### Added
- **HFS+ extents-overflow B-tree.** Catalog file records carry 8 inline
  extents; anything beyond that lived in the overflow B-tree we
  hadn't read yet. `crates/fs-core/src/hfsplus/extents.rs`
  parses overflow keys / records and exposes
  `ExtentsBTree::resolve_full_extents`, which Hfsplus opens at
  startup and consults on `read_file_range` whenever
  `data_fork.is_fully_inline()` is false. Real Mac drives with
  fragmented files (system files, large packages) now read past the
  8-extent boundary instead of erroring.
- **`ForkReader::with_extents`.** Takes a fully-resolved
  `Vec<HFSPlusExtentDescriptor>` so the reader doesn't need to know
  whether the data came from inline or overflow extents.
  `ForkReader::from_fork` stays as the inline-only convenience for
  the four HFS+ special files.
- **WinFsp bridge.** `winfsp_bridge::Bridge` adapts a
  `fs_core::MacFilesystem` to a WinFsp `FileSystemContext`. Read-only
  (`read_only_volume` set on `VolumeParams`; no
  `create`/`write`/`delete` surfaces). Serializes calls via
  `Arc<Mutex<dyn MacFilesystem>>` since our reader is single-Seek.
  Gated behind the `mount` Cargo feature so the rest of the
  workspace builds without WinFsp installed.
- **`applesauce-mount` CLI** (Windows-only): `--disk N <letter>` or
  `<image> <letter>`. Ctrl-C unmounts. `winfsp_build` build script
  handles the WinFsp delay-load wiring so the binary loads without
  WinFsp's bin dir on `PATH`.
- **`applesauce-gui`**: real scan / mount / unmount UI. Scans
  physical disks, lists Mac-typed partitions, lets you pick a free
  drive letter and mount via the bridge. Background threads for scan
  and mount so the UI never freezes. Warns when not elevated.
- **`default-members`** excludes `applesauce-mount` and
  `applesauce-gui`, so `cargo build` from the root works without
  WinFsp + LLVM. Build them explicitly with `cargo build -p
  applesauce-mount` / `-p applesauce-gui`.

### Verified
- `applesauce-cat --disk N` reads a live Mac OS X 10.11.6 HFS+ system
  volume end-to-end (GPT probe → volume header → catalog → fork
  reader → file content).
- 29 unit tests pass across the workspace (`cargo test`).
- `cargo build --release` (default + WinFsp lanes) both produce
  stripped release binaries.

## [0.1.0-pre.5] — 2026-05-11

### Added
- `applesauce-cat --disk N` (Windows): point the HFS+ reader at a real
  Mac drive plugged into the host (`\\.\PhysicalDriveN`). Requires
  Administrator.

## [0.1.0-pre.4]

### Added
- HFS+ catalog traversal (B-tree descent with leaf-spill, case-aware
  key compare, root-folder thread record decode for volume label).
- `MacFilesystem` implementation for HFS+: `list_dir`, `stat`,
  `read_file_range`, `volume_label`.
- `applesauce-cat` CLI: `info` / `ls [path]` / `cat <path>` over an
  image file.

## [0.1.0-pre.3]

### Added
- `HFSPlusVolumeHeader` parser (1024-byte offset, big-endian, signature
  check, case-sensitivity bit).
- HFS+ fork reader: walks `HFSPlusExtentDescriptor` lists to expose a
  fork as `Read + Seek`.
- B-tree node descriptor + record-offset table parsing.

## [0.1.0-pre.2]

### Added
- `block-source` crate: `BlockSource: Read + Seek + Send` trait;
  `ImageFile`, `Window` adapter, and Windows `PhysicalDisk` opener.
- GPT and APM partition-table parsers with Mac-type detection.
- `applesauce-parts` CLI: dump the partition table of an image or
  `\\.\PhysicalDriveN`.

## [0.1.0-pre.1]

### Added
- Workspace scaffold: `block-source`, `fs-core`, `winfsp-bridge` crates
  plus `applesauce-gui` app.
- Dual MIT / Apache-2.0 license.

[Unreleased]: https://github.com/zombodotcom/applesauce/compare/v0.1.0-pre.6...HEAD
[0.1.0-pre.6]: https://github.com/zombodotcom/applesauce/compare/v0.1.0-pre.5...v0.1.0-pre.6
[0.1.0-pre.5]: https://github.com/zombodotcom/applesauce/compare/v0.1.0-pre.4...v0.1.0-pre.5
[0.1.0-pre.4]: https://github.com/zombodotcom/applesauce/compare/v0.1.0-pre.3...v0.1.0-pre.4
[0.1.0-pre.3]: https://github.com/zombodotcom/applesauce/compare/v0.1.0-pre.2...v0.1.0-pre.3
[0.1.0-pre.2]: https://github.com/zombodotcom/applesauce/compare/v0.1.0-pre.1...v0.1.0-pre.2
[0.1.0-pre.1]: https://github.com/zombodotcom/applesauce/releases/tag/v0.1.0-pre.1
