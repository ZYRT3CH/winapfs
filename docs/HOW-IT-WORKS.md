# How winapfs works

## Parts

| Part | What it does |
|---|---|
| `crates/apfs` (library `apfs`) | Reads APFS: checksums, GPT, container superblock and checkpoints, object maps, B-trees, inodes, directories, extents, extended attributes, transparent compression (zlib, LZVN, LZFSE). |
| `crates/winapfs` (the program) | Command line, the WinFsp file system, the launcher integration, the auto-mount service and the installer. |
| `tools/apfsprogs` | Dockerfile for an independent APFS checker (`apfsck`), for testing future write support on disk images. It is only run as a separate tool, never linked in. |

## Status

| Milestone | State |
|---|---|
| 1. Read-only commands (`info`, `ls`, `get`, `cat`, `verify`) | Verified on a real 2 TB drive: 952k metadata records, file counts match the superblock, 164 files byte-identical through two read paths, PNG CRCs pass. |
| 2. Mount read-only with WinFsp | Folder and drive-letter mounts verified on a real drive: whole tree (123,808 files), 25/25 files byte-identical including a 4 GB clip, 34 MB/s on USB 2. |
| 3. Auto-mount service and one-step installer | Built and tested. |
| 4. Write support, on disk images first, checked with `apfsck` and macOS `fsck_apfs` | Planned |
| 5. Encrypted volumes, HFS+ ("Mac OS Extended") | Planned |

## Safety rules

These hold for every milestone:

1. **Read-only by construction.** Disks are opened without write access, so Windows refuses any write. The library's `BlockDevice` trait has no write method.
2. **Corrupt data is an error, not a guess.** Every object checksum is verified, and decompressed sizes must match exactly.
3. **Writes get proven on throwaway images first.** A write path must pass an independent checker on images before it can touch a real disk.
4. **The first real-disk writes go to a spare USB stick**, never to a drive that holds the only copy of anything.

## Reading APFS

The code follows Apple's *Apple File System Reference*. Opening a volume:

1. Read block 0 (the container superblock) to find the checkpoint area. Use the newest checkpoint whose superblock and checkpoint maps all pass their checksums.
2. The container's object map translates virtual object ids into block addresses. Look up each volume superblock in it.
3. Each volume has its own object map and a file-system B-tree. That tree holds inodes, directory entries, extended attributes and file extents, all keyed by object id and record type.
4. File data comes from the file's extents. Compressed files keep their data in the `com.apple.decmpfs` attribute or the `com.apple.ResourceFork` attribute.

`winapfs verify` checks a whole volume. It scans every file-system record, which verifies every tree node's checksum, and cross-checks counts and references. It then tests a spread of real files whose formats have built-in structure: PNG CRCs, MP4/MOV box layout, ZIP central directories, JPEG markers and PDF trailers. Files up to 32 MiB are read through both the streaming path and the random-access path and compared byte for byte.

## Showing drives in Windows

winapfs implements a read-only file system with [WinFsp](https://winfsp.dev). It uses its own small binding that loads `winfsp-x64.dll` at run time. The struct layouts and size checks come from WinFsp's headers.

- **Names.** Characters that Mac names may contain but Windows forbids (`" * : < > ? \ |`, control characters, trailing spaces and dots) are shifted into the Unicode private use area at U+F000 + character. This is the convention of Cygwin and WinFsp's FUSE layer, and it maps back exactly.
- **Speed.** File data may stay in the Windows cache indefinitely, which is safe because nothing changes underneath a read-only mount. Sequential reads trigger growing read-ahead fetches from 256 KiB to 8 MiB. Without read-ahead, Windows forwards 4-8 KiB requests, and on USB each one costs a round trip.
- **Cleanup.** If the mount process dies, WinFsp removes the mount point by itself.

### Drive letters, the launcher and the auto-mount service

Reading a raw disk needs administrator rights. But a drive letter made by an elevated program is invisible to Explorer, which runs unelevated. So:

1. `winapfs setup` (run once, as administrator, by `winapfs install`):
   - copies `winapfs.exe` to `%ProgramFiles%\winapfs`, which only administrators can change
   - adds that folder to the system `PATH`
   - registers `winapfs serve` with the **WinFsp Launcher**, a service that runs as SYSTEM
   - installs the **`winapfs-automount`** service, which starts with Windows
2. `winapfs-automount` checks every 3 seconds for disks with an APFS partition. Any user may ask a disk for its partition table without reading it (`IOCTL_DISK_GET_DRIVE_LAYOUT_EX` needs no access rights). When a Mac disk appears, the service asks the launcher to mount it at the first free letter from M: onwards. When the disk goes away, it unmounts. If someone unmounts a drive while it is still plugged in, the service leaves it alone until the disk is plugged in again.
3. The launcher runs `winapfs serve src=diskN at=X:` as SYSTEM. SYSTEM can read the disk, and SYSTEM's drive letters are visible to every session.

### Security model

- The program the launcher and the service run lives where only administrators can change it. A user-writable location would let anyone swap the file and run code as SYSTEM.
- Only the account that ran setup, Administrators and SYSTEM may start or stop drives. The launcher service's SDDL grants `RPWPLC` to those three.
- `serve` accepts only whole disks (`diskN` or `auto`) and drive letters. It accepts no files, which the caller might not be allowed to read, and no folders, which would be created with SYSTEM's rights.
- Mounts are read-only. While a drive is mounted, its files are visible to every account on the PC.

## Licensing

All code is written from Apple's published specification. Nothing is copied from other APFS implementations, several of which are GPL (apfs-fuse, linux-apfs-rw) or LGPL (libfsapfs). That keeps the project under **MIT OR Apache-2.0**.

- The dependencies are permissive: `miniz_oxide`, `lzfse_rust`, `unicode-normalization`.
- WinFsp is GPLv3 with a FLOSS exception. An open-source program may link its DLL if it shows the notice "WinFsp - Windows File System Proxy, Copyright (C) Bill Zissimopoulos" with a link to https://github.com/winfsp/winfsp in its interface and documentation, and is never linked or shipped with proprietary software. winapfs shows the notice in `winapfs help`, after mounting, and in the README.
- winapfs does not use the `winfsp` Rust crate, which is GPL-3.0 and would make the whole program GPL. It loads the DLL itself instead.
