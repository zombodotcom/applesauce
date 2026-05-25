# applesauce

[![license: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)
[![latest release](https://img.shields.io/github/v/release/zombodotcom/applesauce?include_prereleases)](https://github.com/zombodotcom/applesauce/releases)

Read Mac hard drives on Windows. Free and open source.

`applesauce` mounts **HFS+** and **APFS** volumes as Windows drive letters
so you can browse, copy, and recover files using Windows Explorer (or any
other Windows tool). No kernel driver, no commercial license, no Mac
required. **Read-only** — it never writes to the Mac disk.

## Status

Both **HFS+** and **APFS** read solidly on Windows 10 / 11, from real
disks or image files. Mount, browse, and recover (pull) all work.

What's supported on APFS today:

- Multi-volume containers (each volume mounts as its own drive letter)
- Directory listing, file metadata, and file reads
- **decmpfs** transparent compression: zlib and LZFSE files decompress
  automatically (inline and resource-fork)
- **FileVault**-encrypted volumes are detected and clearly flagged
  (reading them needs the key, which isn't supported)
- **4Kn** drives (4096-byte logical sectors) — common on large USB disks

Not yet: LZVN-compressed files (some macOS system files) are detected
but not decoded; writing; decrypting FileVault volumes.

## Design

Workspace layout:

- **`block-source`** — `Read + Seek` over physical disks
  (`\\.\PhysicalDriveN`, with logical-sector-size detection) or image
  files. GPT and APM partition parsing.
- **`fs-core`** — `MacFilesystem` trait plus pure-Rust **HFS+** and
  **APFS** readers (B-trees, object map, extents, decmpfs).
- **`winfsp-bridge`** — adapts an `fs-core` reader to
  [WinFsp](https://winfsp.dev/) so a volume appears as a drive letter.
- **`applesauce-parts`** — CLI: dump GPT / APM partition tables and
  summarize APFS containers.
- **`applesauce-cat`** — CLI: peek inside a volume without mounting
  (HFS+ `info`/`ls`/`cat`/`pull`; APFS `apfs`/`apfs-ls`/`apfs-cat`/
  `apfs-pull`).
- **`applesauce-mount`** — CLI: mount one volume as a drive letter via
  WinFsp; also registers the unprivileged-mount launcher service.
- **`applesauce-gui`** — `egui` app: scan disks, mount/unmount, browse
  the tree, and pull selected folders with progress.

## Requirements

- Windows 10 or 11 (x86_64)
- [WinFsp 2.0+](https://winfsp.dev/rel/) for mounting (not needed for
  `applesauce-cat` / `applesauce-parts`)
- Administrator to read raw physical disks (not needed for image files).
  Mounting itself, once the service is installed, does **not** need
  admin.

## Install

Download the latest Windows zip from the
[Releases page](https://github.com/zombodotcom/applesauce/releases),
extract it, and run the binaries. Install WinFsp from
<https://winfsp.dev/rel/> if you want to mount.

## Usage (GUI)

Run `applesauce.exe` as Administrator (needed once to scan raw disks):

1. Click **Install service** (one UAC prompt) — registers the mount
   launcher so volumes can be mounted without admin afterwards. Re-run
   this after upgrading; the launcher command template can change
   between versions.
2. Each Mac volume appears as a row (HFS+, or one row per APFS volume).
3. Pick a free drive letter and click **Mount** — the volume shows up in
   Explorer. Or click **Browse…** to pick folders and **Pull** them to a
   destination you choose (restartable; optional "skip files already at
   destination").

## Usage (CLI)

Find your Mac drive (run as Administrator):

```powershell
applesauce-parts.exe --disk 0   # repeat for each disk number
```

HFS+ — peek without mounting:

```powershell
applesauce-cat.exe --disk 4 info
applesauce-cat.exe --disk 4 ls /Users
applesauce-cat.exe --disk 4 cat "/Users/dave/notes.txt"
applesauce-cat.exe --disk 4 pull /Users D:\recovery   # recover a tree
```

APFS — a container holds several named volumes:

```powershell
applesauce-cat.exe --disk 4 apfs                       # list volumes
applesauce-cat.exe --disk 4 apfs-ls   "Macintosh HD - Data" /Users
applesauce-cat.exe --disk 4 apfs-cat  "Macintosh HD - Data" /Users/dave/notes.txt
applesauce-cat.exe --disk 4 apfs-pull "Macintosh HD - Data" /Users/dave D:\recovery
```

`pull` / `apfs-pull` are restartable: they skip destination files whose
size and mtime already match, and write each in-flight file to a
`*.applesauce-partial` temp before renaming on success. Add
`--skip-existing` to skip any file already present by name.

Mount as a drive letter (Administrator + WinFsp):

```powershell
applesauce-mount.exe --disk 4 Z:        # auto-pick the first Mac volume
# Z:\ now browsable in Explorer. Ctrl-C to unmount.
applesauce-mount.exe C:\path\to\mac.dmg Z:   # or mount an image
```

## Build from source

```powershell
# Default lane (no WinFsp deps): applesauce-cat, applesauce-parts, applesauce-gui
cargo build --release

# Mount binary (needs WinFsp SDK + LLVM/libclang):
cargo build --release -p applesauce-mount
```

The `applesauce-mount` build pulls
[`winfsp-rs`](https://github.com/snowflakepowered/winfsp-rs), which uses
`bindgen` against the WinFsp SDK headers. Install
[LLVM](https://releases.llvm.org/) (for `libclang.dll`) and
[WinFsp](https://winfsp.dev/rel/) before building it.

## Troubleshooting

- **"The parameter is incorrect" reading a disk** — historically a 4Kn
  (4096-byte sector) drive issue; fixed in pre.7. Update to the latest
  release.
- **GUI says the mount service is "out of date"** — click **Install
  service** again; the launcher command template changed.
- **Mount fails with a delay-load / WinFsp DLL error** — install WinFsp
  from <https://winfsp.dev/rel/>; the bridge adds its `bin` dir to PATH
  automatically once installed.

## Changelog

See [CHANGELOG.md](CHANGELOG.md) for the running build/feature log.
Releases are published manually to the
[Releases page](https://github.com/zombodotcom/applesauce/releases) with
a Windows x86_64 zip.

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
