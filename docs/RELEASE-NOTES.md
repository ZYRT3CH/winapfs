**Open Mac drives on Windows.** Plug in a drive formatted on a Mac (APFS) and it shows up in File Explorer. winapfs only reads; it never changes the Mac drive.

### How to install

1. Download **`winapfs.exe`** below.
2. Double-click it. It installs WinFsp if needed, then asks Windows for permission. Click **Yes**.
3. Plug in your Mac drive. After a few seconds it appears in File Explorer, usually as **M:**.

If Windows says *"Windows protected your PC"*, click **More info**, then **Run anyway**.

Full guide: [README](https://github.com/ZYRT3CH/winapfs#readme)

### What works

- APFS drives, read-only: browse, open, copy and play files
- Shows up automatically when plugged in, also after a restart
- Compressed Mac files are unpacked on the fly

### Not yet

- Writing to the Mac drive
- Encrypted drives
- "Mac OS Extended (Journaled)" (HFS+) drives

`winapfs.exe.sha256` holds the file's SHA-256 checksum, so you can check your download.
