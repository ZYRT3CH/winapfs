# winapfs

**Open Mac drives on Windows.** Plug in a drive that was formatted on a Mac (APFS) and it shows up in File Explorer like any other drive. You can open, copy and play your files.

winapfs **only reads** the Mac drive. It never changes, deletes or writes anything on it.

Free and open source.

---

## Set it up (about 2 minutes)

You need **Windows 10 or 11, 64-bit**.

### 1. Download

Go to the [latest release](https://github.com/ZYRT3CH/winapfs/releases/latest) and download **`winapfs.exe`**.

### 2. Double-click `winapfs.exe`

A window opens and does the setup for you:

- If your PC does not have **WinFsp** yet, it asks to install it. WinFsp is a free helper program that lets winapfs show up as a drive. Press **Enter** to say yes.
- Windows asks: *"Do you want to allow this app to make changes to your device?"* Click **Yes**. It asks only this once.
- When the window says **Done**, press **Enter** to close it.

> **Windows may warn you first.** You might see *"Windows protected your PC"*. That happens with new programs that are not yet widely known. Click **More info**, then **Run anyway**.

### 3. Plug in your Mac drive

After a few seconds it appears in **File Explorer**, usually as drive **M:**.

That's it. From now on this happens by itself, even after you restart your PC.

---

## Everyday use

| You want to... | Do this |
|---|---|
| Use your Mac drive | Plug it in. It appears in File Explorer after a few seconds. |
| Unplug it | Close any files you have open from it, then unplug. |
| Hide the drive without unplugging | Run `winapfs unmount M:` in a terminal |
| Show it again | Unplug the drive and plug it back in, or run `winapfs mount auto M:` |
| Copy everything to your PC | Copy the folders in File Explorer, or run `winapfs get auto / D:\MacCopy` |

## Remove winapfs

Open a terminal (search for **Terminal** in the Start menu) and run:

```
winapfs uninstall
```

Click **Yes** when Windows asks. If you saved `winapfs.exe` somewhere, run it from that folder (`.\winapfs.exe uninstall`) or use the copy installed in `C:\Program Files\winapfs\`. WinFsp stays installed. You can remove it in **Settings > Apps**.

---

## Questions

**Is it safe for my Mac drive?**
Yes. winapfs opens the drive in read-only mode, so Windows itself blocks any write. Nothing on the Mac drive can change.

**Does it work with "Mac OS Extended (Journaled)" drives?**
Not yet. winapfs reads **APFS**, the format Macs have used since 2017. Drives formatted as "Mac OS Extended" (also called HFS+), which is common on older Macs and Time Machine disks, are not supported yet. To check your drive's format, open **Disk Utility** on a Mac and select the drive.

**Can I save files onto the Mac drive?**
Not yet. Writing is planned, but it will only be added once it has been tested very carefully, because a mistake there could damage a drive.

**My drive is encrypted (it asks for a password on the Mac).**
Encrypted drives are not supported yet.

**Some file names look strange.**
Macs allow a few characters in names that Windows does not, such as `:` or `?`. winapfs shows these characters as similar-looking symbols so you can still open the files.

**How fast is it?**
About as fast as your USB connection allows.

**The drive did not appear.**
Run `winapfs disks` in a terminal. It lists the connected disks that have a Mac partition. If your drive is listed but no drive letter appears, run `winapfs mount auto M:` to see the error message, and [open an issue](https://github.com/ZYRT3CH/winapfs/issues).

---

## For the command line

```
winapfs disks                       list connected Mac disks
winapfs info auto                   show the drive's volumes
winapfs ls auto /Photos             list a folder
winapfs get auto /Photos D:\Copy    copy a folder or file to your PC
winapfs mount auto M:               show the drive as M:
winapfs unmount M:                  hide it again
winapfs mounts                      list mounted Mac drives
winapfs verify auto                 check that the whole drive reads correctly
winapfs help                        everything else
```

`auto` means "the Mac drive that is plugged in". You can also name a disk, like `disk3`. Commands that read the disk directly (`info`, `ls`, `get`, `verify`) need an Administrator terminal.

## How it works

winapfs reads APFS itself, following Apple's published *Apple File System Reference*. A small Windows service watches for Mac drives and shows them through [WinFsp](https://winfsp.dev). See [docs/HOW-IT-WORKS.md](docs/HOW-IT-WORKS.md) for the design, the safety rules and the security model, and [CONTRIBUTING.md](CONTRIBUTING.md) to help.

## License

MIT or Apache-2.0, at your choice. See [LICENSE-MIT](LICENSE-MIT) and [LICENSE-APACHE](LICENSE-APACHE).

winapfs uses **WinFsp - Windows File System Proxy, Copyright (C) Bill Zissimopoulos** ([github.com/winfsp/winfsp](https://github.com/winfsp/winfsp)), under its FLOSS exception.

winapfs is not made by, affiliated with or endorsed by Apple. Mac and APFS are trademarks of Apple Inc.
