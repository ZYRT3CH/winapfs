---
name: drive-speed
description: Find out why a USB drive is slow on Windows and how to speed it up. Checks the USB link the drive negotiated (USB 2 vs USB 3, a USB 3 drive stuck on the USB 2 half of a port), the port and cable, the UAS driver, the write-cache policy, Defender real-time scanning and the file system (NTFS, exFAT, APFS), then explains each finding with its fix. Use when the user says a drive is slow, a copy or transfer is slow, asks about USB speed, or wonders why an external SSD or hard drive is slower than advertised.
---

# Why is my drive slow?

Find out why a USB drive copies slowly on Windows, explain it in plain words,
and help fix it. You can find what slows a drive down; you cannot raise the
drive's own speed limit. Say so if the drive is already at its limit.

## Ground rules

- **Look, don't touch.** Every check below only reads. None needs an
  Administrator terminal.
- **Ask before changing anything.** Never change a system or security setting
  without the user's clear yes: not the write-cache policy, not Defender
  exclusions, nothing else. Before asking, explain the trade-off (below).
- Never turn Defender off, and never suggest reformatting a drive unless the
  user asks about formats; reformatting erases everything on it.
- If you cannot run commands on the user's PC, tell them what to paste into
  PowerShell, one step at a time, and read what they paste back.

## 1. The USB link: run `winapfs usb`

If winapfs is installed (`winapfs --version` works), run:

```
winapfs usb
```

It lists every USB drive with its link speed, port, driver and write-cache
policy, and a `fix:` or `check:` line for anything that can go faster. Read
those lines out to the user in plain words.

If winapfs is not installed, offer to run it without installing anything: the
official `winapfs.exe` from https://github.com/ZYRT3CH/winapfs/releases/latest
runs on its own (`.\winapfs.exe usb`), needs no admin rights and changes
nothing. Ask before downloading. If the user prefers not to, do the checks
yourself:

- **USB drives:** `Get-Disk | Where-Object BusType -eq USB | Format-Table Number, FriendlyName, Size`
- **Link speed:** Windows has no command for it. Either write a small read-only
  C# helper (PowerShell `Add-Type`) that asks the USB hubs, or measure. The
  helper: enumerate hub interfaces with SetupAPI
  (`{f18a0e88-c30c-11d0-8815-00a0c906bed8}`), open each with no access rights,
  and per port send `IOCTL_USB_GET_NODE_CONNECTION_INFORMATION_EX` (0x220448:
  ConnectionStatus at byte 31, Speed at 23),
  `IOCTL_USB_GET_NODE_CONNECTION_INFORMATION_EX_V2` (0x22045C, input Length 16
  and protocols 7: protocols at byte 8, bit 2 = USB 3; Flags at 12: bit 0 at
  5 Gbps, bit 1 USB 3 capable, bit 2 at 10 Gbps+, bit 3 10 Gbps capable) and
  `IOCTL_USB_GET_PORT_CONNECTOR_PROPERTIES` (0x220458: Type-C = bit 3 at
  byte 8, companion port number at 14). To measure instead: copy one big file
  (a few GB) off the drive and watch Explorer's copy speed. Stuck near
  35-40 MB/s means a USB 2 link.
- **Driver:** `Get-PnpDevice -PresentOnly | Where-Object { $_.Service -in 'UASPStor','USBSTOR' } | Format-Table FriendlyName, Service`
- **Write-cache policy:** `DEVPKEY_Device_RemovalPolicy` of the disk
  (`Get-PnpDeviceProperty -InstanceId <disk instance id> -KeyName DEVPKEY_Device_RemovalPolicy`):
  3 = Quick removal, 2 = Better performance.

## 2. Also check

- **Defender real-time scanning:** `(Get-MpComputerStatus).RealTimeProtectionEnabled`.
- **File system:** `[System.IO.DriveInfo]::GetDrives() | Where-Object IsReady | Format-Table Name, DriveFormat, DriveType`.
  `APFS` is a Mac drive shown through winapfs.

## 3. What each finding means, and the fix

**Linked at USB 2 in a port that supports USB 3.** Each USB 3 port is really
two ports in one: a USB 2 half and a USB 3 half. A plug that is not quite
home, a worn cable or a USB 2 cable connects only the USB 2 half: about
40 MB/s instead of hundreds. Windows does not warn about it. Fix: unplug and
plug it back in firmly; use the cable that came with the drive (many USB-C
phone cables are USB 2 only); use a port on the back of the PC rather than
the front panel, a hub or an extension cable. Run `winapfs usb` again to
confirm the new link.

**The port is USB 2 only.** Use a USB 3 port: often blue inside, or marked
SS, SS10 or with a 5/10/20 Gbps label.

**Linked at 5 Gbps, but the drive can do 10 Gbps.** A 10 Gbps port (often
USB-C or marked SS10) with the drive's own cable about doubles its top speed,
if the PC has such a port.

**Older storage driver (BOT, not UAS).** UAS handles many small files much
better. Which one is used is decided by the drive or its enclosure; nothing to
change on the PC. A UAS enclosure or adapter fixes it.

**Write-cache policy.** Quick removal (Windows' default) means the drive can
be unplugged any time without ejecting, but writes, especially many small
files to NTFS or exFAT, are slower. Better performance lets Windows cache
writes: faster, but the drive must then always be ejected ("Safely Remove
Hardware") before unplugging, or files can be lost or the drive damaged. It
mostly matters for NTFS and exFAT drives. Changing it is the user's choice
(Device Manager > Disk drives > the drive > Properties > Policies, needs
admin). Never change it without asking.

**Defender real-time scanning.** Defender checks every file as it is written
and opened. With big files that costs little; with thousands of small files it
can slow a copy several times over. An exclusion for the drive or the target
folder speeds this up, but files there are then not scanned when written or
opened: only for a drive and files the user trusts, and ideally removed again
after the copy. Adding one needs admin
(`Add-MpPreference -ExclusionPath 'X:\'`, undo with
`Remove-MpPreference -ExclusionPath 'X:\'`). Never add one without asking,
and never suggest turning real-time protection off.

**File system.** NTFS is the fastest on Windows for many small files. exFAT
is fine for big files and slower for many small ones. APFS (a Mac drive
through winapfs) reads and writes big files at close to the drive's speed;
many tiny files copy slower than on NTFS. Reformatting erases the drive, so
only bring it up if the user asks.

**Already at full speed.** If the link is right and copies are still slow,
the limit may be the drive itself: hard drives manage about 100-200 MB/s and
slow down with many small files; cheap flash drives write slowly; some drives
slow down during very long writes once their fast cache is full. Say so
plainly: no setting changes that.

## 4. Wrap up

List what you found, most important first, each with its fix in one or two
sentences. Ask before any change, and after one, check again.
