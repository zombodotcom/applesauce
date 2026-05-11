# Changelog

All notable changes to **applesauce** are recorded here. Format roughly
follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the
project uses [Semantic Versioning](https://semver.org/). Until v1.0.0 the
API and on-disk-format surface may shift between minor versions.

## [Unreleased]

### Added
- WinFsp bridge: `winfsp_bridge::Bridge` adapts a
  `fs_core::MacFilesystem` to a WinFsp `FileSystemContext`, exposing
  the volume as a Windows drive letter. Read-only — `read_only_volume`
  set on the WinFsp volume params, no `create`/`write`/`delete`
  surfaces. Serializes calls via `Arc<Mutex<dyn MacFilesystem>>` since
  our reader is single-Seek.
- `applesauce-mount` CLI (Windows-only): `--disk N <letter>` or
  `<image> <letter>`. Ctrl-C unmounts.
- `winfsp-bridge` crate gates its WinFsp deps behind a `mount` feature
  (off by default) so `cargo check` from the root keeps working
  without WinFsp installed.
- `default-members` in workspace `Cargo.toml` excludes
  `applesauce-mount` so `cargo build` from the root doesn't pull
  WinFsp; explicit `-p applesauce-mount` opts in.
- Release workflow installs LLVM + WinFsp on the runner, builds all
  binaries, and drafts a GitHub Release with a zipped artifact.

### Removed
- GitHub Actions CI workflow. Running `cargo fmt --check`, `cargo
  clippy`, and `cargo test` locally before commit is enough for this
  size of project.

### Verified
- `applesauce-cat --disk 4` reads a live Mac OS X 10.11.6 (El Capitan)
  HFS+ system volume end-to-end: GPT partition probe → volume header
  → catalog B-tree → fork reader → file content
  (e.g. `SystemVersion.plist`).
- `cargo check` on the default workspace (no WinFsp) is clean.

### Pending
- End-to-end mount test on a real Mac drive (requires installing
  WinFsp + LLVM locally; CI will validate the build path).

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
