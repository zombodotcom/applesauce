# applesauce

Read Mac hard drives on Windows. Free and open source.

`applesauce` mounts HFS+ and APFS volumes as Windows drive letters so you
can browse, copy, and recover files using Windows Explorer (or any other
Windows tool). No kernel driver, no commercial license, no Mac required.

## Status

Early development. The project is being built in the open over ~6 weeks
toward a v1 release that handles HFS+ and APFS solidly on Windows 10 and
11. Read-only for now.

## Design

Four-piece workspace:

- **`block-source`** — `Read + Seek` over physical disks
  (`\\.\PhysicalDriveN`) or image files. GPT and APM partition parsing.
- **`fs-core`** — `MacFilesystem` trait + pure-Rust HFS+ and APFS
  readers.
- **`winfsp-bridge`** — adapts an `fs-core` reader to
  [WinFsp](https://winfsp.dev/) so volumes appear as drive letters.
- **`applesauce-gui`** — small `egui` mount manager: scan, mount,
  unmount.

## Requirements

- Windows 10 or 11
- [WinFsp](https://winfsp.dev/) (bundled by the installer)
- Administrator privileges to read raw physical disks (not needed for
  image files)

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
