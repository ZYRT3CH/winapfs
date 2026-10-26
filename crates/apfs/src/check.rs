//! Whole-volume self-check, used to validate this crate against real disks.
//!
//! Metadata: one pass over every file-system record (so every tree node's
//! checksum), cross-checking counts and references. Data: a spread sample of
//! files whose formats carry their own structure or checksums, read through
//! both the streaming and the random-access path.

use std::collections::{HashMap, HashSet};
use std::io::{self, Write};

use crate::bytes::{u16_at, u64_at};
use crate::decmpfs::DECMPFS_XATTR;
use crate::error::Result;
use crate::fs::{
    FileKind, Inode, Volume, APFS_TYPE_DIR_REC, APFS_TYPE_FILE_EXTENT, APFS_TYPE_INODE, APFS_TYPE_XATTR, OBJ_ID_MASK,
    PRIV_DIR_INO_NUM, ROOT_DIR_INO_NUM, ROOT_DIR_PARENT,
};

const MIB: u64 = 1 << 20;

pub struct CheckOptions {
    /// Files sampled per recognised format.
    pub samples_per_format: usize,
    /// Sampled files up to this size are read twice (streaming and random
    /// access) and compared byte for byte.
    pub compare_limit: u64,
    /// Total uncompressed bytes spent decompressing transparently compressed files.
    pub compressed_budget: u64,
}

impl Default for CheckOptions {
    fn default() -> Self {
        Self { samples_per_format: 40, compare_limit: 32 * MIB, compressed_budget: 2048 * MIB }
    }
}

#[derive(Debug, Default)]
pub struct Problems {
    pub count: u64,
    pub examples: Vec<String>,
}

impl Problems {
    fn add(&mut self, msg: String) {
        self.count += 1;
        if self.examples.len() < 25 {
            self.examples.push(msg);
        }
    }
}

#[derive(Debug, Clone)]
pub struct FormatResult {
    pub format: &'static str,
    pub checked: u64,
    pub ok: u64,
    /// Files whose content is another recognised format, intact as that format.
    pub mislabelled: u64,
}

#[derive(Debug, Default)]
pub struct CheckReport {
    pub records: u64,
    pub files: u64,
    pub dirs: u64,
    pub symlinks: u64,
    pub other: u64,
    pub compressed: u64,
    pub dir_entries: u64,
    pub xattrs: u64,
    pub extents: u64,
    pub formats: Vec<FormatResult>,
    pub compressed_checked: u64,
    pub compressed_ok: u64,
    pub compared: u64,
    pub data_bytes: u64,
    pub problems: Problems,
    /// Observations that are not faults (e.g. a mislabelled file).
    pub notes: Vec<String>,
}

struct InodeInfo {
    kind: FileKind,
    private_id: u64,
    size: u64,
    compressed: bool,
    name: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Format {
    Jpeg,
    Png,
    Bmff,
    Zip,
    Pdf,
}

const FORMATS: [(Format, &str); 5] =
    [(Format::Jpeg, "jpeg"), (Format::Png, "png"), (Format::Bmff, "mov/mp4/heic"), (Format::Zip, "zip/office"), (Format::Pdf, "pdf")];

fn format_label(format: Format) -> &'static str {
    FORMATS.iter().find(|(f, _)| *f == format).map_or("?", |(_, l)| l)
}

fn format_of(name: &str) -> Option<Format> {
    // Office lock files (~$x.docx) and AppleDouble sidecars (._x.png) borrow
    // the extension without being that format.
    if name.starts_with("~$") || name.starts_with("._") {
        return None;
    }
    let ext = name.rsplit_once('.')?.1.to_ascii_lowercase();
    Some(match ext.as_str() {
        "jpg" | "jpeg" => Format::Jpeg,
        "png" => Format::Png,
        "mov" | "mp4" | "m4v" | "m4a" | "heic" | "3gp" => Format::Bmff,
        "zip" | "docx" | "xlsx" | "pptx" => Format::Zip,
        "pdf" => Format::Pdf,
        _ => return None,
    })
}

/// `progress` receives short status lines while the check runs.
pub fn check_volume(v: &Volume, opts: &CheckOptions, progress: &mut dyn FnMut(&str)) -> Result<CheckReport> {
    let mut rep = CheckReport::default();
    let mut inodes: HashMap<u64, InodeInfo> = HashMap::new();
    let mut drecs: Vec<(u64, u64)> = Vec::new();
    let mut allocated: HashMap<u64, u64> = HashMap::new();
    let mut has_decmpfs: HashSet<u64> = HashSet::new();
    let mut last_extent: Option<(u64, u64)> = None; // (stream id, end of previous extent)

    progress("metadata: scanning every file-system record");
    let mut problems = Problems::default();
    v.scan_all(&mut |k, val| {
        rep.records += 1;
        if rep.records % 200_000 == 0 {
            progress(&format!("metadata: {} records", rep.records));
        }
        let hdr = u64_at(k, 0)?;
        let id = hdr & OBJ_ID_MASK;
        match (hdr >> 60) as u8 {
            APFS_TYPE_INODE => {
                let ino = Inode::parse(id, val)?;
                match ino.kind() {
                    FileKind::Regular => rep.files += 1,
                    // The superblock's count leaves out the root and private directories.
                    FileKind::Directory if id == ROOT_DIR_INO_NUM || id == PRIV_DIR_INO_NUM => {}
                    FileKind::Directory => rep.dirs += 1,
                    FileKind::Symlink => rep.symlinks += 1,
                    _ => rep.other += 1,
                }
                rep.compressed += ino.is_compressed() as u64;
                inodes.insert(
                    id,
                    InodeInfo {
                        kind: ino.kind(),
                        private_id: ino.private_id,
                        size: ino.dstream.map_or(0, |d| d.size),
                        compressed: ino.is_compressed(),
                        name: ino.name,
                    },
                );
            }
            APFS_TYPE_DIR_REC => {
                rep.dir_entries += 1;
                drecs.push((id, u64_at(val, 0)?));
            }
            APFS_TYPE_XATTR => {
                rep.xattrs += 1;
                let name_len = u16_at(k, 8)? as usize;
                if k.get(10..10 + name_len).is_some_and(|n| n.starts_with(DECMPFS_XATTR.as_bytes())) {
                    has_decmpfs.insert(id);
                }
            }
            APFS_TYPE_FILE_EXTENT => {
                rep.extents += 1;
                let logical = u64_at(k, 8)?;
                let len = u64_at(val, 0)? & 0x00ff_ffff_ffff_ffff;
                if let Some((sid, end)) = last_extent {
                    if sid == id && logical < end {
                        problems.add(format!("data stream {id:#x}: extent at {logical} overlaps the previous one"));
                    }
                }
                last_extent = Some((id, logical + len));
                *allocated.entry(id).or_default() += len;
            }
            _ => {}
        }
        Ok(true)
    })?;
    rep.problems = problems;

    progress("metadata: cross-checking references");
    for &(parent, target) in &drecs {
        if !inodes.contains_key(&target) {
            rep.problems.add(format!("folder {parent} lists inode {target}, which does not exist"));
        }
        if parent != ROOT_DIR_PARENT && !inodes.contains_key(&parent) {
            rep.problems.add(format!("directory entries exist under missing folder {parent}"));
        }
    }
    for (id, ino) in &inodes {
        if ino.kind != FileKind::Regular {
            continue;
        }
        if ino.compressed {
            if !has_decmpfs.contains(id) {
                rep.problems.add(format!("compressed file {id} has no {DECMPFS_XATTR} attribute"));
            }
        } else if ino.size > allocated.get(&ino.private_id).copied().unwrap_or(0) {
            rep.problems.add(format!(
                "file {id} ({}) is {} bytes but its extents cover only {}",
                ino.name.as_deref().unwrap_or("?"),
                ino.size,
                allocated.get(&ino.private_id).copied().unwrap_or(0)
            ));
        }
    }

    check_data(v, opts, &inodes, &mut rep, progress)?;
    Ok(rep)
}

fn check_data(
    v: &Volume,
    opts: &CheckOptions,
    inodes: &HashMap<u64, InodeInfo>,
    rep: &mut CheckReport,
    progress: &mut dyn FnMut(&str),
) -> Result<()> {
    // Deterministic, spread-out samples: sort by inode id, take evenly spaced picks.
    let mut ids: Vec<u64> = inodes.iter().filter(|(_, i)| i.kind == FileKind::Regular).map(|(&id, _)| id).collect();
    ids.sort_unstable();

    for (format, label) in FORMATS {
        let candidates: Vec<u64> = ids
            .iter()
            .copied()
            .filter(|id| {
                let i = &inodes[id];
                !i.compressed && i.size > 0 && i.name.as_deref().and_then(format_of) == Some(format)
            })
            .collect();
        let n = candidates.len().min(opts.samples_per_format);
        let mut result = FormatResult { format: label, checked: 0, ok: 0, mislabelled: 0 };
        for k in 0..n {
            let id = candidates[k * candidates.len() / n];
            progress(&format!("data: {label} {}/{n}", k + 1));
            let ino = v.inode(id)?;
            let name = ino.name.as_deref().unwrap_or("?");
            result.checked += 1;
            match check_file(v, &ino, format, opts, rep) {
                Ok(None) => result.ok += 1,
                Ok(Some(really)) => {
                    result.mislabelled += 1;
                    if rep.notes.len() < 20 {
                        rep.notes.push(format!("{name} (file {id}) is really {really}"));
                    }
                }
                Err(e) => rep.problems.add(format!("{label} file {id} ({name}): {e}")),
            }
        }
        rep.formats.push(result);
    }

    let mut spent = 0u64;
    for &id in ids.iter().filter(|id| inodes[id].compressed) {
        if spent >= opts.compressed_budget {
            break;
        }
        let ino = v.inode(id)?;
        rep.compressed_checked += 1;
        if rep.compressed_checked % 250 == 0 {
            progress(&format!("data: {} compressed files decompressed", rep.compressed_checked));
        }
        let mut sink = Counter(0);
        let outcome = v.file_size(&ino).and_then(|size| v.read_file(&ino, &mut sink).map(|n| (size, n)));
        match outcome {
            Ok((size, n)) if n == size && sink.0 == size => rep.compressed_ok += 1,
            Ok((size, n)) => rep.problems.add(format!("compressed file {id}: expected {size} bytes, got {n}/{}", sink.0)),
            Err(e) => rep.problems.add(format!("compressed file {id} ({}): {e}", ino.name.as_deref().unwrap_or("?"))),
        }
        spent += sink.0;
        rep.data_bytes += sink.0;
    }
    Ok(())
}

struct Counter(u64);

impl Write for Counter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0 += buf.len() as u64;
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn check_file(
    v: &Volume,
    ino: &Inode,
    format: Format,
    opts: &CheckOptions,
    rep: &mut CheckReport,
) -> std::result::Result<Option<String>, String> {
    let size = ino.dstream.map_or(0, |d| d.size);
    let mut bytes_read = 0u64;
    let mut read = |off: u64, len: usize| -> std::result::Result<Vec<u8>, String> {
        let mut b = vec![0u8; len];
        let n = v.read_at(ino, off, &mut b).map_err(|e| e.to_string())?;
        b.truncate(n);
        bytes_read += n as u64;
        Ok(b)
    };
    // Trust the content over the extension: a mislabelled file is checked as
    // what it really is. Landing on another format's exact header by misreading
    // a block is not plausible, so a recognised header also vouches for the read.
    let head = read(0, size.min(16) as usize)?;
    let actual = sniff(&head);
    if actual.is_none() {
        if let Some(kind) = unchecked_format(&head) {
            rep.data_bytes += bytes_read;
            return Ok(Some(format!("{kind} (a format verify does not check)")));
        }
    }
    let target = actual.unwrap_or(format);
    let verify_crc = size <= opts.compare_limit;
    let outcome = check_structure(target, size, verify_crc, &mut read).map_err(|e| match actual {
        None => format!("{e}; content starts with {}", hex(&head)),
        Some(_) => e,
    });
    rep.data_bytes += bytes_read;
    outcome?;

    if size <= opts.compare_limit {
        // Read the whole file through the streaming path (what `get` uses) and
        // compare it with the random-access path (what a mounted drive uses),
        // with deliberately misaligned piece sizes.
        let mut streamed = Vec::with_capacity(size as usize);
        v.read_file(ino, &mut streamed).map_err(|e| e.to_string())?;
        if streamed.len() as u64 != size {
            return Err(format!("streamed {} bytes, expected {size}", streamed.len()));
        }
        let piece = MIB as usize + 4097;
        let mut off = 0usize;
        while off < streamed.len() {
            let mut b = vec![0u8; piece.min(streamed.len() - off)];
            v.read_at(ino, off as u64, &mut b).map_err(|e| e.to_string())?;
            if b[..] != streamed[off..off + b.len()] {
                return Err(format!("streaming and random-access reads differ near offset {off}"));
            }
            off += b.len();
        }
        rep.compared += 1;
        rep.data_bytes += 2 * size;
    }
    Ok((target != format).then(|| format!("{}, intact as that", format_label(target))))
}

const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];

/// Recognises a format this module can check from a file's first bytes.
fn sniff(head: &[u8]) -> Option<Format> {
    const FIRST_BOXES: [&[u8; 4]; 7] = [b"ftyp", b"moov", b"mdat", b"wide", b"free", b"skip", b"pnot"];
    if head.starts_with(&[0xff, 0xd8, 0xff]) {
        Some(Format::Jpeg)
    } else if head.starts_with(&PNG_SIGNATURE) {
        Some(Format::Png)
    } else if head.starts_with(b"PK\x03\x04") || head.starts_with(b"PK\x05\x06") {
        Some(Format::Zip)
    } else if head.starts_with(b"%PDF-") {
        Some(Format::Pdf)
    } else if head.len() >= 8 && FIRST_BOXES.iter().any(|b| head[4..8] == b[..]) {
        Some(Format::Bmff)
    } else {
        None
    }
}

/// Common formats with an unmistakable header that this module cannot check.
fn unchecked_format(head: &[u8]) -> Option<&'static str> {
    Some(if head.starts_with(b"GIF87a") || head.starts_with(b"GIF89a") {
        "a GIF image"
    } else if head.starts_with(b"RIFF") && head.get(8..12) == Some(&b"WEBP"[..]) {
        "a WebP image"
    } else if head.starts_with(b"II*\0") || head.starts_with(b"MM\0*") {
        "a TIFF image"
    } else if head.starts_with(b"8BPS") {
        "a Photoshop document"
    } else if head.starts_with(b"bplist00") {
        "a binary property list"
    } else {
        return None;
    })
}

fn hex(bytes: &[u8]) -> String {
    let h: Vec<String> = bytes.iter().map(|b| format!("{b:02x}")).collect();
    let printable: String = bytes.iter().map(|&b| if b.is_ascii_graphic() { b as char } else { '.' }).collect();
    format!("{} \"{printable}\"", h.join(" "))
}

type ReadFn<'a> = dyn FnMut(u64, usize) -> std::result::Result<Vec<u8>, String> + 'a;

fn check_structure(format: Format, size: u64, verify_crc: bool, read: &mut ReadFn) -> std::result::Result<(), String> {
    match format {
        Format::Jpeg => check_jpeg(size, read),
        Format::Png => check_png(size, verify_crc, read),
        Format::Bmff => check_bmff(size, read),
        Format::Zip => check_zip(size, read),
        Format::Pdf => check_pdf(size, read),
    }
}

fn be32(b: &[u8], off: usize) -> u32 {
    u32::from_be_bytes(b[off..off + 4].try_into().unwrap())
}

/// SOI, then a chain of length-prefixed marker segments up to the image data.
fn check_jpeg(size: u64, read: &mut ReadFn) -> std::result::Result<(), String> {
    let head = read(0, size.min(256 * 1024) as usize)?;
    if !head.starts_with(&[0xff, 0xd8, 0xff]) {
        return Err("missing JPEG start-of-image marker".into());
    }
    let mut p = 2usize;
    while p + 4 <= head.len() {
        if head[p] != 0xff {
            return Err(format!("expected a JPEG marker at offset {p}"));
        }
        match head[p + 1] {
            0xff => p += 1,
            0x01 | 0xd0..=0xd8 => p += 2,
            0xd9 | 0xda => return Ok(()), // end of image / start of scan
            _ => p += 2 + u16::from_be_bytes([head[p + 2], head[p + 3]]) as usize,
        }
    }
    Ok(())
}

/// Signature, then chunks up to IEND; with `verify_crc`, every chunk's CRC,
/// which covers every byte of the file.
fn check_png(size: u64, verify_crc: bool, read: &mut ReadFn) -> std::result::Result<(), String> {
    if read(0, 8)? != PNG_SIGNATURE {
        return Err("missing PNG signature".into());
    }
    let mut off = 8u64;
    for _ in 0..1_000_000 {
        let h = read(off, 8)?;
        if h.len() < 8 {
            return Err(format!("PNG ends at {off} without an IEND chunk"));
        }
        let len = be32(&h, 0) as u64;
        let kind = [h[4], h[5], h[6], h[7]];
        if !kind.iter().all(u8::is_ascii_alphabetic) || off + 12 + len > size {
            return Err(format!("bad PNG chunk header at offset {off}"));
        }
        if verify_crc {
            let body = read(off + 4, 4 + len as usize + 4)?;
            let n = 4 + len as usize;
            if crc32(&body[..n]) != be32(&body, n) {
                return Err(format!("CRC mismatch in {} chunk at offset {off}", String::from_utf8_lossy(&kind)));
            }
        }
        off += 12 + len;
        if &kind == b"IEND" {
            return Ok(());
        }
    }
    Err("too many PNG chunks".into())
}

/// ISO base media (QuickTime/MP4/HEIC): top-level boxes must tile the file.
fn check_bmff(size: u64, read: &mut ReadFn) -> std::result::Result<(), String> {
    let mut off = 0u64;
    for _ in 0..1_000_000 {
        if off == size {
            return Ok(());
        }
        let h = read(off, (size - off).min(16) as usize)?;
        if h.len() < 8 || (be32(&h, 0) == 0 && h[4..8] == [0; 4]) {
            return if h.iter().all(|&b| b == 0) && trailing_zeros(read, off, size)? {
                Ok(()) // zero padding after the last box
            } else {
                Err(format!("truncated box header at offset {off}"))
            };
        }
        if !h[4..8].iter().all(|c| c.is_ascii_graphic() || *c == b' ') {
            return Err(format!("unreadable box type {:02x?} at offset {off} of {size}", &h[4..8]));
        }
        let len = match be32(&h, 0) {
            0 => size - off,
            1 if h.len() >= 16 => u64::from_be_bytes(h[8..16].try_into().unwrap()),
            1 => return Err(format!("truncated 64-bit box size at offset {off}")),
            n => n as u64,
        };
        if len < 8 || off + len > size {
            return Err(format!("box at offset {off} claims {len} bytes in a {size}-byte file"));
        }
        off += len;
    }
    Err("too many top-level boxes".into())
}

fn trailing_zeros(read: &mut ReadFn, mut off: u64, size: u64) -> std::result::Result<bool, String> {
    while off < size {
        let b = read(off, (size - off).min(MIB) as usize)?;
        if b.is_empty() || b.iter().any(|&c| c != 0) {
            return Ok(false);
        }
        off += b.len() as u64;
    }
    Ok(true)
}

/// End-of-central-directory record, the central directory it points to, and
/// a spread of local file headers the directory points to.
fn check_zip(size: u64, read: &mut ReadFn) -> std::result::Result<(), String> {
    let tail_len = size.min(65_535 + 22);
    let tail = read(size - tail_len, tail_len as usize)?;
    let eocd = (0..tail.len().saturating_sub(21))
        .rev()
        .find(|&i| tail[i..i + 4] == *b"PK\x05\x06")
        .ok_or("no ZIP end-of-central-directory record")?;
    let le16 = |b: &[u8], o: usize| u16::from_le_bytes([b[o], b[o + 1]]) as usize;
    let le32 = |b: &[u8], o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap()) as u64;
    let cd_size = le32(&tail, eocd + 12);
    let cd_off = le32(&tail, eocd + 16);
    if cd_off == 0xffff_ffff || cd_size == 0xffff_ffff {
        return Ok(()); // ZIP64; the EOCD record alone is our check
    }
    if cd_off + cd_size > size {
        return Err(format!("central directory at {cd_off}+{cd_size} lies outside the file"));
    }
    let cd = read(cd_off, cd_size as usize)?;
    let mut locals = Vec::new();
    let mut p = 0usize;
    while p + 46 <= cd.len() {
        if cd[p..p + 4] != *b"PK\x01\x02" {
            return Err(format!("bad central directory entry at offset {}", cd_off + p as u64));
        }
        locals.push(le32(&cd, p + 42));
        p += 46 + le16(&cd, p + 28) + le16(&cd, p + 30) + le16(&cd, p + 32);
    }
    let step = locals.len().div_ceil(64).max(1);
    for &off in locals.iter().step_by(step) {
        if off != 0xffff_ffff && read(off, 4)? != *b"PK\x03\x04" {
            return Err(format!("central directory points at offset {off}, which is not a local file header"));
        }
    }
    Ok(())
}

fn check_pdf(size: u64, read: &mut ReadFn) -> std::result::Result<(), String> {
    if !read(0, 5)?.starts_with(b"%PDF-") {
        return Err("missing %PDF- header".into());
    }
    let tail_len = size.min(4096);
    let tail = read(size - tail_len, tail_len as usize)?;
    if !tail.windows(5).any(|w| w == b"%%EOF") {
        return Err("no %%EOF marker in the last 4 KiB".into());
    }
    Ok(())
}

/// CRC-32 (IEEE 802.3), as used by PNG and ZIP.
fn crc32(data: &[u8]) -> u32 {
    static TABLE: std::sync::OnceLock<[u32; 256]> = std::sync::OnceLock::new();
    let table = TABLE.get_or_init(|| {
        let mut t = [0u32; 256];
        for (i, e) in t.iter_mut().enumerate() {
            let mut c = i as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xedb8_8320 ^ (c >> 1) } else { c >> 1 };
            }
            *e = c;
        }
        t
    });
    !data.iter().fold(!0u32, |c, &b| table[((c ^ b as u32) & 0xff) as usize] ^ (c >> 8))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reader(data: &[u8]) -> impl FnMut(u64, usize) -> std::result::Result<Vec<u8>, String> + '_ {
        move |off, len| {
            let s = (off as usize).min(data.len());
            Ok(data[s..(s + len).min(data.len())].to_vec())
        }
    }

    #[test]
    fn content_beats_extension() {
        assert_eq!(sniff(&[0xff, 0xd8, 0xff, 0xe0]), Some(Format::Jpeg));
        assert_eq!(sniff(b"\0\0\0\x14ftypqt  "), Some(Format::Bmff));
        assert_eq!(sniff(b"GIF89a"), None);
        assert_eq!(unchecked_format(b"GIF89a"), Some("a GIF image"));
        assert_eq!(format_of("Split_LinesVert_08.PNG"), Some(Format::Png));
        assert_eq!(format_of("~$ief Filmproduksjon LNS.docx"), None);
        assert_eq!(format_of("._IMG_0001.jpg"), None);
    }

    #[test]
    fn crc32_known_value() {
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
    }

    #[test]
    fn png_chunks_and_crc() {
        let mut png = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        for (kind, body) in [(b"IHDR", &[0u8; 13][..]), (b"IEND", &[][..])] {
            png.extend_from_slice(&(body.len() as u32).to_be_bytes());
            let mut c = kind.to_vec();
            c.extend_from_slice(body);
            png.extend_from_slice(&c);
            png.extend_from_slice(&crc32(&c).to_be_bytes());
        }
        assert!(check_png(png.len() as u64, true, &mut reader(&png)).is_ok());
        png[20] ^= 1; // corrupt IHDR data
        assert!(check_png(png.len() as u64, true, &mut reader(&png)).is_err());
    }

    #[test]
    fn bmff_boxes_must_tile() {
        let mut mov = Vec::new();
        mov.extend_from_slice(&16u32.to_be_bytes());
        mov.extend_from_slice(b"ftypqt  \0\0\0\0");
        mov.extend_from_slice(&8u32.to_be_bytes());
        mov.extend_from_slice(b"free");
        assert!(check_bmff(mov.len() as u64, &mut reader(&mov)).is_ok());
        mov.extend_from_slice(&[0xde, 0xad]); // two stray bytes
        assert!(check_bmff(mov.len() as u64, &mut reader(&mov)).is_err());
    }
}
