<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/assets/banner-dark.svg">
    <img src="docs/assets/banner-light.svg" alt="winapfs: your Mac drive, on Windows. Read it. Write it. Just plug it in." width="100%">
  </picture>
</p>

<p align="center">
  <a href="https://github.com/ZYRT3CH/winapfs/releases/latest/download/winapfs.exe"><img src="https://img.shields.io/badge/download-winapfs.exe-a78bfa?style=for-the-badge" alt="Download winapfs.exe"></a>
  <img src="https://img.shields.io/badge/Windows-10%20%7C%2011-22d3ee?style=for-the-badge" alt="Windows 10 and 11">
  <img src="https://img.shields.io/badge/APFS-read%20%2B%20write-f472b6?style=for-the-badge" alt="APFS read and write">
  <img src="https://img.shields.io/badge/license-MIT%20%7C%20Apache--2.0-34d399?style=for-the-badge" alt="MIT or Apache-2.0">
  <a href="https://github.com/ZYRT3CH/winapfs/actions/workflows/ci.yml"><img src="https://github.com/ZYRT3CH/winapfs/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
</p>

<h3 align="center">Plug in a Mac drive. Use it like any other drive.<br>Open your files, copy new ones on, rename, tidy up. On Windows.</h3>

<p align="center">
  No Mac needed. No paid tools. No reformatting.<br>
  <a href="#-set-it-up-in-2-minutes"><b>Set it up</b></a> ·
  <a href="#-writing-to-your-mac-drive-beta"><b>Writing (beta)</b></a> ·
  <a href="#-questions"><b>Questions</b></a> ·
  <a href="docs/HOW-IT-WORKS.md"><b>How it works</b></a>
</p>

<br>

<table>
  <tr>
    <td width="33%" valign="top">
      <h3>📂 Opens Mac drives</h3>
      Drives formatted on a Mac (APFS) show up in File Explorer with a drive letter. Photos, videos, projects: open them, play them, copy them.
    </td>
    <td width="33%" valign="top">
      <h3>📝 Writes, too <sup>beta</sup></h3>
      Copy files onto the drive, make folders, rename, move and delete, straight from Explorer or any program. Take it back to your Mac and keep going.
    </td>
    <td width="33%" valign="top">
      <h3>🔌 Plug and play</h3>
      Set up once. From then on your Mac drive appears by itself whenever you plug it in, even after a restart.
    </td>
  </tr>
  <tr>
    <td width="33%" valign="top">
      <h3>🛡️ Careful by design</h3>
      APFS never overwrites data in place, and winapfs keeps it that way. It checks the whole drive before writing and refuses if anything looks off.
    </td>
    <td width="33%" valign="top">
      <h3>⚡ Quick</h3>
      Changes are saved in batches, so copying hundreds of small files feels like a normal drive. Speed is set by your USB port.
    </td>
    <td width="33%" valign="top">
      <h3>💜 Free and open</h3>
      Open source, MIT or Apache-2.0. Written from scratch in Rust, following Apple's published APFS reference.
    </td>
  </tr>
</table>

<p align="center">
  <img src="docs/assets/demo.svg" width="780" alt="Terminal: winapfs write on, plug in the Mac drive, it is checked and mounted writable at M:, a video is copied on and verified.">
</p>

---

## 🚀 Set it up in 2 minutes

You need **Windows 10 or 11, 64-bit**. Pick whichever way you like:

### ⚡ One command (easiest)

Open **Terminal** or **PowerShell** from the Start menu, paste this, and press **Enter**:

```powershell
irm https://raw.githubusercontent.com/ZYRT3CH/winapfs/main/install.ps1 | iex
```

It downloads the latest `winapfs.exe` from this page's official release, checks it against its published checksum, and sets everything up. Say **yes** if it offers to install WinFsp (a free helper that lets winapfs show up as a drive), and click **Yes** when Windows asks for permission. Curious what it does first? [Read the script](install.ps1): it is short.

### 🖱️ Download and double-click

**1.** [**Download winapfs.exe**](https://github.com/ZYRT3CH/winapfs/releases/latest/download/winapfs.exe) (the latest version, straight from GitHub).

**2. Double-click `winapfs.exe`.** A window opens and does the setup for you:

- If your PC does not have **WinFsp** yet, it asks to install it. Press **Enter** to say yes.
- Windows asks *"Do you want to allow this app to make changes to your device?"* Click **Yes**. (Twice, if WinFsp is being installed: once for WinFsp, once for winapfs.)
- When the window says **Done**, press **Enter** to close it.

> **Windows may warn you first.** You might see *"Windows protected your PC"*. That happens with new programs that are not yet widely known. Click **More info**, then **Run anyway**. (The one-command way does not get this warning.)

### Then: plug in your Mac drive

After a few seconds it appears in **File Explorer**, usually as **M:**. 🎉 That's it. From now on this happens by itself.

<sub>Every release also has a `winapfs.exe.sha256` file to check your download, and all releases are listed [here](https://github.com/ZYRT3CH/winapfs/releases).</sub>

---

## 📝 Writing to your Mac drive (beta)

Out of the box your Mac drive opens **read-only**, so nothing can change by accident. To save files onto it too, turn on write mode once. Open **Terminal** from the Start menu and run:

```
winapfs write on
```

Click **Yes** when Windows asks. Your Mac drive is shown again, now writable, and stays that way every time you plug it in. Turn it back off with `winapfs write off`.

Before a drive appears writable, winapfs reads its structure and checks the free-space map. On a big drive that takes about half a minute. If anything is off, or the drive uses something winapfs cannot write yet, the drive still appears, read-only, so your files stay reachable.

**Just curious?** `winapfs mount auto W: --rehearse` gives you a fully writable drive where every change stays in memory and disappears when you unmount. A perfect, risk-free test drive.

<details>
<summary><b>🛡️ How winapfs keeps your drive safe</b></summary>
<br>

- **Copy-on-write, all the way down.** APFS never overwrites live data: every change goes to free space, and only then does a new checkpoint point to it. winapfs follows that rule, so a crash, a frozen PC or a yanked cable leaves the drive exactly as it was at the last saved change.
- **Saved about once a second.** Changes are collected and committed after a quiet second (at most every 5 seconds), and right away when you unmount.
- **Checked before every writable mount.** winapfs rebuilds which blocks are in use from everything on the volume and compares that with the drive's free-space map. If they disagree, it does not write.
- **Checked again after.** The same check runs when you unmount.
- **Tested hard.** Every write path is checked with an independent APFS checker on test images, and compared byte for byte with the same operations on NTFS. It has also been rehearsed and used on a real 1.8 TB macOS drive with 120,000+ files.

</details>

> [!WARNING]
> **Writing is new.** It has been tested carefully, but not yet by thousands of people. Keep a backup of anything you cannot lose, and run `winapfs unmount M:` before unplugging (or wait a few seconds after your last change). Not supported for writing yet, these appear read-only: encrypted drives, volumes with snapshots, Fusion drives. macOS-compressed files and hard-linked files can be read but not changed.

---

## 🧭 Everyday use

| You want to... | Do this |
|---|---|
| Use your Mac drive | Plug it in. It appears in File Explorer after a few seconds. |
| Save files onto it | Turn on write mode once: `winapfs write on`. Then copy files as usual. |
| Unplug it | Let copies finish, run `winapfs unmount M:`, then unplug. |
| Show it again | Unplug and plug it back in, or run `winapfs mount auto M:` |
| Copy files to your PC | Copy them in File Explorer, like from any other drive |

## 🧹 Remove winapfs

Open **Terminal** and run `winapfs uninstall`, then click **Yes** when Windows asks. If you saved `winapfs.exe` somewhere, run it from that folder (`.\winapfs.exe uninstall`) or use the copy in `C:\Program Files\winapfs\`. WinFsp stays installed; you can remove it in **Settings > Apps**.

---

## ❓ Questions

**Can I save files onto the Mac drive?**
Yes, with write mode (beta). See [Writing to your Mac drive](#-writing-to-your-mac-drive-beta).

**Is it safe for my Mac drive?**
In the default read-only mode, nothing can change at all: Windows itself blocks every write. With write mode on, winapfs follows APFS's copy-on-write rules and checks the drive before writing (see above). As with any new software that writes to disks: keep backups.

**Will my Mac still read the drive after I write to it on Windows?**
It should: winapfs writes standard APFS, following Apple's specification, and every kind of change is checked with independent APFS tools. Write support is in beta, so if your Mac ever complains about the drive, please [open an issue](https://github.com/ZYRT3CH/winapfs/issues).

**Does it work with "Mac OS Extended (Journaled)" drives?**
Not yet. winapfs works with **APFS**, the format Macs have used since 2017. "Mac OS Extended" (HFS+), common on older Macs and Time Machine disks, is not supported yet. To check your drive's format, open **Disk Utility** on a Mac and select the drive.

**My drive is encrypted (it asks for a password on the Mac).**
Encrypted drives are not supported yet.

**Some file names look strange.**
Macs allow a few characters in names that Windows does not, such as `:` or `?`. winapfs shows these as similar-looking symbols so you can still open the files.

**How fast is it?**
As fast as your USB connection and drive allow. On a USB 2 port that is roughly 35 MB/s. A blue USB 3 port is many times faster.

**The drive did not appear.**
Run `winapfs disks` in a terminal. It lists connected disks that have a Mac partition. If your drive is listed but no drive letter appears, run `winapfs mount auto M:` to see the error message, and [open an issue](https://github.com/ZYRT3CH/winapfs/issues).

---

## 💻 For the command line

```
winapfs disks                          list connected Mac disks
winapfs info auto                      show the drive's volumes
winapfs ls auto /Photos                list a folder
winapfs get auto /Photos D:\Copy       copy a folder or file to your PC
winapfs mount auto M:                  show the drive as M:
winapfs mount auto M: --write          ...writable (beta)
winapfs mount auto W: --rehearse       ...writable, changes kept in memory only
winapfs write on | off                 plugged-in drives appear writable, or not
winapfs unmount M:                     hide it again
winapfs mounts                         list mounted Mac drives
winapfs verify auto                    check that the whole drive reads correctly
winapfs help                           everything else
```

`auto` means "the Mac drive that is plugged in". You can also name a disk, like `disk3`. Commands that read the disk directly (`info`, `ls`, `get`, `verify`) need an Administrator terminal.

## 🔧 How it works

winapfs reads and writes APFS itself, following Apple's published *Apple File System Reference*: B-trees, object maps, the space manager and checkpoints, all in Rust. A small Windows service watches for Mac drives and shows them through [WinFsp](https://winfsp.dev). See [docs/HOW-IT-WORKS.md](docs/HOW-IT-WORKS.md) for the design, the safety rules and the security model, and [CONTRIBUTING.md](CONTRIBUTING.md) to help out.

## 📜 License

MIT or Apache-2.0, at your choice. See [LICENSE-MIT](LICENSE-MIT) and [LICENSE-APACHE](LICENSE-APACHE).

winapfs uses **WinFsp - Windows File System Proxy, Copyright (C) Bill Zissimopoulos** ([github.com/winfsp/winfsp](https://github.com/winfsp/winfsp)), under its FLOSS exception.

winapfs is not made by, affiliated with or endorsed by Apple. Mac and APFS are trademarks of Apple Inc.

<p align="center"><sub>Made with 💜 for everyone who owns a Mac drive and a Windows PC.</sub></p>
