**Your Mac drive, on Windows. Read it. Write it.** Plug in a drive formatted on a Mac (APFS) and it shows up in File Explorer. It opens read-only; turn on write mode and you can save files onto it too (beta).

### How to install

**One command:** open Terminal or PowerShell and paste:

```powershell
irm https://raw.githubusercontent.com/ZYRT3CH/winapfs/main/install.ps1 | iex
```

It downloads `winapfs.exe` from this release, checks its checksum and sets everything up.

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
