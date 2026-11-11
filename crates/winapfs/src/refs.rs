//! `winapfs dev-extent-check <source>`: checks that the count on every
//! extent reference matches how many data streams use its blocks. Written
//! against the APFS reference independently of the write code, so it can
//! check that code's results (rehearsals run it before and after).
//!
//! Each volume keeps an extent-reference tree: per run of file-data blocks,
//! a count of the data streams using them. Clones share blocks, so their
//! count is above one; deleting a file lowers the count and frees only the
//! blocks that reach zero. A count below the real number of users would
//! free blocks another file still uses: data loss.

use std::sync::Arc;

use apfs::btree::{self, Node, NodeSource};
use apfs::{Container, Volume};

use crate::CliResult;

const OBJ_ID_MASK: u64 = 0x0fff_ffff_ffff_ffff;
const TYPE_FILE_EXTENT: u64 = 8;
const FILE_EXTENT_LEN_MASK: u64 = 0x00ff_ffff_ffff_ffff;
const PEXT_LEN_MASK: u64 = 0x0fff_ffff_ffff_ffff;
const APFS_KIND_NEW: u64 = 1;
/// Mismatches listed in full; the rest are counted.
const SHOWN: usize = 10;

struct Reader<'a>(&'a Container);

impl NodeSource for Reader<'_> {
    fn read_node(&self, paddr: u64) -> apfs::Result<Arc<Node>> {
        self.0.node(paddr)
    }
}

/// One extent-reference record: `len` blocks from `start`, used `count` times.
struct Ref {
    start: u64,
    len: u64,
    owner: u64,
    count: i64,
    kind: u64,
}

fn u64_at(b: &[u8], o: usize) -> u64 {
    b.get(o..o + 8).map_or(0, |s| u64::from_le_bytes(s.try_into().unwrap()))
}

/// Checks every volume of `c`; returns the number of problems found.
pub fn extent_check_container(c: &Container, label: &str) -> CliResult<usize> {
    let mut problems = 0;
    for i in 0..c.sb.fs_oids.len() {
        problems += check_volume(c, &c.volume(i)?, i, label)?;
    }
    Ok(problems)
}

fn check_volume(c: &Container, v: &Volume, index: usize, label: &str) -> CliResult<usize> {
    let bs = c.block_size() as u64;
    // Every stored file extent: (first block, blocks, data stream).
    let mut uses: Vec<(u64, u64, u64)> = Vec::new();
    v.scan_all(&mut |k, val| {
        let h = u64_at(k, 0);
        if h >> 60 == TYPE_FILE_EXTENT {
            let (len, phys) = (u64_at(val, 0) & FILE_EXTENT_LEN_MASK, u64_at(val, 8));
            if phys != 0 && len != 0 {
                uses.push((phys, len.div_ceil(bs), h & OBJ_ID_MASK));
            }
        }
        Ok(true)
    })?;

    let mut refs: Vec<Ref> = Vec::new();
    let root = c.node(v.sb.extentref_tree_oid)?;
    let info = root.info()?;
    btree::scan_with_ghosts(&Reader(c), &root, &info, &|oid| Ok(oid), &mut |k, val| {
        let val = val.unwrap_or_default();
        let lk = u64_at(val, 0);
        refs.push(Ref {
            start: u64_at(k, 0) & OBJ_ID_MASK,
            len: lk & PEXT_LEN_MASK,
            owner: u64_at(val, 8),
            count: val.get(16..20).map_or(0, |b| i32::from_le_bytes(b.try_into().unwrap())) as i64,
            kind: lk >> 60,
        });
        Ok(true)
    })?;
    refs.sort_by_key(|r| r.start);

    let mut problems: Vec<String> = Vec::new();
    for w in refs.windows(2) {
        if w[0].start + w[0].len > w[1].start {
            problems.push(format!("extent references {:#x}+{} and {:#x} overlap", w[0].start, w[0].len, w[1].start));
        }
    }

    // Walk both as step functions of the block address: how many streams
    // use each block, and the count its reference gives it.
    let mut events: Vec<(u64, i64)> = Vec::with_capacity(uses.len() * 2);
    for &(phys, blocks, _) in &uses {
        events.push((phys, 1));
        events.push((phys + blocks, -1));
    }
    let mut points: Vec<u64> = events.iter().map(|e| e.0).collect();
    for r in &refs {
        points.push(r.start);
        points.push(r.start + r.len);
    }
    events.sort_unstable();
    points.sort_unstable();
    points.dedup();
    let overlaps = problems.len();
    let (mut used, mut ei, mut ri, mut mismatches) = (0i64, 0usize, 0usize, 0usize);
    let (mut short_blocks, mut over_blocks, mut missing_blocks) = (0u64, 0u64, 0u64);
    for w in points.windows(2) {
        let (a, b) = (w[0], w[1]);
        while ei < events.len() && events[ei].0 <= a {
            used += events[ei].1;
            ei += 1;
        }
        while ri < refs.len() && refs[ri].start + refs[ri].len <= a {
            ri += 1;
        }
        let count = refs.get(ri).filter(|r| r.start <= a).map_or(0, |r| r.count);
        if used == count {
            continue;
        }
        let n = b - a;
        if count == 0 {
            missing_blocks += n;
        } else if count < used {
            short_blocks += n;
        } else {
            over_blocks += n;
        }
        mismatches += 1;
        if problems.len() < SHOWN {
            problems.push(format!("blocks {a:#x}+{n}: {used} streams use them, their reference counts {count}"));
        }
    }

    let streams: std::collections::HashSet<u64> = uses.iter().map(|u| u.2).collect();
    let shared = refs.iter().filter(|r| r.count > 1).count();
    let orphan_owner = refs.iter().filter(|r| !streams.contains(&r.owner)).count();
    let not_new = refs.iter().filter(|r| r.kind != APFS_KIND_NEW).count();
    println!(
        "extents    {label}  volume {index} \"{}\": {} references ({shared} shared), {} file extents; {orphan_owner} owned by a stream that no longer has data, {not_new} not of kind new",
        v.sb.name,
        refs.len(),
        uses.len()
    );
    let found = overlaps + mismatches;
    if found == 0 {
        println!("extents    consistent: every block's count matches the streams using it");
    } else {
        println!(
            "extents    {found} problems: blocks used but without a reference {missing_blocks}, counted too low (data-loss risk) {short_blocks}, counted too high (leak) {over_blocks}"
        );
        for p in problems.iter().take(SHOWN) {
            println!("  {p}");
        }
    }
    Ok(found)
}
