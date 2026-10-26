//! `winapfs dump <source>`: prints the structures a write has to change, for
//! developing and debugging write support. Comparing dumps of the same image
//! before and after a change by a known-good implementation shows the rules
//! the APFS reference leaves out.

use apfs::btree::{self, NodeSource};
use apfs::obj::ObjHeader;
use apfs::spaceman::{self, Spaceman};
use apfs::{checksum, format_uuid, Container};

use crate::{open_source, CliResult};

pub fn dump(src: &str, part: Option<u32>) -> CliResult {
    let (c, location) = open_source(src, part)?;
    let sb = &c.sb;
    println!("== container  {location}");
    println!(
        "block_size {}  block_count {}  xid {}  next_xid {}  next_oid {:#x}  uuid {}",
        sb.block_size,
        sb.block_count,
        sb.hdr.xid,
        sb.next_xid,
        sb.next_oid,
        format_uuid(&sb.uuid)
    );
    println!(
        "features {:#x}  ro_compat {:#x}  incompat {:#x}  flags {:#x}  max_fs {}  eph_info {:#x?}",
        sb.features, sb.ro_compat_features, sb.incompat_features, sb.flags, sb.max_file_systems, sb.ephemeral_info
    );
    println!(
        "xp_desc  base {:#x} blocks {} index {} len {} next {}",
        sb.xp_desc_base, sb.xp_desc_blocks, sb.xp_desc_index, sb.xp_desc_len, sb.xp_desc_next
    );
    println!(
        "xp_data  base {:#x} blocks {} index {} len {} next {}",
        sb.xp_data_base, sb.xp_data_blocks, sb.xp_data_index, sb.xp_data_len, sb.xp_data_next
    );
    println!(
        "spaceman_oid {:#x}  omap_oid {:#x}  reaper_oid {:#x}  fs_oids {:x?}",
        sb.spaceman_oid, sb.omap_oid, sb.reaper_oid, sb.fs_oids
    );
    let b0 = c.raw_block(0)?;
    println!("block 0: xid {}  checksum {}", ObjHeader::parse(&b0)?.xid, ok(checksum::verify(&b0)));

    println!("\n== checkpoint descriptor ring");
    ring(&c, sb.xp_desc_base, sb.xp_desc_blocks)?;
    println!("\n== checkpoint data ring");
    ring(&c, sb.xp_data_base, sb.xp_data_blocks)?;
    println!("\n== checkpoint map (xid {})", sb.hdr.xid);
    for m in &c.checkpoint_map {
        println!(
            "  type {:#010x} subtype {:#x} size {} fs_oid {:#x} oid {:#x} -> {:#x}",
            m.obj_type, m.subtype, m.size, m.fs_oid, m.oid, m.paddr
        );
    }

    let sm = c.space_manager()?;
    dump_spaceman(&c, &sm)?;
    dump_free_queues(&c, &sm)?;

    println!("\n== reaper (oid {:#x})", sb.reaper_oid);
    let reaper = c.ephemeral(sb.reaper_oid)?;
    println!("  {} bytes; first 96 after the header: {}", reaper.len(), hex(&reaper[32..128.min(reaper.len())]));

    println!("\n== container object map");
    for (oid, xid, v) in c.omap_entries()? {
        println!("  oid {oid:#x} xid {xid} -> {:#x} (flags {:#x}, size {})", v.paddr, v.flags, v.size);
    }

    for (i, vsb) in c.volume_superblocks()?.iter().enumerate() {
        println!("\n== volume {i} \"{}\"", vsb.name);
        println!(
            "  xid {}  oid {:#x}  fs_index {}  features {:#x} ro_compat {:#x} incompat {:#x}  fs_flags {:#x}  role {:#x}",
            vsb.hdr.xid, vsb.hdr.oid, vsb.fs_index, vsb.features, vsb.ro_compat_features, vsb.incompat_features, vsb.fs_flags, vsb.role
        );
        println!(
            "  omap_oid {:#x}  root_tree_oid {:#x}  extentref_tree_oid {:#x}  snap_meta_tree_oid {:#x}  next_obj_id {:#x}",
            vsb.omap_oid, vsb.root_tree_oid, vsb.extentref_tree_oid, vsb.snap_meta_tree_oid, vsb.next_obj_id
        );
        println!(
            "  files {} dirs {} symlinks {} other {} snapshots {}  alloc_blocks {}  reserve {} quota {}",
            vsb.num_files,
            vsb.num_directories,
            vsb.num_symlinks,
            vsb.num_other_fsobjects,
            vsb.num_snapshots,
            vsb.alloc_blocks,
            vsb.reserve_blocks,
            vsb.quota_blocks
        );
        println!("  formatted_by \"{}\"  last_modified_by \"{}\"", vsb.formatted_by, vsb.last_modified_by);
        match c.volume(i) {
            Ok(v) => {
                for (oid, xid, val) in v.omap_entries()? {
                    println!("  omap: oid {oid:#x} xid {xid} -> {:#x} (flags {:#x})", val.paddr, val.flags);
                }
                let root = v.fs_root();
                println!(
                    "  fs tree root: oid {:#x} xid {} level {} keys {}  info {:?}",
                    root.hdr.oid,
                    root.hdr.xid,
                    root.level,
                    root.nkeys,
                    root.info()?
                );
            }
            Err(e) => println!("  (cannot open: {e})"),
        }
    }
    Ok(())
}

fn ok(b: bool) -> &'static str {
    if b { "ok" } else { "BAD" }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect::<Vec<_>>().join("")
}

fn ring(c: &Container, base: u64, blocks: u32) -> CliResult {
    for i in 0..(blocks & 0x7fff_ffff) as u64 {
        let b = c.raw_block(base + i)?;
        if b.iter().all(|&x| x == 0) {
            println!("  [{i:3}] {:#x}  (zero)", base + i);
            continue;
        }
        let h = ObjHeader::parse(&b)?;
        println!(
            "  [{i:3}] {:#x}  type {:#010x} subtype {:#x} oid {:#x} xid {}  checksum {}",
            base + i,
            h.obj_type,
            h.subtype,
            h.oid,
            h.xid,
            ok(checksum::verify(&b))
        );
    }
    Ok(())
}

fn dump_spaceman(c: &Container, sm: &Spaceman) -> CliResult {
    println!("\n== space manager (oid {:#x}, xid {}, {} bytes)", sm.hdr.oid, sm.hdr.xid, sm.raw.len());
    println!(
        "  block_size {} blocks_per_chunk {} chunks_per_cib {} cibs_per_cab {}  flags {:#x} version {} struct_size {}",
        sm.block_size, sm.blocks_per_chunk, sm.chunks_per_cib, sm.cibs_per_cab, sm.flags, sm.version, sm.struct_size
    );
    for (d, dv) in sm.dev.iter().enumerate() {
        println!(
            "  dev[{d}]: blocks {} chunks {} cibs {} cabs {} free {} addr_offset {:#x}  addrs {:x?}",
            dv.block_count,
            dv.chunk_count,
            dv.cib_count,
            dv.cab_count,
            dv.free_count,
            dv.addr_offset,
            sm.dev_addrs(d)?
        );
    }
    println!(
        "  internal pool: base {:#x} blocks {}  bitmap ring base {:#x} blocks {} size_in_blocks {} tx_multiplier {}",
        sm.ip_base, sm.ip_block_count, sm.ip_bm_base, sm.ip_bm_block_count, sm.ip_bm_size_in_blocks, sm.ip_bm_tx_multiplier
    );
    println!(
        "  ip bitmap: free head {} tail {}  xid_offset {:#x} bitmap_offset {:#x} free_next_offset {:#x}",
        sm.ip_bm_free_head, sm.ip_bm_free_tail, sm.ip_bm_xid_offset, sm.ip_bitmap_offset, sm.ip_bm_free_next_offset
    );
    println!("  ip_bm xids {:?}", sm.ip_bm_xids()?);
    println!("  ip_bitmap indices {:?}", sm.ip_bitmap_indices()?);
    println!("  ip_bm free_next {:?}", sm.ip_bm_free_next()?);
    println!("  fs_reserve block_count {} alloc_count {}", sm.fs_reserve_block_count, sm.fs_reserve_alloc_count);
    for (i, q) in sm.fq.iter().enumerate() {
        println!(
            "  fq[{i}]: count {} tree_oid {:#x} oldest_xid {} node_limit {}",
            q.count, q.tree_oid, q.oldest_xid, q.tree_node_limit
        );
    }
    // The internal-pool bitmap: one bit per internal-pool block.
    for (slot, &idx) in sm.ip_bitmap_indices()?.iter().enumerate() {
        let b = c.raw_block(sm.ip_bm_base + idx as u64)?;
        let used: Vec<u64> = (0..sm.ip_block_count).filter(|&i| b[(i / 8) as usize] >> (i % 8) & 1 == 1).collect();
        println!("  ip bitmap slot {slot} (ring block {idx}): used ip blocks {used:?}");
    }
    for d in 0..2 {
        let dv = &sm.dev[d];
        if dv.block_count == 0 {
            continue;
        }
        let mut cibs = sm.dev_addrs(d)?;
        if dv.cab_count > 0 {
            let mut all = Vec::new();
            for cab in cibs {
                let (index, addrs) = spaceman::cib_addrs(&c.raw_block(cab)?)?;
                println!("  cab {cab:#x} index {index}: cibs {addrs:x?}");
                all.extend(addrs);
            }
            cibs = all;
        }
        for cib in cibs {
            let b = c.raw_block(cib)?;
            let (index, chunks) = spaceman::chunk_infos(&b)?;
            println!("  cib {cib:#x} index {index} xid {} checksum {}: {} chunks", ObjHeader::parse(&b)?.xid, ok(checksum::verify(&b)), chunks.len());
            for ch in chunks.iter().filter(|ch| ch.free_count != ch.block_count || ch.bitmap_addr != 0) {
                println!(
                    "    chunk addr {:#x} blocks {} free {} bitmap {:#x} xid {}",
                    ch.addr, ch.block_count, ch.free_count, ch.bitmap_addr, ch.xid
                );
                if ch.bitmap_addr != 0 {
                    let bm = c.raw_block(ch.bitmap_addr)?;
                    let used = runs((0..ch.block_count as u64).filter(|&i| bm[(i / 8) as usize] >> (i % 8) & 1 == 1));
                    println!("      used blocks (relative): {used}");
                }
            }
        }
    }
    Ok(())
}

/// Formats ascending numbers as compact ranges: `0-5,9,12-13`.
fn runs(it: impl Iterator<Item = u64>) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut cur: Option<(u64, u64)> = None;
    for n in it {
        cur = match cur {
            Some((a, b)) if n == b + 1 => Some((a, n)),
            Some((a, b)) => {
                out.push(if a == b { a.to_string() } else { format!("{a}-{b}") });
                Some((n, n))
            }
            None => Some((n, n)),
        };
    }
    if let Some((a, b)) = cur {
        out.push(if a == b { a.to_string() } else { format!("{a}-{b}") });
    }
    if out.is_empty() { "none".into() } else { out.join(",") }
}

fn dump_free_queues(c: &Container, sm: &Spaceman) -> CliResult {
    for (i, q) in sm.fq.iter().enumerate() {
        if q.tree_oid == 0 {
            continue;
        }
        println!("\n== free queue {i} (tree oid {:#x})", q.tree_oid);
        let Some(root_paddr) = c.ephemeral_paddr(q.tree_oid) else {
            println!("  (tree not in checkpoint map)");
            continue;
        };
        let root = c.node(root_paddr)?;
        let info = root.info()?;
        println!("  root at {root_paddr:#x}: level {} keys {}  info {info:?}", root.level, root.nkeys);
        let resolve = |oid: u64| {
            c.ephemeral_paddr(oid).ok_or_else(|| apfs::Error::Corrupt(format!("ephemeral node {oid:#x} not in checkpoint")))
        };
        let src: &dyn NodeSource = &NodeReader(c);
        btree::scan_with_ghosts(src, &root, &info, &resolve, &mut |k, v| {
            let xid = u64::from_le_bytes(k[0..8].try_into().unwrap());
            let paddr = u64::from_le_bytes(k[8..16].try_into().unwrap());
            let count = v.map_or("1 (ghost)".to_string(), |v| u64::from_le_bytes(v[0..8].try_into().unwrap()).to_string());
            println!("  xid {xid} paddr {paddr:#x} count {count}");
            Ok(true)
        })?;
    }
    Ok(())
}

/// `winapfs dev-rebuild-nodes <image>`: rebuilds every root B-tree node in
/// the container from its parsed entries and compares the bytes, to prove the
/// node writer matches existing nodes exactly.
pub fn rebuild_nodes(src: &str) -> CliResult {
    use apfs::btree_write::{build, NodeBuild};
    let (c, _) = open_source(src, None)?;
    let (mut same, mut differ) = (0, 0);
    for paddr in 1..c.sb.block_count {
        let Ok(raw) = c.raw_block(paddr) else { break };
        let Ok(h) = ObjHeader::parse(&raw) else { continue };
        if !matches!(h.kind(), 2 | 3) || !checksum::verify(&raw) {
            continue;
        }
        let node = c.node(paddr)?;
        if !node.is_root() {
            continue; // non-root nodes need their tree's info; covered once trees grow
        }
        let info = node.info()?;
        let entries = node.all_entries(&info)?;
        let fixed = node.is_fixed().then_some((info.key_size as usize, info.val_size as usize));
        let rebuilt = build(
            &NodeBuild {
                oid: h.oid,
                xid: h.xid,
                obj_type: h.obj_type,
                subtype: h.subtype,
                level: node.level,
                root: true,
                fixed,
                info,
                entries: &entries,
            },
            raw.len(),
        )?;
        if rebuilt == raw {
            same += 1;
        } else {
            differ += 1;
            let first = rebuilt.iter().zip(&raw).position(|(a, b)| a != b).unwrap_or(0);
            println!(
                "DIFF  block {paddr:#x} type {:#x} sub {:#x} keys {}: first difference at byte {first} (ours {:02x?}, theirs {:02x?})",
                h.obj_type,
                h.subtype,
                node.nkeys,
                &rebuilt[first..(first + 8).min(raw.len())],
                &raw[first..(first + 8).min(raw.len())]
            );
        }
    }
    println!("rebuilt root nodes: {same} identical, {differ} different");
    Ok(())
}

struct NodeReader<'a>(&'a Container);

impl NodeSource for NodeReader<'_> {
    fn read_node(&self, paddr: u64) -> apfs::Result<std::sync::Arc<btree::Node>> {
        self.0.node(paddr)
    }
}
