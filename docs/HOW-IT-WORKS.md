# How winapfs works

## Parts

| Part | What it does |
|---|---|
| `crates/apfs` (library `apfs`) | Reads APFS: checksums, GPT, container superblock and checkpoints, object maps, B-trees, inodes, directories, extents, extended attributes, transparent compression (zlib, LZVN, LZFSE). Writes APFS (`apfs::write`): copy-on-write B-trees, object maps, the space manager and free queues, checkpoints, files and folders. |
| `crates/winapfs` (the program) | Command line, the WinFsp file system, the launcher integration, the auto-mount service and the installer. |
| `tools/apfsprogs` | Dockerfile for an independent APFS checker (`apfsck`), used to check write support on disk images. It is only run as a separate tool, never linked in. |

## Status

| Milestone | State |
|---|---|
| 1. Read-only commands (`info`, `ls`, `get`, `cat`, `verify`) | Verified on a real 2 TB drive: 952k metadata records, file counts match the superblock, 164 files byte-identical through two read paths, PNG CRCs pass. |
| 2. Mount read-only with WinFsp | Folder and drive-letter mounts verified on a real drive: whole tree (123,808 files), 25/25 files byte-identical including a 4 GB clip, 34 MB/s on USB 2. |
| 3. Auto-mount service and one-step installer | Built and tested. |
| 4. Write support | **Beta.** Create, write, overwrite, truncate, rename, move and delete files and folders, through the mount or the library. Checked with `apfsck` and byte-for-byte against NTFS on images, rehearsed against the metadata of a real macOS 15 container, and used on a real 1.8 TB drive. Not yet checked with macOS `fsck_apfs`. |
| 5. Encrypted volumes, HFS+ ("Mac OS Extended") | Planned |

## Safety rules

1. **Read-only unless asked.** Without `--write` (or `winapfs write on`), disks are opened without write access, so Windows refuses any write.
2. **Corrupt data is an error, not a guess.** Every object checksum is verified, and decompressed sizes must match exactly.
3. **Copy-on-write only.** Nothing the newest checkpoint uses is ever overwritten; see [Writing APFS](#writing-apfs).
4. **Check before writing.** A writable mount first loads everything a transaction needs (refusing what it does not understand) and checks the free-space map against everything that points at blocks. If anything is off, the drive is shown read-only instead.
5. **New write paths are proven on throwaway images first**, with an independent checker, before they touch a real disk.

## Reading APFS

The code follows Apple's *Apple File System Reference*. Opening a volume:

1. Read block 0 (the container superblock) to find the checkpoint area. Use the newest checkpoint whose superblock and checkpoint maps all pass their checksums.
2. The container's object map translates virtual object ids into block addresses. Look up each volume superblock in it.
3. Each volume has its own object map and a file-system B-tree. That tree holds inodes, directory entries, extended attributes and file extents, all keyed by object id and record type.
4. File data comes from the file's extents. Compressed files keep their data in the `com.apple.decmpfs` attribute or the `com.apple.ResourceFork` attribute.

`winapfs verify` checks a whole volume. It scans every file-system record, which verifies every tree node's checksum, and cross-checks counts and references. It then tests a spread of real files whose formats have built-in structure: PNG CRCs, MP4/MOV box layout, ZIP central directories, JPEG markers and PDF trailers. Files up to 32 MiB are read through both the streaming path and the random-access path and compared byte for byte.

## Writing APFS

APFS never changes live metadata in place. A **transaction** writes new copies of whatever it changes into free blocks, then a new **checkpoint**: the ephemeral objects (space manager, free queues, reaper) in the checkpoint data area, a checkpoint map and a new container superblock in the descriptor area. Until that superblock is written, the previous checkpoint is still the newest valid one, so a crash loses the transaction, never the container. winapfs writes in that order, with a flush before and after the superblock.

- **B-trees** are edited copy-on-write: only the paths touched are loaded; full nodes split, empty ones go, and changed nodes are written to new blocks. Physical trees (object maps, the extent-reference tree) rewrite the path to the root; virtual trees (the file-system tree) are re-pointed through the volume's object map.
- **The space manager** tracks free blocks in per-chunk bitmaps, kept with the chunk-info blocks in its internal pool, which has its own bitmap in a ring of blocks. Changed bitmaps go to new internal-pool blocks. A freed block goes into a **free queue** with the transaction's id and stays allocated until the next transaction releases it, so the previous checkpoint stays intact.
- **Files** being written keep a block map in memory. New data goes straight to newly allocated blocks (in stretches of 8 MiB or more where possible, so files do not fragment), queued into large writes; blocks the committed state uses are replaced copy-on-write and released through the extent-reference tree. Holes in sparse files are stored as Apple expects: extents at block 0, the sparse flag and the sparse-bytes field.
- **The mount** collects changes in one transaction and commits after a quiet second, at most every 5 seconds, on flush and at unmount. Listings and lookups see uncommitted changes.

Tools for checking this, all in `winapfs`:

| Command | What it does |
|---|---|
| `dev-space-check <source>` | Rebuilds which blocks are in use from everything that points at blocks (trees, object maps, extents, checkpoint areas, free queues, internal pool) and compares with the bitmaps. A block in use but marked free would be handed out again later: data loss. |
| `mount <source> X: --rehearse` | A writable mount whose writes stay in memory: the write code runs against a real disk without changing it. |
| `dev-rehearse <source> <script> [folder]` | Runs a write script with writes in memory, and exports the metadata before and after as sparse images for `apfsck`. |
| `dev-real-write` / `dev-undo` | A guarded single-transaction write with an undo point (block 0 and both checkpoint areas), and its undo. |

`apfsck` checks images thoroughly but stops on several things macOS writes in practice (an object-map node holding a newer key, clone flags, decmpfs attributes on uncompressed files), so on real Mac drives `dev-space-check` and macOS First Aid are the oracles.

## Showing drives in Windows

winapfs implements a file system with [WinFsp](https://winfsp.dev). It uses its own small binding that loads `winfsp-x64.dll` at run time. The struct layouts and size checks come from WinFsp's headers.

- **Names.** Characters that Mac names may contain but Windows forbids (`" * : < > ? \ |`, control characters, trailing spaces and dots) are shifted into the Unicode private use area at U+F000 + character. This is the convention of Cygwin and WinFsp's FUSE layer, and it maps back exactly.
- **Speed.** On read-only mounts file data may stay in the Windows cache indefinitely: nothing can change it behind the mount's back. Writable mounts bypass the Windows cache, because a long copy through it can deadlock inside WinFsp's cached-write path (write throttling waits for the lazy writer, which waits for a file lock the throttled thread holds). Every write goes straight to winapfs instead. Both kinds of mount read ahead with growing fetches from 256 KiB to 8 MiB; writable mounts drop that window on any change. Without read-ahead, Windows forwards 4-8 KiB requests, and on USB each one costs a round trip.
- **Never losing accepted changes silently.** Changes are committed in batches. If a commit fails, or a failure leaves a change half made, the batch is gone. The mount then refuses all further changes, so programs see errors (and an Explorer move stops before deleting more originals) instead of carrying on. A write that would not fit is refused before it changes anything, keeping 16 MiB free for the metadata commits need, so a full drive fails that one file and keeps the rest.
- **Shared file details.** Open handles of one file share one record of its size and times: Windows' cache manager may write through any of them.
- **Cleanup.** If the mount process dies, WinFsp removes the mount point by itself, and the drive stays at its last commit.

### Drive letters, the launcher and the auto-mount service

Reading a raw disk needs administrator rights. But a drive letter made by an elevated program is invisible to Explorer, which runs unelevated. So:

1. `winapfs setup` (run once, as administrator, by `winapfs install`):
   - copies `winapfs.exe` to `%ProgramFiles%\winapfs`, which only administrators can change
   - adds that folder to the system `PATH`
   - registers `winapfs serve` with the **WinFsp Launcher**, a service that runs as SYSTEM
   - installs the **`winapfs-automount`** service, which starts with Windows
2. `winapfs-automount` checks every 3 seconds for disks with an APFS partition. Any user may ask a disk for its partition table without reading it (`IOCTL_DISK_GET_DRIVE_LAYOUT_EX` needs no access rights). When a Mac disk appears, the service asks the launcher to mount it at the first free letter from M: onwards, writable if write mode is on. When the disk goes away, it unmounts. If someone unmounts a drive while it is still plugged in, the service leaves it alone until the disk is plugged in again.
3. The launcher runs `winapfs serve src=diskN at=X: [mode=write]` as SYSTEM. SYSTEM can read and write the disk, and SYSTEM's drive letters are visible to every session.

### Security model

- The program the launcher and the service run lives where only administrators can change it. A user-writable location would let anyone swap the file and run code as SYSTEM.
- Only the account that ran setup, Administrators and SYSTEM may start or stop drives. The launcher service's SDDL grants `RPWPLC` to those three.
- `serve` accepts only whole disks (`diskN` or `auto`) and drive letters. It accepts no files, which the caller might not be allowed to read, and no folders, which would be created with SYSTEM's rights.
- Write mode is a file in `%ProgramFiles%\winapfs`, so only administrators can turn it on. The accounts allowed to start drives can also mount one writable themselves.
- While a drive is mounted, its files are visible to every account on the PC; writable mounts let every account change them.

## Licensing

All code is written from Apple's published specification. Nothing is copied from other APFS implementations, several of which are GPL (apfs-fuse, linux-apfs-rw, apfsprogs) or LGPL (libfsapfs). That keeps the project under **MIT OR Apache-2.0**. `apfsck` from apfsprogs is only run, as a separate checker in a container, to test images.

- The dependencies are permissive: `miniz_oxide`, `lzfse_rust`, `unicode-normalization`.
- WinFsp is GPLv3 with a FLOSS exception. An open-source program may link its DLL if it shows the notice "WinFsp - Windows File System Proxy, Copyright (C) Bill Zissimopoulos" with a link to https://github.com/winfsp/winfsp in its interface and documentation, and is never linked or shipped with proprietary software. winapfs shows the notice in `winapfs help`, after mounting, and in the README.
- winapfs does not use the `winfsp` Rust crate, which is GPL-3.0 and would make the whole program GPL. It loads the DLL itself instead.
