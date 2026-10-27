//! `winapfs dev-rehearse <source> <script> [metadata-folder]`: runs a write
//! script against a real disk with every write kept in memory, so the write
//! code meets a container macOS made without the disk being touched.
//!
//! With a metadata folder, the container's metadata (every allocated block
//! that is not file data) is exported before and after, into sparse images
//! of the container's full size, for checking with apfsck.

use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::time::Instant;

use apfs::spaceman::{self, SD_MAIN, SFQ_IP, SFQ_MAIN};
use apfs::write::{check_writable, restore_blocks, Session};
use apfs::{format_uuid, Container};

use crate::{human_size, open_device, run_script, CliError, CliResult};

const APFS_TYPE_XATTR: u64 = 4;
const APFS_TYPE_FILE_EXTENT: u64 = 8;
const OBJ_ID_MASK: u64 = 0x0fff_ffff_ffff_ffff;
const XATTR_DATA_STREAM: u16 = 1;
const FILE_EXTENT_LEN_MASK: u64 = 0x00ff_ffff_ffff_ffff;

pub fn run(src: &str, script: &str, vol: usize, part: Option<u32>, meta: Option<&Path>) -> CliResult {
    let (dev, location, ..) = open_device(src, part)?;
    let c = Container::open(Box::new(dev.try_clone()?))?;
    println!("== {location}");
    summary(&c)?;

    let problems = check_writable(&c);
    println!("\n== can winapfs write here?");
    if problems.is_empty() {
        println!("  yes: everything a transaction loads was read and understood");
    }
    for p in &problems {
        println!("  BLOCKER  {p}");
    }

    if let Some(dir) = meta {
        std::fs::create_dir_all(dir)?;
        println!("\n== exporting metadata, before");
        export_metadata(&c, &dir.join("before.img"))?;
    }
    if !problems.is_empty() {
        return Err(CliError::new("not rehearsing: the blockers above come first"));
    }

    println!("\n== rehearsal: running {script} with writes kept in memory");
    let start = Instant::now();
    let (mut s, overlay) = Session::rehearsal(dev, vol)?;
    let before_xid = s.container().xid();
    let result = run_script(&mut s, script);
    println!(
        "  {}: xid {} -> {}, {} blocks written to memory, {:.1} s",
        if result.is_ok() { "done" } else { "STOPPED" },
        before_xid,
        s.container().xid(),
        overlay.written(),
        start.elapsed().as_secs_f64()
    );
    if let Err(e) = &result {
        println!("  error: {}", e.msg);
    }
    if let Ok(v) = s.volume() {
        println!("  volume \"{}\" now: {} files, {} folders", v.sb.name, v.sb.num_files, v.sb.num_directories);
    }
    if let Some(dir) = meta {
        println!("\n== exporting metadata, after");
        export_metadata(s.container(), &dir.join("after.img"))?;
    }
    result
}

fn summary(c: &Container) -> CliResult {
    let sb = &c.sb;
    let bs = c.block_size() as u64;
    println!("container  {}  block {bs}  xid {}  volumes {}", human_size(sb.block_count * bs), sb.hdr.xid, sb.fs_oids.len());
    println!(
        "checkpoints  descriptor ring {} blocks (in use {}), data ring {} blocks (in use {}), map entries {}",
        sb.xp_desc_blocks & 0x7fff_ffff,
        sb.xp_desc_len,
        sb.xp_data_blocks & 0x7fff_ffff,
        sb.xp_data_len,
        c.checkpoint_map.len()
    );
    let sm = c.space_manager()?;
    let d = &sm.dev[SD_MAIN];
    println!(
        "space manager  version {}  chunks {}  cibs {}  cabs {}  free {}  tier2 blocks {}",
        sm.version,
        d.chunk_count,
        d.cib_count,
        d.cab_count,
        human_size(d.free_count * bs),
        sm.dev[1].block_count
    );
    println!(
        "internal pool  {} blocks at {:#x}  bitmap {} block(s) x ring of {} at {:#x}",
        sm.ip_block_count, sm.ip_base, sm.ip_bm_size_in_blocks, sm.ip_bm_block_count, sm.ip_bm_base
    );
    for (name, q) in [("internal pool", SFQ_IP), ("main", SFQ_MAIN)] {
        let fq = &sm.fq[q];
        println!(
            "free queue {name}: {} blocks queued, oldest xid {}, node limit {}",
            fq.count, fq.oldest_xid, fq.tree_node_limit
        );
    }
    for (i, v) in c.volume_superblocks()?.iter().enumerate() {
        println!(
            "volume {i} \"{}\"  files {} folders {}  snapshots {}  used {}  last written by {}",
            v.name,
            v.num_files,
            v.num_directories,
            v.num_snapshots,
            human_size(v.alloc_blocks * bs),
            v.last_modified_by
        );
    }
    Ok(())
}

/// One bit per block of the container.
struct Bits(Vec<u64>);

impl Bits {
    fn new(n: u64) -> Self {
        Self(vec![0; n.div_ceil(64) as usize])
    }
    fn set_range(&mut self, start: u64, len: u64) {
        for b in start..(start + len).min(self.0.len() as u64 * 64) {
            self.0[(b / 64) as usize] |= 1 << (b % 64);
        }
    }
    fn get(&self, b: u64) -> bool {
        self.0.get((b / 64) as usize).is_some_and(|w| w >> (b % 64) & 1 == 1)
    }
}

/// Copies every metadata block of `c` into a sparse image of the
/// container's size, at the same offsets; file data is left out.
fn export_metadata(c: &Container, out: &Path) -> CliResult {
    let start = Instant::now();
    let bs = c.block_size() as u64;
    let n = c.sb.block_count;
    let sm = c.space_manager()?;

    // Allocated blocks, from the chunk bitmaps.
    let mut alloc = Bits::new(n);
    for cib in sm.dev_addrs(SD_MAIN)? {
        let (_, chunks) = spaceman::chunk_infos(&c.raw_block(cib)?)?;
        for ch in chunks {
            if ch.bitmap_addr == 0 {
                continue; // nothing allocated in this chunk
            }
            let bm = c.raw_block(ch.bitmap_addr)?;
            for i in 0..ch.block_count as u64 {
                if bm[(i / 8) as usize] >> (i % 8) & 1 == 1 {
                    alloc.set_range(ch.addr + i, 1);
                }
            }
        }
    }

    // File data: the extents of every data stream except extended
    // attributes' (kept, as checkers may read them).
    let mut data = Bits::new(n);
    for i in 0..c.sb.fs_oids.len() {
        let v = c.volume(i)?;
        let mut xattr_streams = HashSet::new();
        let mut extents = Vec::new();
        v.scan_all(&mut |k, val| {
            let hdr = u64::from_le_bytes(k[0..8].try_into().unwrap());
            match hdr >> 60 {
                APFS_TYPE_XATTR if val.len() >= 12 => {
                    if u16::from_le_bytes([val[0], val[1]]) & XATTR_DATA_STREAM != 0 {
                        xattr_streams.insert(u64::from_le_bytes(val[4..12].try_into().unwrap()));
                    }
                }
                APFS_TYPE_FILE_EXTENT if val.len() >= 16 => {
                    let len = u64::from_le_bytes(val[0..8].try_into().unwrap()) & FILE_EXTENT_LEN_MASK;
                    let phys = u64::from_le_bytes(val[8..16].try_into().unwrap());
                    if phys != 0 {
                        extents.push((hdr & OBJ_ID_MASK, phys, len.div_ceil(bs)));
                    }
                }
                _ => {}
            }
            Ok(true)
        })?;
        for (stream, phys, blocks) in extents {
            if !xattr_streams.contains(&stream) {
                data.set_range(phys, blocks);
            }
        }
    }

    // Plus the fixed areas, whatever the bitmaps say.
    let mut keep = Bits::new(n);
    let sb = &c.sb;
    keep.set_range(0, 1);
    keep.set_range(sb.xp_desc_base, (sb.xp_desc_blocks & 0x7fff_ffff) as u64);
    keep.set_range(sb.xp_data_base, (sb.xp_data_blocks & 0x7fff_ffff) as u64);
    keep.set_range(sm.ip_base, sm.ip_block_count);
    keep.set_range(sm.ip_bm_base, sm.ip_bm_block_count as u64);

    let mut f = sparse_file(out, n * bs)?;
    let (mut copied, mut b) = (0u64, 0u64);
    while b < n {
        let wanted = |b: u64| keep.get(b) || (alloc.get(b) && !data.get(b));
        if !wanted(b) {
            b += 1;
            continue;
        }
        let mut run = 1;
        while b + run < n && run < 256 && wanted(b + run) {
            run += 1;
        }
        let bytes = c.raw_blocks(b, run)?;
        write_at(&mut f, b * bs, &bytes)?;
        copied += run;
        b += run;
    }
    f.flush()?;
    println!(
        "  {} of metadata into {} ({} of file data left out), {:.1} s",
        human_size(copied * bs),
        out.display(),
        human_size(data.0.iter().map(|w| w.count_ones() as u64).sum::<u64>() * bs),
        start.elapsed().as_secs_f64()
    );
    Ok(())
}

fn write_at(f: &mut File, off: u64, buf: &[u8]) -> CliResult {
    use std::os::windows::fs::FileExt;
    let mut done = 0;
    while done < buf.len() {
        done += f.seek_write(&buf[done..], off + done as u64)?;
    }
    Ok(())
}

/// Creates `path` as a sparse file of `len` bytes. Refuses to go on unless
/// NTFS confirms the file is sparse: a plain file would fill the disk with
/// zeros up to the highest block written.
fn sparse_file(path: &Path, len: u64) -> CliResult<File> {
    use std::os::windows::io::AsRawHandle;
    #[link(name = "kernel32")]
    extern "system" {
        fn DeviceIoControl(
            h: *mut std::ffi::c_void,
            code: u32,
            inb: *mut std::ffi::c_void,
            inlen: u32,
            outb: *mut std::ffi::c_void,
            outlen: u32,
            ret: *mut u32,
            ov: *mut std::ffi::c_void,
        ) -> i32;
    }
    const FSCTL_SET_SPARSE: u32 = 0x0009_00C4;
    let f = OpenOptions::new().read(true).write(true).create(true).truncate(true).open(path)?;
    let mut ret = 0u32;
    let ok = unsafe {
        DeviceIoControl(f.as_raw_handle(), FSCTL_SET_SPARSE, std::ptr::null_mut(), 0, std::ptr::null_mut(), 0, &mut ret, std::ptr::null_mut())
    };
    if ok == 0 {
        return Err(CliError::new(format!("could not make {} sparse; not exporting", path.display())));
    }
    f.set_len(len)?;
    Ok(f)
}

const UNDO_MAGIC: &[u8; 8] = b"WAPFSUN1";

/// Parses `E0BAE11E-CADF-4226-BD15-E0B7FFB82FDC` into the bytes stored on disk.
fn parse_uuid(s: &str) -> CliResult<[u8; 16]> {
    let hex: String = s.chars().filter(|c| *c != '-').collect();
    if hex.len() != 32 {
        return Err(CliError::new(format!("{s} is not a container UUID")));
    }
    let mut out = [0u8; 16];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).map_err(|_| CliError::new(format!("{s} is not a container UUID")))?;
    }
    Ok(out)
}

/// `dev-real-write <source> <script> <container-uuid> <undo-file>`: writes
/// for real, as safely as it can be done:
///
/// 1. refuses unless the container has the given UUID and nothing blocks writing;
/// 2. rehearses the script on this disk with writes kept in memory, and
///    checks the space map of the result;
/// 3. saves an undo point: the blocks a transaction changes in place (block
///    0 and the checkpoint areas), verified by reading them back;
/// 4. runs the script as one transaction, so until the drive is used again
///    nothing the previous checkpoint uses has been overwritten;
/// 5. checks the space map on the disk afterwards.
pub fn real_write(src: &str, script: &str, uuid: &str, undo: &Path, vol: usize, part: Option<u32>) -> CliResult {
    let uuid = parse_uuid(uuid)?;
    let (dev, location, path, base) = open_device(src, part)?;
    let c = Container::open(Box::new(dev.try_clone()?))?;
    println!("== {location}");
    summary(&c)?;
    if c.sb.uuid != uuid {
        return Err(CliError::new(format!(
            "this is container {}, not {}; nothing written",
            format_uuid(&c.sb.uuid),
            format_uuid(&uuid)
        )));
    }
    let problems = check_writable(&c);
    if !problems.is_empty() {
        return Err(CliError::new(format!("cannot write here: {}", problems.join("; "))));
    }

    println!("\n== step 1 of 4: rehearsal on this disk, writes kept in memory");
    let (mut s, overlay) = Session::rehearsal(dev.try_clone()?, vol)?;
    s.set_batch(true);
    run_script(&mut s, script)?;
    println!("  xid {} -> {}, {} blocks written to memory", c.xid(), s.container().xid(), overlay.written());
    crate::dump::space_check_container(s.container(), "  rehearsal result")?;
    drop(s);

    println!("\n== step 2 of 4: saving the undo point to {}", undo.display());
    save_undo(&c, undo)?;

    println!("\n== step 3 of 4: writing to the disk, one transaction");
    let start = Instant::now();
    let mut s = Session::open_real_disk(Path::new(&path), base, uuid, vol)?;
    s.set_batch(true);
    run_script(&mut s, script)?;
    println!("  committed xid {} in {:.1} s", s.container().xid(), start.elapsed().as_secs_f64());
    drop(s);

    println!("\n== step 4 of 4: checking the disk");
    let (dev, ..) = open_device(src, part)?;
    let after = Container::open(Box::new(dev))?;
    crate::dump::space_check_container(&after, &location)?;
    let v = after.volume(vol)?;
    println!("  volume \"{}\": {} files, {} folders", v.sb.name, v.sb.num_files, v.sb.num_directories);
    println!("\nundo (only until the drive is used again): winapfs dev-undo {src} {}", undo.display());
    Ok(())
}

/// Saves block 0 and both checkpoint areas: all a transaction overwrites
/// in place. Everything else it writes goes to blocks that were free.
fn save_undo(c: &Container, path: &Path) -> CliResult {
    if path.exists() {
        return Err(CliError::new(format!("{} already exists; not overwriting an undo point", path.display())));
    }
    let sb = &c.sb;
    let ranges = [
        (sb.xp_data_base, (sb.xp_data_blocks & 0x7fff_ffff) as u64),
        (sb.xp_desc_base, (sb.xp_desc_blocks & 0x7fff_ffff) as u64),
        (0, 1),
    ];
    let mut out = Vec::new();
    out.extend(UNDO_MAGIC);
    out.extend(sb.uuid);
    out.extend(c.xid().to_le_bytes());
    out.extend(c.block_size().to_le_bytes());
    out.extend((ranges.len() as u32).to_le_bytes());
    for (start, count) in ranges {
        out.extend(start.to_le_bytes());
        out.extend(count.to_le_bytes());
    }
    for (start, count) in ranges {
        let mut b = start;
        while b < start + count {
            let n = (start + count - b).min(256);
            out.extend(c.raw_blocks(b, n)?);
            b += n;
        }
    }
    let mut f = File::create(path)?;
    f.write_all(&out)?;
    f.sync_all()?;
    drop(f);
    if std::fs::read(path)? != out {
        return Err(CliError::new("the undo file does not read back correctly; nothing written to the disk"));
    }
    println!("  {} saved and verified (container xid {})", human_size(out.len() as u64), c.xid());
    Ok(())
}

struct Undo {
    uuid: [u8; 16],
    xid: u64,
    blocks: Vec<(u64, Vec<u8>)>,
}

fn load_undo(path: &Path) -> CliResult<Undo> {
    let d = std::fs::read(path)?;
    let bad = || CliError::new(format!("{} is not a winapfs undo file", path.display()));
    if d.len() < 40 || &d[0..8] != UNDO_MAGIC {
        return Err(bad());
    }
    let u64_at = |o: usize| d.get(o..o + 8).map(|b| u64::from_le_bytes(b.try_into().unwrap())).ok_or_else(bad);
    let uuid: [u8; 16] = d[8..24].try_into().unwrap();
    let xid = u64_at(24)?;
    let bs = u32::from_le_bytes(d[32..36].try_into().unwrap()) as u64;
    let n = u32::from_le_bytes(d[36..40].try_into().unwrap()) as usize;
    let mut pos = 40 + n * 16;
    let mut blocks = Vec::new();
    for i in 0..n {
        let (start, count) = (u64_at(40 + i * 16)?, u64_at(48 + i * 16)?);
        let len = (count * bs) as usize;
        blocks.push((start, d.get(pos..pos + len).ok_or_else(bad)?.to_vec()));
        pos += len;
    }
    Ok(Undo { uuid, xid, blocks })
}

/// `dev-undo <source> <undo-file>`: puts back the saved blocks, returning the
/// container to the checkpoint before `dev-real-write`. Only safe while that
/// write is still the only transaction since: later ones may have reused
/// blocks the old checkpoint needs, so then it refuses.
pub fn undo(src: &str, undo_path: &Path, part: Option<u32>) -> CliResult {
    let u = load_undo(undo_path)?;
    let (dev, location, path, base) = open_device(src, part)?;
    let c = Container::open(Box::new(dev))?;
    println!("== {location}: container {} at xid {}", format_uuid(&c.sb.uuid), c.xid());
    if c.sb.uuid != u.uuid {
        return Err(CliError::new("this undo file belongs to a different container"));
    }
    if c.xid() == u.xid {
        println!("already at xid {}: nothing to undo", u.xid);
        return Ok(());
    }
    if c.xid() != u.xid + 1 {
        return Err(CliError::new(format!(
            "the drive is at xid {}, but the undo point is for xid {} followed by exactly one write; it has been used since, so undoing is no longer safe",
            c.xid(),
            u.xid
        )));
    }
    drop(c);
    restore_blocks(Path::new(&path), base, u.uuid, &u.blocks)?;
    let (dev, ..) = open_device(src, part)?;
    let back = Container::open(Box::new(dev))?;
    if back.xid() != u.xid {
        return Err(CliError::new(format!("after restoring, the newest checkpoint is xid {}, expected {}", back.xid(), u.xid)));
    }
    println!("restored: the container is back at xid {}", u.xid);
    crate::dump::space_check_container(&back, &location)
}
