//! `winapfs dev-link-check <source>`: checks that every file's names agree
//! with its records, hard links included. Written against the APFS
//! reference independently of the write code, so it can check that code's
//! results (rehearsals run it before and after).
//!
//! A file with several names (hard links) has, per name, a directory entry
//! carrying a sibling id, a sibling-link record under the inode (the name's
//! folder and name) and a sibling-map record (sibling id to inode). The
//! inode's link count is its number of names, and its own folder and name
//! are those of one of them, its primary link.

use std::collections::{HashMap, HashSet};

use apfs::{Container, Volume};

use crate::CliResult;

const OBJ_ID_MASK: u64 = 0x0fff_ffff_ffff_ffff;
const TYPE_INODE: u64 = 3;
const TYPE_SIBLING_LINK: u64 = 5;
const TYPE_DIR_REC: u64 = 9;
const TYPE_SIBLING_MAP: u64 = 12;
const INO_EXT_TYPE_NAME: u8 = 4;
const DREC_EXT_TYPE_SIBLING_ID: u8 = 1;
const S_IFMT: u16 = 0o170000;
const S_IFDIR: u16 = 0o040000;
/// Problems listed in full; the rest are counted.
const SHOWN: usize = 10;

struct InodeInfo {
    parent: u64,
    /// Link count for files, child count for folders.
    count: i32,
    dir: bool,
    name: Option<String>,
}

struct Drec {
    parent: u64,
    name: String,
    file_id: u64,
    sibling: Option<u64>,
}

struct SiblingLink {
    inode: u64,
    sibling: u64,
    parent: u64,
    name: String,
}

fn u16_at(b: &[u8], o: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(o..o + 2)?.try_into().ok()?))
}

fn u32_at(b: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(o..o + 4)?.try_into().ok()?))
}

fn u64_at(b: &[u8], o: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(o..o + 8)?.try_into().ok()?))
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b.strip_suffix(&[0]).unwrap_or(b)).into_owned()
}

/// The extended fields (`xf_blob_t`) at the start of `blob`: (type, data).
fn xfields(blob: &[u8]) -> Vec<(u8, &[u8])> {
    let Some(n) = u16_at(blob, 0) else { return Vec::new() };
    let mut out = Vec::new();
    let mut data = 4 + 4 * n as usize;
    for i in 0..n as usize {
        let (Some(&kind), Some(size)) = (blob.get(4 + 4 * i), u16_at(blob, 6 + 4 * i)) else { break };
        let Some(d) = blob.get(data..data + size as usize) else { break };
        out.push((kind, d));
        data += (size as usize).div_ceil(8) * 8;
    }
    out
}

/// A directory entry key's name: hashed (`j_drec_hashed_key_t`) or not.
fn drec_name(k: &[u8]) -> String {
    match u32_at(k, 8) {
        Some(w) if (w & 0x3ff) as usize == k.len().saturating_sub(12) => text(&k[12..]),
        _ => text(k.get(10..).unwrap_or_default()),
    }
}

/// Checks every volume of `c`; returns the number of problems found.
pub fn link_check_container(c: &Container, label: &str) -> CliResult<usize> {
    let mut problems = 0;
    for i in 0..c.sb.fs_oids.len() {
        problems += check_volume(&c.volume(i)?, i, label)?;
    }
    Ok(problems)
}

fn check_volume(v: &Volume, index: usize, label: &str) -> CliResult<usize> {
    let mut inodes: HashMap<u64, InodeInfo> = HashMap::new();
    let mut drecs: Vec<Drec> = Vec::new();
    let mut links: Vec<SiblingLink> = Vec::new();
    let mut maps: HashMap<u64, u64> = HashMap::new();
    v.scan_all(&mut |k, val| {
        let Some(h) = u64_at(k, 0) else { return Ok(true) };
        let id = h & OBJ_ID_MASK;
        match h >> 60 {
            TYPE_INODE => {
                let mode = u16_at(val, 80).unwrap_or(0);
                let name = xfields(val.get(92..).unwrap_or_default()).into_iter().find(|f| f.0 == INO_EXT_TYPE_NAME).map(|f| text(f.1));
                inodes.insert(
                    id,
                    InodeInfo {
                        parent: u64_at(val, 0).unwrap_or(0),
                        count: u32_at(val, 56).unwrap_or(0) as i32,
                        dir: mode & S_IFMT == S_IFDIR,
                        name,
                    },
                );
            }
            TYPE_DIR_REC => {
                let sibling = xfields(val.get(18..).unwrap_or_default())
                    .into_iter()
                    .find(|f| f.0 == DREC_EXT_TYPE_SIBLING_ID)
                    .and_then(|f| u64_at(f.1, 0));
                drecs.push(Drec { parent: id, name: drec_name(k), file_id: u64_at(val, 0).unwrap_or(0), sibling });
            }
            TYPE_SIBLING_LINK => {
                let len = u16_at(val, 8).unwrap_or(0) as usize;
                links.push(SiblingLink {
                    inode: id,
                    sibling: u64_at(k, 8).unwrap_or(0),
                    parent: u64_at(val, 0).unwrap_or(0),
                    name: text(val.get(10..10 + len).unwrap_or_default()),
                });
            }
            TYPE_SIBLING_MAP => {
                maps.insert(id, u64_at(val, 0).unwrap_or(0));
            }
            _ => {}
        }
        Ok(true)
    })?;

    let mut problems: Vec<String> = Vec::new();
    let key = |name: &str| v.name_key(name);
    let mut names_of: HashMap<u64, i64> = HashMap::new();
    let mut children_of: HashMap<u64, i64> = HashMap::new();
    let mut entry_at: HashMap<(u64, String), (u64, Option<u64>)> = HashMap::new();
    for d in &drecs {
        *names_of.entry(d.file_id).or_default() += 1;
        *children_of.entry(d.parent).or_default() += 1;
        entry_at.insert((d.parent, key(&d.name)), (d.file_id, d.sibling));
        if !inodes.contains_key(&d.file_id) {
            problems.push(format!("entry {:?} in folder {} names inode {}, which does not exist", d.name, d.parent, d.file_id));
        }
    }
    for (&id, ino) in &inodes {
        let (found, what) = if ino.dir { (children_of.get(&id), "entries") } else { (names_of.get(&id), "names") };
        let found = found.copied().unwrap_or(0);
        if found != ino.count as i64 {
            problems.push(format!("inode {id} ({:?}): count {} but {found} {what}", ino.name, ino.count));
        }
    }

    let mut links_of: HashMap<u64, Vec<&SiblingLink>> = HashMap::new();
    let mut link_ids: HashSet<(u64, u64)> = HashSet::new();
    for l in &links {
        links_of.entry(l.inode).or_default().push(l);
        link_ids.insert((l.inode, l.sibling));
        if maps.get(&l.sibling) != Some(&l.inode) {
            problems.push(format!("sibling {} of inode {} ({:?}) has no sibling map pointing back", l.sibling, l.inode, l.name));
        }
        match entry_at.get(&(l.parent, key(&l.name))) {
            Some(&(file, sibling)) if file == l.inode && sibling.is_none_or(|s| s == l.sibling) => {}
            Some(&(file, sibling)) => problems.push(format!(
                "sibling {} of inode {} ({:?} in folder {}): the entry there names inode {file}, sibling {sibling:?}",
                l.sibling, l.inode, l.name, l.parent
            )),
            None => problems.push(format!("sibling {} of inode {} ({:?} in folder {}): no such entry", l.sibling, l.inode, l.name, l.parent)),
        }
    }
    for (&sibling, &inode) in &maps {
        if !link_ids.contains(&(inode, sibling)) {
            problems.push(format!("sibling map {sibling} points at inode {inode}, which has no such sibling link"));
        }
    }
    for d in &drecs {
        if let Some(s) = d.sibling {
            if !link_ids.contains(&(d.file_id, s)) {
                problems.push(format!("entry {:?} in folder {} carries sibling {s}, which inode {} does not have", d.name, d.parent, d.file_id));
            }
        }
    }
    let (mut several, mut lowest_primary) = (0, 0);
    for (&id, ls) in &links_of {
        let Some(ino) = inodes.get(&id) else {
            problems.push(format!("sibling links under inode {id}, which does not exist"));
            continue;
        };
        if ls.len() as i64 != ino.count as i64 {
            problems.push(format!("inode {id} ({:?}): link count {} but {} sibling links", ino.name, ino.count, ls.len()));
        }
        let primary = ls.iter().find(|l| l.parent == ino.parent && ino.name.as_deref().is_none_or(|n| key(n) == key(&l.name)));
        match primary {
            None => problems.push(format!("inode {id} ({:?} in folder {}) matches none of its sibling links", ino.name, ino.parent)),
            Some(p) if ls.len() > 1 => {
                several += 1;
                if ls.iter().all(|l| l.sibling >= p.sibling) {
                    lowest_primary += 1;
                }
            }
            Some(_) => {}
        }
    }

    println!(
        "links      {label}  volume {index} \"{}\": {} files with sibling records, {several} with several names (primary is the lowest sibling id for {lowest_primary})",
        v.sb.name,
        links_of.len()
    );
    if problems.is_empty() {
        println!("links      consistent: every name matches its records");
    } else {
        println!("links      {} problems", problems.len());
        for p in problems.iter().take(SHOWN) {
            println!("  {p}");
        }
    }
    Ok(problems.len())
}
