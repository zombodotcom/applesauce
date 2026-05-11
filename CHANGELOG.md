# Changelog

All notable changes to **applesauce** are recorded here. Format roughly
follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the
project uses [Semantic Versioning](https://semver.org/). Until v1.0.0 the
API and on-disk-format surface may shift between minor versions.

## [Unreleased]

### Added
- WinFsp bridge in progress: adapts `fs_core::MacFilesystem` to a
  WinFsp `FileSystemContext`, exposing an HFS+ volume as a Windows
  drive letter. Read-only.
- `applesauce-mount` CLI: mount a Mac drive (`--disk N`) or image file
  to a chosen drive letter.

### Verified
- `applesauce-cat --disk 4` reads a live Mac OS X 10.11.6 (El Capitan)
  HFS+ system volume end-to-end: GPT partition probe → volume header
  → catalog B-tree → fork reader → file content
  (e.g. `SystemVersion.plist`).

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

[Unreleased]: https://github.com/zombodotcom/applesauce/compare/v0.1.0-pre.5...HEAD
[0.1.0-pre.5]: https://github.com/zombodotcom/applesauce/compare/v0.1.0-pre.4...v0.1.0-pre.5
[0.1.0-pre.4]: https://github.com/zombodotcom/applesauce/compare/v0.1.0-pre.3...v0.1.0-pre.4
[0.1.0-pre.3]: https://github.com/zombodotcom/applesauce/compare/v0.1.0-pre.2...v0.1.0-pre.3
[0.1.0-pre.2]: https://github.com/zombodotcom/applesauce/compare/v0.1.0-pre.1...v0.1.0-pre.2
[0.1.0-pre.1]: https://github.com/zombodotcom/applesauce/releases/tag/v0.1.0-pre.1
