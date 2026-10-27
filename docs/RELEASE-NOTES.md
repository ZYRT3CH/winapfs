**Your Mac drive, on Windows. Read it. Write it.** Plug in a drive formatted on a Mac (APFS) and it shows up in File Explorer. It opens read-only; turn on write mode and you can save files onto it too (beta).

### What's new in 0.1.3: find out why a drive is slow

**`winapfs usb`** checks every USB drive, Mac or not, and tells you why it might be slow: the speed its USB link actually got, whether the port can do better, USB-C, the storage driver (UAS) and Windows' write-cache setting. Anything that can go faster gets a one-line fix. It needs no admin rights and changes nothing, and the downloaded `winapfs.exe` works without any setup (`.\winapfs.exe usb` in its folder).

It catches the problem that slowed a Samsung T7 to about 40 MB/s for hours. Every USB 3 port is really two ports in one, and the drive had connected on the USB 2 half:

```
1 USB drive, 1 can go faster
disk3  Samsung PSSD T7  link: USB 2 (480 Mbps), USB-C  driver: UAS  cache: quick removal
  fix: linked at USB 2 in a port that supports USB 3: replug it firmly, then try the drive's own cable or a port on the back of the PC
```

Plugged back in, it linked at 10 Gbps and read 829 MB/s.

**Ask Claude to help.** The README has a copy-paste prompt ("Check why my USB drive is slow and help me speed it up") and a ready-made Claude Code skill. Claude runs the checks, explains each finding in plain words, and asks before changing any setting. See [Drive slow? Find out why](https://github.com/ZYRT3CH/winapfs#-drive-slow-find-out-why).

### Also new in 0.1.2: a lot faster in write mode

Measured on a Samsung T7 over USB 3 (10 Gbps), same drive, same port, same test, before and after:

| | 0.1.1 | 0.1.2 | |
|---|---|---|---|
| Reading big files | 305 MB/s | **829 MB/s** | 2.7x |
| Writing big files | 445 MB/s | **782 MB/s** | 1.8x |
| Copying small files | 132 files/s | **200+ files/s** | 1.5x |

The drive itself tops out around 1,020 MB/s on that link, so big reads now get about 80% of everything the cable can carry.

- **Saving no longer stops the drive.** winapfs saves changes in batches every few seconds. Before, everything waited while a batch was written, 2 to 4 seconds at a time on a USB drive. Now a batch is prepared in memory in milliseconds and written while new changes keep coming in.
- **Big files are written in the background.** File data goes to the drive on two background writers, so Explorer can keep handing over data while earlier data is still being written.
- **Reads keep several requests in flight.** USB SSDs read far faster with several requests outstanding, so winapfs now reads four pieces at once, and fetches the next part of a file while you are still reading the current one.

Safety comes first in all of it: a batch's blocks are kept in memory until it is on the drive, blocks it freed are not reused until then, and saves land strictly in order, so a crash or a pulled cable at any moment still leaves the last saved state intact. Every change was tested by killing winapfs mid-save (every surviving file intact, byte for byte), by reading files while they are rewritten over and over (about 50 GB of block reuse, no wrong byte), and with an independent APFS checker.

**Copying many small files is now limited by Windows, not winapfs.** On the test PC even WinFsp's own empty in-memory demo drive manages only about 220 small files a second, because Microsoft Defender scans and reads back every new file on drives like these. Excluding your Mac drive from real-time scanning (Windows Security, Virus & threat protection, Exclusions) speeds this up a lot, at the cost of those files not being scanned as they are written. That choice is yours.

### Also new since 0.1.0 (in 0.1.1)

- **Big copies in write mode no longer freeze** (a deadlock in Windows' file cache and WinFsp; writable drives now bypass that cache). If a copy is frozen right now: update, then restart Windows.
- **Saved files are never lost silently:** if a batch ever cannot be saved, the drive stops taking changes and says why, so a move stops before deleting more originals.
- **A full drive fails only the file that doesn't fit**, and the drive stays writable.

### How to install

**One command:** open Terminal or PowerShell and paste:

```powershell
irm https://raw.githubusercontent.com/ZYRT3CH/winapfs/main/install.ps1 | iex
```

It downloads `winapfs.exe` from this release, checks its checksum and sets everything up. Already have winapfs? The same command updates it. Updating briefly unmounts your Mac drives, so let any copy finish first; they come back by themselves.

**Or by hand:**

1. Download **`winapfs.exe`** below.
2. Double-click it. It installs WinFsp if needed, then asks Windows for permission. Click **Yes**.
3. Plug in your Mac drive. After a few seconds it appears in File Explorer, usually as **M:**.
4. Want to save files onto it too? Open Terminal and run `winapfs write on` (beta).

If Windows says *"Windows protected your PC"*, click **More info**, then **Run anyway**.

Full guide: [README](https://github.com/ZYRT3CH/winapfs#readme)

### What works

- APFS drives: browse, open, copy and play files
- **Writing (beta):** copy files onto the drive, make folders, rename, move, overwrite and delete, from Explorer or any program
- Every writable mount checks the drive first and falls back to read-only if anything is off
- Shows up automatically when plugged in, also after a restart
- Compressed Mac files are unpacked on the fly

### Not yet

- Writing to encrypted drives, volumes with snapshots, or Fusion drives (they appear read-only)
- Changing macOS-compressed or hard-linked files (they can be read)
- Encrypted drives
- "Mac OS Extended (Journaled)" (HFS+) drives

Writing is new: keep a backup of anything you cannot lose.

`winapfs.exe.sha256` holds the file's SHA-256 checksum, so you can check your download.
