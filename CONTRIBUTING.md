# Contributing

Thank you for helping. A few rules keep the project safe and its license clean.

## The one rule about code

**Do not copy code from other APFS projects.** Several are GPL (apfs-fuse, linux-apfs-rw) or LGPL (libfsapfs). Copying from them would change winapfs's license. Work from Apple's published [Apple File System Reference](https://developer.apple.com/support/downloads/Apple-File-System-Reference.pdf) instead. Running other tools to test against is fine (see `tools/apfsprogs`).

## Safety

winapfs must never damage a drive. Read the [safety rules](docs/HOW-IT-WORKS.md#safety-rules) before you touch anything that reads disks, and especially before anything that could write.

## Build and test

You need [Rust](https://rustup.rs) on Windows. Either toolchain works: `stable-x86_64-pc-windows-msvc` with Visual Studio Build Tools, or `stable-x86_64-pc-windows-gnu`, which brings its own linker.

```
cargo test
cargo build --release
```

The program is then at `target\release\winapfs.exe`.

### Testing the mount without a Mac drive

Install [WinFsp](https://winfsp.dev), then:

```
target\release\winapfs.exe mount-demo C:\demo
python scripts\demo-check.py C:\demo
target\release\winapfs.exe unmount C:\demo
```

`mount-demo` serves a small built-in tree that has the awkward parts of real Mac drives: names Windows forbids, non-ASCII names, a hidden file, a 10 MB file and a folder with 2,000 entries. `demo-check.py` runs 22 checks through ordinary Windows file access. Neither needs administrator rights.

Set `WINAPFS_DEBUG=1` before mounting to log every WinFsp request to stderr.

### Testing on a real Mac drive

Use an Administrator terminal:

```
winapfs disks
winapfs verify auto
```

`verify` only reads. Please include its output when you report a problem.

## Pull requests

- Keep changes focused, and say what you tested.
- `cargo test` must pass with no warnings.
- Match the style of the surrounding code.
