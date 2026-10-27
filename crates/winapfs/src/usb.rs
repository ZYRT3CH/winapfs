//! `winapfs usb`: why a USB drive may be slow. Lists the connected USB
//! storage drives with the speed their link negotiated, the kind of port,
//! the storage driver and Windows' write-cache policy, and a hint for
//! anything that can be fixed.
//!
//! Read-only, and needs no administrator rights: every hub and disk query
//! here is a FILE_ANY_ACCESS request, the same ones Microsoft's USBView makes.
//!
//! A USB 3 connector shows up as two hub ports: a USB 2 port and its
//! "companion" USB 3 port. A USB 3 drive that links on the USB 2 half (a
//! plug not quite home, a USB 2 cable) runs at 480 Mbps, and Windows says
//! nothing. That is the main thing this looks for.

use std::ffi::c_void;
use std::fmt::Write;

use crate::winfsp::wide;

/// GUID_DEVINTERFACE_USB_HUB {f18a0e88-c30c-11d0-8815-00a0c906bed8}
const USB_HUB: Guid = Guid(0xf18a_0e88, 0xc30c, 0x11d0, [0x88, 0x15, 0x00, 0xa0, 0xc9, 0x06, 0xbe, 0xd8]);
/// GUID_DEVINTERFACE_DISK {53f56307-b6bf-11d0-94f2-00a0c91efb8b}
const DISK: Guid = Guid(0x53f5_6307, 0xb6bf, 0x11d0, [0x94, 0xf2, 0x00, 0xa0, 0xc9, 0x1e, 0xfb, 0x8b]);

const IOCTL_USB_GET_NODE_INFORMATION: u32 = 0x0022_0408;
const IOCTL_USB_GET_NODE_CONNECTION_INFORMATION_EX: u32 = 0x0022_0448;
const IOCTL_USB_GET_PORT_CONNECTOR_PROPERTIES: u32 = 0x0022_0458;
const IOCTL_USB_GET_NODE_CONNECTION_INFORMATION_EX_V2: u32 = 0x0022_045C;
const IOCTL_STORAGE_CHECK_VERIFY2: u32 = 0x002D_0800;
const IOCTL_STORAGE_GET_HOTPLUG_INFO: u32 = 0x002D_0C14;
const IOCTL_STORAGE_GET_DEVICE_NUMBER: u32 = 0x002D_1080;

/// CM_DRP_* device properties (the SPDRP_* value plus one).
const CM_DRP_DEVICEDESC: u32 = 0x01;
const CM_DRP_SERVICE: u32 = 0x05;
const CM_DRP_FRIENDLYNAME: u32 = 0x0D;
const CM_DRP_ADDRESS: u32 = 0x1D;

/// Where the README explains the other causes of slow copies.
const MORE: &str = "https://github.com/ZYRT3CH/winapfs#-drive-slow-find-out-why";

/// The speed a device's link negotiated, slowest first.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
enum Speed {
    #[default]
    Unknown,
    Usb1,
    Usb2,
    Usb3,
    Usb3Plus,
}

impl Speed {
    fn label(self) -> &'static str {
        match self {
            Speed::Unknown => "unknown speed",
            Speed::Usb1 => "USB 1 (12 Mbps)",
            Speed::Usb2 => "USB 2 (480 Mbps)",
            Speed::Usb3 => "USB 3 (5 Gbps)",
            Speed::Usb3Plus => "USB 3 (10 Gbps+)",
        }
    }
}

/// One hub port, as its hub describes it.
#[derive(Clone, Debug, Default, PartialEq)]
struct Port {
    /// The port speaks USB 3 (SuperSpeed).
    usb3: bool,
    type_c: bool,
    /// The other half of the same connector (USB 2 or USB 3), 0 if none, on
    /// the hub `companion_hub` names (empty: this hub).
    companion: u16,
    companion_hub: String,
    connected: bool,
    speed: Speed,
    /// What the plugged-in device says it can do.
    can_usb3: bool,
    can_usb3_plus: bool,
}

/// A port from the answers to the connector, V2 and connection queries;
/// a query that failed leaves its fields at their defaults.
fn parse_port(connector: Option<&[u8]>, v2: Option<&[u8]>, conn: Option<&[u8]>) -> Port {
    let mut p = Port::default();
    if let Some(c) = connector {
        // USB_PORT_CONNECTOR_PROPERTIES: UsbPortProperties at 8 (bit 3: Type-C),
        // CompanionPortNumber at 14, CompanionHubSymbolicLinkName from 16.
        p.type_c = u32_at(c, 8) & 0x8 != 0;
        p.companion = u16_at(c, 14);
        p.companion_hub = wide_str(c.get(16..).unwrap_or_default());
    }
    let mut flags = 0;
    if let Some(v) = v2 {
        // USB_NODE_CONNECTION_INFORMATION_EX_V2: SupportedUsbProtocols at 8
        // (bit 2: USB 3), Flags at 12: operating at SuperSpeed (bit 0), capable
        // of it (bit 1), operating at SuperSpeedPlus (bit 2), capable of it (bit 3).
        p.usb3 = u32_at(v, 8) & 0x4 != 0;
        flags = u32_at(v, 12);
        p.can_usb3 = flags & 0x3 != 0;
        p.can_usb3_plus = flags & 0xC != 0;
    }
    if let Some(c) = conn {
        // USB_NODE_CONNECTION_INFORMATION_EX (packed): Speed at 23,
        // ConnectionStatus at 31 (1 = connected). Speed alone calls a
        // SuperSpeed link high speed, so the V2 flags come first.
        p.connected = u32_at(c, 31) == 1;
        if p.connected {
            p.speed = match (flags, c.get(23).copied()) {
                (f, _) if f & 0x4 != 0 => Speed::Usb3Plus,
                (f, _) if f & 0x1 != 0 => Speed::Usb3,
                (_, Some(0 | 1)) => Speed::Usb1,
                (_, Some(2)) => Speed::Usb2,
                (_, Some(3)) => Speed::Usb3,
                _ => Speed::Unknown,
            };
        }
    }
    p
}

/// A USB hub: its device instance ID, its interface path (as `hub_key`
/// writes it) and its ports, port 1 first.
struct Hub {
    id: String,
    key: String,
    ports: Vec<Port>,
}

/// Interface paths compare without their `\\?\` prefix and case.
fn hub_key(path: &str) -> String {
    let lower = path.to_ascii_lowercase();
    ["\\\\?\\", "\\??\\", "\\\\.\\"]
        .iter()
        .find_map(|p| lower.strip_prefix(p))
        .unwrap_or(&lower)
        .to_string()
}

/// A drive's USB link, and the port it is plugged into.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Link {
    speed: Speed,
    type_c: bool,
    via_hub: bool,
    /// The connector has a USB 3 half, and nothing is in it.
    usb3_free: bool,
    can_usb3: bool,
    can_usb3_plus: bool,
}

/// The link of whatever is plugged into `port` (numbered from 1) of `hubs[hub]`.
fn link(hubs: &[Hub], hub: usize, port: u32) -> Option<Link> {
    let h = &hubs[hub];
    let p = h.ports.get((port as usize).checked_sub(1)?)?;
    if !p.connected {
        return None;
    }
    let companion = match p.companion {
        0 => None,
        n => {
            let other = if p.companion_hub.is_empty() {
                Some(h)
            } else {
                let key = hub_key(&p.companion_hub);
                hubs.iter().find(|x| x.key == key)
            };
            other.and_then(|o| o.ports.get(n as usize - 1))
        }
    };
    Some(Link {
        speed: p.speed,
        type_c: p.type_c || companion.is_some_and(|c| c.type_c),
        via_hub: !h.id.starts_with("USB\\ROOT_HUB"),
        usb3_free: !p.usb3 && companion.is_some_and(|c| c.usb3 && !c.connected),
        can_usb3: p.can_usb3,
        can_usb3_plus: p.can_usb3_plus,
    })
}

/// A USB storage drive and what may limit its speed.
#[derive(Clone, Debug, Default, PartialEq)]
struct Drive {
    disk: u32,
    name: String,
    /// `None` when its hub would not say.
    link: Option<Link>,
    /// UAS (the UASPStor driver), or the older Bulk-Only one (USBSTOR).
    uas: Option<bool>,
    /// Windows' write-cache policy: "Quick removal" (true) or "Better performance".
    quick_removal: Option<bool>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Level {
    /// The drive can go faster, and this says how.
    Fix,
    /// It may go faster; this says what to try.
    Check,
    /// Worth knowing, nothing to change on the PC.
    Note,
}

impl Level {
    fn label(self) -> &'static str {
        match self {
            Level::Fix => "fix",
            Level::Check => "check",
            Level::Note => "note",
        }
    }
}

/// What slows the drive down, each with what to do about it.
fn findings(d: &Drive) -> Vec<(Level, &'static str)> {
    let mut out = Vec::new();
    let Some(l) = d.link else {
        out.push((Level::Note, "could not read this drive's USB port"));
        return out;
    };
    let speed = match l.speed {
        Speed::Usb1 => Some((Level::Fix, "linked at USB 1, the slowest speed: replug it, or try another port and cable")),
        Speed::Usb2 if l.usb3_free && l.can_usb3 => Some((
            Level::Fix,
            "linked at USB 2 in a port that supports USB 3: replug it firmly, then try the drive's own cable or a port on the back of the PC",
        )),
        Speed::Usb2 if l.usb3_free => Some((
            Level::Check,
            "linked at USB 2 in a port that supports USB 3; if this is a USB 3 drive, replug it firmly, then try its own cable or a port on the back of the PC",
        )),
        Speed::Usb2 if l.can_usb3 && l.via_hub => {
            Some((Level::Fix, "the drive supports USB 3 but linked at USB 2 through a hub: plug it straight into a USB 3 port on the PC"))
        }
        Speed::Usb2 if l.can_usb3 => {
            Some((Level::Fix, "the drive supports USB 3 but this port is USB 2 only: use a USB 3 port (often blue, or marked SS)"))
        }
        Speed::Usb2 => {
            Some((Level::Note, "linked at USB 2 (about 40 MB/s at most); the drive does not report USB 3, so this is likely its limit"))
        }
        Speed::Usb3 if l.can_usb3_plus => Some((
            Level::Fix,
            "the drive supports 10 Gbps but linked at 5 Gbps: use its own cable and a 10 Gbps port (often USB-C, or marked SS10) if the PC has one",
        )),
        _ => None,
    };
    out.extend(speed);
    if d.uas == Some(false) && l.speed >= Speed::Usb3 {
        out.push((Level::Note, "uses the older USB storage driver (BOT, not UAS): many small files copy slower; the drive's enclosure decides this"));
    }
    out
}

/// The text `winapfs usb` prints.
fn report(drives: &[Drive]) -> String {
    let mut s = String::new();
    if drives.is_empty() {
        s.push_str("no USB drives connected\nnext: plug in the drive, then run `winapfs usb` again\n");
        return s;
    }
    let all: Vec<_> = drives.iter().map(findings).collect();
    let count = |level| all.iter().filter(|f| f.iter().any(|&(l, _)| l == level)).count();
    let (fix, check) = (count(Level::Fix), count(Level::Check));
    let n = drives.len();
    let _ = write!(s, "{n} USB drive{}, ", if n == 1 { "" } else { "s" });
    let _ = match (fix, check) {
        (0, 0) => writeln!(s, "no USB slowdown found"),
        (0, k) => writeln!(s, "{k} worth a check"),
        (k, _) => writeln!(s, "{k} can go faster"),
    };
    for (d, found) in drives.iter().zip(&all) {
        let link = d.link.map_or("?".to_string(), |l| {
            format!("{}{}{}", l.speed.label(), if l.type_c { ", USB-C" } else { "" }, if l.via_hub { ", via a hub" } else { "" })
        });
        let driver = d.uas.map_or("?", |u| if u { "UAS" } else { "BOT (older)" });
        let cache = d.quick_removal.map_or("?", |q| if q { "quick removal" } else { "better performance" });
        let _ = writeln!(s, "disk{}  {}  link: {link}  driver: {driver}  cache: {cache}", d.disk, d.name);
        if found.is_empty() {
            s.push_str("  ok: linked at the drive's full speed\n");
        }
        for (level, text) in found {
            let _ = writeln!(s, "  {}: {text}", level.label());
        }
    }
    let _ = match fix + check {
        0 => writeln!(s, "next: still slow? Defender scanning and the drive's format matter too: {MORE}"),
        _ => writeln!(s, "next: after a change, run `winapfs usb` again to see the new link"),
    };
    s
}

/// "Samsung PSSD T7 SCSI Disk Device" -> "Samsung PSSD T7".
fn clean_name(name: &str) -> String {
    let mut n = name.trim();
    for suffix in [" SCSI Disk Device", " USB Device", " Disk Device"] {
        if let Some(cut) = n.len().checked_sub(suffix.len()) {
            if n.get(cut..).is_some_and(|tail| tail.eq_ignore_ascii_case(suffix)) {
                n = n[..cut].trim_end();
                break;
            }
        }
    }
    if n.is_empty() { "USB drive".into() } else { n.into() }
}

fn u16_at(b: &[u8], o: usize) -> u16 {
    b.get(o..o + 2).map_or(0, |x| u16::from_le_bytes([x[0], x[1]]))
}

fn u32_at(b: &[u8], o: usize) -> u32 {
    b.get(o..o + 4).map_or(0, |x| u32::from_le_bytes(x.try_into().unwrap()))
}

/// A NUL-terminated UTF-16 string stored as little-endian bytes.
fn wide_str(b: &[u8]) -> String {
    let units: Vec<u16> = b.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|&u| u != 0).collect();
    String::from_utf16_lossy(&units)
}

/// Prints the report for the drives connected now.
pub fn run() {
    print!("{}", report(&drives()));
}

/// Every connected USB storage drive, by disk number.
fn drives() -> Vec<Drive> {
    let hubs = hubs();
    let mut out = Vec::new();
    for (path, node) in interfaces(&DISK) {
        let Some(h) = open(&path) else { continue };
        let Some(number) = ioctl(&h, IOCTL_STORAGE_GET_DEVICE_NUMBER, &[], 12) else { continue };
        // STORAGE_HOTPLUG_INFO: MediaRemovable at 4, DeviceHotplug at 6. Device
        // Manager's "Quick removal" is DeviceHotplug; "Better performance" clears it.
        let hotplug = ioctl(&h, IOCTL_STORAGE_GET_HOTPLUG_INFO, &[], 8).filter(|b| b.len() >= 8);
        let removable_media = hotplug.as_ref().is_some_and(|b| b[4] != 0);
        if removable_media && ioctl(&h, IOCTL_STORAGE_CHECK_VERIFY2, &[], 0).is_none() {
            continue; // an empty card-reader slot
        }
        // Walk up the device tree to the hub port the drive is plugged into,
        // noting the storage driver on the way.
        let (mut uas, mut at) = (None, None);
        let mut cur = node;
        for _ in 0..16 {
            match registry_string(cur, CM_DRP_SERVICE).to_ascii_lowercase().as_str() {
                "uaspstor" => uas = Some(true),
                "usbstor" => uas = Some(false),
                _ => {}
            }
            let Some(parent) = parent(cur) else { break };
            let parent_id = device_id(parent);
            if let Some(i) = hubs.iter().position(|h| h.id == parent_id) {
                // A USB device's address is its port number on the hub.
                at = registry(cur, CM_DRP_ADDRESS).map(|a| (i, u32_at(&a, 0)));
                break;
            }
            cur = parent;
        }
        if at.is_none() && uas.is_none() {
            continue; // not a USB drive
        }
        let name = Some(registry_string(node, CM_DRP_FRIENDLYNAME))
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| registry_string(node, CM_DRP_DEVICEDESC));
        out.push(Drive {
            disk: u32_at(&number, 4),
            name: clean_name(&name),
            link: at.and_then(|(i, port)| link(&hubs, i, port)),
            uas,
            quick_removal: hotplug.map(|b| b[6] != 0),
        });
    }
    out.sort_by_key(|d| d.disk);
    out
}

/// Every USB hub with what each of its ports reports.
fn hubs() -> Vec<Hub> {
    interfaces(&USB_HUB)
        .into_iter()
        .filter_map(|(path, node)| {
            let h = open(&path)?;
            // USB_NODE_INFORMATION: the hub descriptor's bNumberOfPorts at 6.
            let count = ioctl(&h, IOCTL_USB_GET_NODE_INFORMATION, &[0; 256], 256).map_or(0, |b| b.get(6).copied().unwrap_or(0));
            let ports = (1..=u32::from(count)).map(|n| query_port(&h, n)).collect();
            Some(Hub { id: device_id(node), key: hub_key(&path), ports })
        })
        .collect()
}

fn query_port(hub: &Handle, n: u32) -> Port {
    let mut input = [0u8; 512];
    input[..4].copy_from_slice(&n.to_le_bytes());
    let mut v2 = [0u8; 16];
    v2[..4].copy_from_slice(&n.to_le_bytes());
    v2[4..8].copy_from_slice(&16u32.to_le_bytes()); // Length
    v2[8..12].copy_from_slice(&7u32.to_le_bytes()); // the protocols we understand: USB 1.1, 2 and 3
    parse_port(
        ioctl(hub, IOCTL_USB_GET_PORT_CONNECTOR_PROPERTIES, &input, 512).as_deref(),
        ioctl(hub, IOCTL_USB_GET_NODE_CONNECTION_INFORMATION_EX_V2, &v2, 16).as_deref(),
        ioctl(hub, IOCTL_USB_GET_NODE_CONNECTION_INFORMATION_EX, &input, 512).as_deref(),
    )
}

#[repr(C)]
#[derive(Default)]
struct Guid(u32, u16, u16, [u8; 8]);

#[repr(C)]
#[derive(Default)]
struct SpDeviceInterfaceData {
    size: u32,
    class: Guid,
    flags: u32,
    reserved: usize,
}

#[repr(C)]
#[derive(Default)]
struct SpDevinfoData {
    size: u32,
    class: Guid,
    dev_inst: u32,
    reserved: usize,
}

#[link(name = "setupapi")]
extern "system" {
    fn SetupDiGetClassDevsW(class: *const Guid, enumerator: *const u16, parent: *mut c_void, flags: u32) -> *mut c_void;
    fn SetupDiEnumDeviceInterfaces(
        set: *mut c_void,
        info: *mut SpDevinfoData,
        class: *const Guid,
        index: u32,
        data: *mut SpDeviceInterfaceData,
    ) -> i32;
    fn SetupDiGetDeviceInterfaceDetailW(
        set: *mut c_void,
        data: *mut SpDeviceInterfaceData,
        detail: *mut c_void,
        size: u32,
        needed: *mut u32,
        info: *mut SpDevinfoData,
    ) -> i32;
    fn SetupDiDestroyDeviceInfoList(set: *mut c_void) -> i32;
}

// The CM_* functions live in cfgmgr32; the GNU toolchain's bundled import
// libraries have no libcfgmgr32.a, but setupapi exports them too.
#[cfg_attr(target_env = "msvc", link(name = "cfgmgr32"))]
#[cfg_attr(not(target_env = "msvc"), link(name = "setupapi"))]
extern "system" {
    fn CM_Get_Parent(parent: *mut u32, node: u32, flags: u32) -> u32;
    fn CM_Get_Device_IDW(node: u32, buf: *mut u16, len: u32, flags: u32) -> u32;
    fn CM_Get_DevNode_Registry_PropertyW(node: u32, prop: u32, kind: *mut u32, buf: *mut c_void, len: *mut u32, flags: u32) -> u32;
}

#[link(name = "kernel32")]
extern "system" {
    fn CreateFileW(
        name: *const u16,
        access: u32,
        share: u32,
        security: *mut c_void,
        disposition: u32,
        flags: u32,
        template: *mut c_void,
    ) -> *mut c_void;
    fn DeviceIoControl(
        device: *mut c_void,
        code: u32,
        input: *mut c_void,
        input_len: u32,
        output: *mut c_void,
        output_len: u32,
        returned: *mut u32,
        overlapped: *mut c_void,
    ) -> i32;
    fn CloseHandle(handle: *mut c_void) -> i32;
}

/// The interface path and device node of every present device of `class`.
fn interfaces(class: &Guid) -> Vec<(String, u32)> {
    const DIGCF_PRESENT: u32 = 0x2;
    const DIGCF_DEVICEINTERFACE: u32 = 0x10;
    let mut out = Vec::new();
    unsafe {
        let set = SetupDiGetClassDevsW(class, std::ptr::null(), std::ptr::null_mut(), DIGCF_PRESENT | DIGCF_DEVICEINTERFACE);
        if set as isize == -1 {
            return out;
        }
        for i in 0.. {
            let mut data = SpDeviceInterfaceData { size: std::mem::size_of::<SpDeviceInterfaceData>() as u32, ..Default::default() };
            if SetupDiEnumDeviceInterfaces(set, std::ptr::null_mut(), class, i, &mut data) == 0 {
                break;
            }
            let mut need = 0u32;
            SetupDiGetDeviceInterfaceDetailW(set, &mut data, std::ptr::null_mut(), 0, &mut need, std::ptr::null_mut());
            if need <= 4 {
                continue;
            }
            // SP_DEVICE_INTERFACE_DETAIL_DATA_W: cbSize (the struct's own size,
            // padding included), then the path.
            let mut buf = vec![0u32; (need as usize).div_ceil(4)];
            buf[0] = if cfg!(target_pointer_width = "64") { 8 } else { 6 };
            let mut info = SpDevinfoData { size: std::mem::size_of::<SpDevinfoData>() as u32, ..Default::default() };
            if SetupDiGetDeviceInterfaceDetailW(set, &mut data, buf.as_mut_ptr().cast(), need, &mut need, &mut info) == 0 {
                continue;
            }
            let bytes = std::slice::from_raw_parts(buf.as_ptr().cast::<u8>(), need as usize);
            out.push((wide_str(&bytes[4..]), info.dev_inst));
        }
        SetupDiDestroyDeviceInfoList(set);
    }
    out
}

struct Handle(*mut c_void);

impl Drop for Handle {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

/// Opens a device for queries only: no read or write rights.
fn open(path: &str) -> Option<Handle> {
    const FILE_SHARE_READ_WRITE: u32 = 0x3;
    const OPEN_EXISTING: u32 = 3;
    let name = wide(path);
    let h = unsafe {
        CreateFileW(name.as_ptr(), 0, FILE_SHARE_READ_WRITE, std::ptr::null_mut(), OPEN_EXISTING, 0, std::ptr::null_mut())
    };
    (h as isize != -1).then_some(Handle(h))
}

/// The bytes a device returns for `code`, or `None` if it refused.
fn ioctl(h: &Handle, code: u32, input: &[u8], out_len: usize) -> Option<Vec<u8>> {
    let mut out = vec![0u8; out_len];
    let mut got = 0u32;
    let ok = unsafe {
        DeviceIoControl(
            h.0,
            code,
            input.as_ptr() as *mut c_void,
            input.len() as u32,
            out.as_mut_ptr().cast(),
            out_len as u32,
            &mut got,
            std::ptr::null_mut(),
        )
    };
    out.truncate(got as usize);
    (ok != 0).then_some(out)
}

fn parent(node: u32) -> Option<u32> {
    let mut p = 0u32;
    (unsafe { CM_Get_Parent(&mut p, node, 0) } == 0).then_some(p)
}

/// A device node's instance ID, such as `USB\ROOT_HUB30\4&1A2B3C&0&0`, upper case.
fn device_id(node: u32) -> String {
    let mut buf = [0u16; 512];
    if unsafe { CM_Get_Device_IDW(node, buf.as_mut_ptr(), buf.len() as u32, 0) } != 0 {
        return String::new();
    }
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end]).to_ascii_uppercase()
}

fn registry(node: u32, prop: u32) -> Option<Vec<u8>> {
    let mut buf = vec![0u8; 1024];
    let mut len = buf.len() as u32;
    let ok = unsafe { CM_Get_DevNode_Registry_PropertyW(node, prop, std::ptr::null_mut(), buf.as_mut_ptr().cast(), &mut len, 0) };
    buf.truncate(len as usize);
    (ok == 0).then_some(buf)
}

fn registry_string(node: u32, prop: u32) -> String {
    registry(node, prop).map(|b| wide_str(&b)).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn le32(b: &mut [u8], o: usize, v: u32) {
        b[o..o + 4].copy_from_slice(&v.to_le_bytes());
    }

    /// The answers a hub gives for one port, as raw query results.
    fn port_bytes(type_c: bool, companion: u16, protocols: u32, flags: u32, speed: u8, connected: bool) -> Port {
        let mut con = vec![0u8; 16];
        le32(&mut con, 8, if type_c { 0x9 } else { 0x1 });
        con[14..16].copy_from_slice(&companion.to_le_bytes());
        con.extend("USB#ROOT_HUB30#4&1&0&0".encode_utf16().chain([0]).flat_map(u16::to_le_bytes));
        let mut v2 = vec![0u8; 16];
        le32(&mut v2, 8, protocols);
        le32(&mut v2, 12, flags);
        let mut c = vec![0u8; 35];
        c[23] = speed;
        le32(&mut c, 31, u32::from(connected));
        parse_port(Some(&con), Some(&v2), Some(&c))
    }

    #[test]
    fn parses_port_queries() {
        // A USB 3 drive on the USB 2 half of a USB-C port: high speed, SuperSpeed capable.
        let p = port_bytes(true, 17, 0x3, 0x2, 2, true);
        assert_eq!(
            p,
            Port {
                usb3: false,
                type_c: true,
                companion: 17,
                companion_hub: "USB#ROOT_HUB30#4&1&0&0".into(),
                connected: true,
                speed: Speed::Usb2,
                can_usb3: true,
                can_usb3_plus: false,
            }
        );
        // The V2 flags decide SuperSpeed; the Speed byte cannot tell.
        assert_eq!(port_bytes(true, 1, 0x4, 0x1 | 0x2 | 0x4 | 0x8, 2, true).speed, Speed::Usb3Plus);
        assert_eq!(port_bytes(false, 1, 0x4, 0x1 | 0x2, 2, true).speed, Speed::Usb3);
        assert_eq!(port_bytes(false, 0, 0x3, 0, 1, true).speed, Speed::Usb1);
        assert_eq!(port_bytes(false, 0, 0x3, 0, 3, true).speed, Speed::Usb3);
        let empty = port_bytes(false, 0, 0x4, 0, 0, false);
        assert!(!empty.connected && empty.usb3 && empty.speed == Speed::Unknown);
        // Failed queries leave a blank port.
        assert_eq!(parse_port(None, None, None), Port::default());
        assert_eq!(parse_port(None, None, Some(&[0u8; 10])), Port::default());
    }

    fn root(ports: Vec<Port>) -> Hub {
        Hub { id: r"USB\ROOT_HUB30\4&1&0&0".into(), key: hub_key(r"\\?\USB#ROOT_HUB30#4&1&0&0#{f18a0e88}"), ports }
    }

    #[test]
    fn finds_the_empty_usb3_half() {
        let mut ports = vec![Port::default(); 20];
        ports[0] = Port { companion: 17, connected: true, speed: Speed::Usb2, can_usb3: true, ..Port::default() };
        ports[16] = Port { usb3: true, type_c: true, companion: 1, ..Port::default() };
        let hubs = [root(ports)];
        let l = link(&hubs, 0, 1).unwrap();
        assert!(l.usb3_free && l.type_c && !l.via_hub && l.speed == Speed::Usb2);
        assert_eq!(link(&hubs, 0, 17), None, "nothing plugged in");
        assert_eq!(link(&hubs, 0, 0), None);
        assert_eq!(link(&hubs, 0, 99), None);

        // The companion named by its hub's link, and taken: not free.
        let mut ports = hubs[0].ports.clone();
        ports[0].companion_hub = "USB#ROOT_HUB30#4&1&0&0#{F18A0E88}".into();
        ports[16].connected = true;
        assert!(!link(&[root(ports)], 0, 1).unwrap().usb3_free);

        let hub = Hub { id: r"USB\VID_05E3&PID_0610\5&2".into(), key: "x".into(), ports: vec![hubs[0].ports[0].clone()] };
        let l = link(&[hub], 0, 1).unwrap();
        assert!(l.via_hub && !l.usb3_free, "companion port not found");
    }

    fn drive(link: Link) -> Drive {
        Drive { disk: 3, name: "Samsung PSSD T7".into(), link: Some(link), uas: Some(true), quick_removal: Some(true) }
    }

    #[test]
    fn hints_match_the_link() {
        let usb2 = Link { speed: Speed::Usb2, usb3_free: true, can_usb3: true, type_c: true, ..Link::default() };
        let f = findings(&drive(usb2));
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].0, Level::Fix);
        assert!(f[0].1.starts_with("linked at USB 2 in a port that supports USB 3: "));

        // A drive that does not claim USB 3 is only worth a check.
        assert_eq!(findings(&drive(Link { can_usb3: false, ..usb2 }))[0].0, Level::Check);
        let usb2_port = Link { usb3_free: false, ..usb2 };
        assert!(findings(&drive(usb2_port))[0].1.contains("USB 2 only"));
        assert!(findings(&drive(Link { via_hub: true, ..usb2_port }))[0].1.contains("hub"));
        assert_eq!(findings(&drive(Link { can_usb3: false, ..usb2_port }))[0].0, Level::Note);
        assert_eq!(findings(&drive(Link { speed: Speed::Usb1, ..usb2 }))[0].0, Level::Fix);

        let fast = Link { speed: Speed::Usb3Plus, can_usb3: true, can_usb3_plus: true, type_c: true, ..Link::default() };
        assert!(findings(&drive(fast)).is_empty());
        assert!(findings(&drive(Link { speed: Speed::Usb3, can_usb3_plus: false, ..fast })).is_empty());
        assert!(findings(&drive(Link { speed: Speed::Usb3, ..fast }))[0].1.contains("10 Gbps"));

        let bot = findings(&Drive { uas: Some(false), ..drive(fast) });
        assert!(bot.len() == 1 && bot[0].0 == Level::Note && bot[0].1.contains("UAS"));
        // BOT is normal on a USB 2 link, so it is not mentioned there.
        assert_eq!(findings(&Drive { uas: Some(false), ..drive(usb2) }).len(), 1);

        assert_eq!(findings(&Drive { link: None, ..drive(fast) })[0].0, Level::Note);
    }

    #[test]
    fn reports_compactly() {
        assert_eq!(report(&[]), "no USB drives connected\nnext: plug in the drive, then run `winapfs usb` again\n");

        let fast = Link { speed: Speed::Usb3Plus, can_usb3: true, can_usb3_plus: true, type_c: true, ..Link::default() };
        let out = report(&[drive(fast)]);
        assert_eq!(
            out,
            format!(
                "1 USB drive, no USB slowdown found\n\
                 disk3  Samsung PSSD T7  link: USB 3 (10 Gbps+), USB-C  driver: UAS  cache: quick removal\n  \
                 ok: linked at the drive's full speed\n\
                 next: still slow? Defender scanning and the drive's format matter too: {MORE}\n"
            )
        );

        let slow = Link { speed: Speed::Usb2, usb3_free: true, can_usb3: true, ..Link::default() };
        let unknown = Drive { disk: 5, name: "USB drive".into(), link: None, uas: None, quick_removal: Some(false) };
        let out = report(&[drive(slow), unknown]);
        assert!(out.starts_with("2 USB drives, 1 can go faster\n"), "{out}");
        assert!(out.contains("disk3  Samsung PSSD T7  link: USB 2 (480 Mbps)  driver: UAS  cache: quick removal\n  fix: linked at USB 2"));
        assert!(out.contains("disk5  USB drive  link: ?  driver: ?  cache: better performance\n  note: could not read"));
        assert!(out.ends_with("next: after a change, run `winapfs usb` again to see the new link\n"));
    }

    #[test]
    fn readme_shows_the_real_output() {
        let stuck = Link { speed: Speed::Usb2, type_c: true, usb3_free: true, can_usb3: true, ..Link::default() };
        let shown: String = report(&[drive(stuck)]).lines().take(3).map(|l| format!("{l}\n")).collect();
        let readme = include_str!("../../../README.md").replace("\r\n", "\n");
        assert!(readme.contains(&shown), "README's `winapfs usb` example should read:\n{shown}");
    }

    #[test]
    fn cleans_names_and_paths() {
        assert_eq!(clean_name("Samsung PSSD T7 SCSI Disk Device"), "Samsung PSSD T7");
        assert_eq!(clean_name("WD My Passport 25E2 USB Device"), "WD My Passport 25E2");
        assert_eq!(clean_name("Generic Flash Disk usb device"), "Generic Flash Disk");
        assert_eq!(clean_name("  "), "USB drive");
        assert_eq!(clean_name("Øvre Disk"), "Øvre Disk");
        assert_eq!(hub_key(r"\\?\USB#ROOT_HUB30#4&1"), hub_key("usb#root_hub30#4&1"));
        assert_eq!(hub_key(r"\??\USB#X"), "usb#x");
        assert_eq!(wide_str(&[0x41, 0, 0x42, 0, 0, 0, 0x43, 0]), "AB");
    }
}
