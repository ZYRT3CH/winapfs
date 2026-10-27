**Your Mac drive, on Windows. Read it. Write it.** Plug in a drive formatted on a Mac (APFS) and it shows up in File Explorer. It opens read-only; turn on write mode and you can save files onto it too (beta).

### What's new in 0.1.1

**Big copies in write mode no longer freeze.** A long copy or move onto a writable Mac drive could stop dead at 0 bytes/s, with Explorer unable to cancel. The cause was a deadlock deep inside Windows' file cache and WinFsp, not in your files: write throttling waited for Windows' background writer, which waited for a file lock the throttled thread held. Writable drives now bypass the Windows cache, so every write goes straight to winapfs, and winapfs reads ahead itself to keep reading fast. With this fix, a 322 GB move onto a Samsung T7 over USB 3 ran at 350-500 MB/s on large files. **If a copy is frozen right now:** update, then restart Windows to clear it. Your Mac drive keeps everything saved before the freeze.

**Saved files are never lost silently.** Changes are saved in batches every few seconds. Before, if saving a batch failed, the whole batch quietly disappeared while Explorer carried on, and in a move it had already deleted the originals. Now the drive stops taking changes and says why, so programs see an error and a move stops before deleting more originals.

**A full drive fails only the file that doesn't fit.** A copy that runs out of space is refused before it changes anything (winapfs keeps 16 MiB free for its own bookkeeping), so the files copied just before it stay put and the drive stays writable.

**Smoother copies of many small files.** Explorer asks for the free space constantly while copying; that answer is now remembered instead of read from the drive each time, and each save does one disk flush fewer.

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
