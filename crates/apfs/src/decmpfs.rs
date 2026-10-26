//! Transparent file compression (`com.apple.decmpfs`). The attribute starts
//! with a 16-byte header; small files keep their data inline after it, larger
//! ones in the `com.apple.ResourceFork` attribute as 64 KiB chunks.

use std::io::Write;

use crate::bytes::{slice_at, u32_at, u64_at};
use crate::error::{corrupt, unsupported, Result};

pub const DECMPFS_XATTR: &str = "com.apple.decmpfs";
pub const RESOURCE_FORK_XATTR: &str = "com.apple.ResourceFork";

const MAGIC: u32 = 0x636d_7066; // "fpmc" as a little-endian u32
const CHUNK: u64 = 65536;

const ZLIB_INLINE: u32 = 3;
const ZLIB_RSRC: u32 = 4;
const LZVN_INLINE: u32 = 7;
const LZVN_RSRC: u32 = 8;
const LZFSE_INLINE: u32 = 11;
const LZFSE_RSRC: u32 = 12;

#[derive(Debug, Clone, Copy)]
pub struct Header {
    pub compression_type: u32,
    pub size: u64,
}

pub fn header(x: &[u8]) -> Result<Header> {
    if u32_at(x, 0)? != MAGIC {
        return Err(corrupt("decmpfs attribute without fpmc magic"));
    }
    Ok(Header { compression_type: u32_at(x, 4)?, size: u64_at(x, 8)? })
}

#[derive(Clone, Copy)]
enum Codec {
    Zlib,
    Lzvn,
    Lzfse,
}

/// Decompresses into `w`. `rsrc` fetches the resource fork when the data lives there.
pub fn decompress(x: &[u8], rsrc: &mut dyn FnMut() -> Result<Vec<u8>>, w: &mut dyn Write) -> Result<u64> {
    let h = header(x)?;
    let inline = &x[16..];
    match h.compression_type {
        ZLIB_INLINE => w.write_all(&chunk(Codec::Zlib, inline, h.size)?)?,
        LZVN_INLINE => w.write_all(&chunk(Codec::Lzvn, inline, h.size)?)?,
        LZFSE_INLINE => w.write_all(&chunk(Codec::Lzfse, inline, h.size)?)?,
        ZLIB_RSRC => zlib_resource_fork(&rsrc()?, h.size, w)?,
        LZVN_RSRC => chunked_resource_fork(Codec::Lzvn, &rsrc()?, h.size, w)?,
        LZFSE_RSRC => chunked_resource_fork(Codec::Lzfse, &rsrc()?, h.size, w)?,
        t => return Err(unsupported(format!("decmpfs compression type {t}"))),
    }
    Ok(h.size)
}

/// zlib chunks live inside a classic resource fork: a big-endian header
/// points at the resource data, which holds a little-endian chunk table.
fn zlib_resource_fork(r: &[u8], size: u64, w: &mut dyn Write) -> Result<()> {
    let data_off = u32::from_be_bytes(slice_at(r, 0, 4)?.try_into().unwrap()) as usize;
    let base = data_off + 4; // skip the resource's own length field
    let count = u32_at(r, base)? as u64;
    if count != size.div_ceil(CHUNK) {
        return Err(corrupt(format!("{count} zlib chunks for a {size}-byte file")));
    }
    for i in 0..count as usize {
        let off = u32_at(r, base + 4 + i * 8)? as usize;
        let len = u32_at(r, base + 8 + i * 8)? as usize;
        let expected = (size - i as u64 * CHUNK).min(CHUNK);
        w.write_all(&chunk(Codec::Zlib, slice_at(r, base + off, len)?, expected)?)?;
    }
    Ok(())
}

/// LZVN and LZFSE resource forks start with a table of chunk offsets.
fn chunked_resource_fork(codec: Codec, r: &[u8], size: u64, w: &mut dyn Write) -> Result<()> {
    let n = size.div_ceil(CHUNK) as usize;
    for i in 0..n {
        let start = u32_at(r, i * 4)? as usize;
        let end = u32_at(r, (i + 1) * 4)? as usize;
        if end < start {
            return Err(corrupt("resource fork chunk table out of order"));
        }
        let expected = (size - i as u64 * CHUNK).min(CHUNK);
        w.write_all(&chunk(codec, slice_at(r, start, end - start)?, expected)?)?;
    }
    Ok(())
}

/// Decodes one chunk that must expand to exactly `expected` bytes. A chunk
/// that did not compress is stored raw behind a one-byte marker.
fn chunk(codec: Codec, data: &[u8], expected: u64) -> Result<Vec<u8>> {
    let expected = usize::try_from(expected).map_err(|_| corrupt("chunk size overflow"))?;
    let first = *data.first().ok_or_else(|| corrupt("empty compressed chunk"))?;
    let raw_marker = match codec {
        Codec::Zlib => first & 0x0f == 0x0f,
        Codec::Lzvn => first == 0x06,
        Codec::Lzfse => !data.starts_with(b"bvx"),
    };
    let out = if raw_marker && data.len() == expected + 1 {
        data[1..].to_vec()
    } else {
        match codec {
            Codec::Zlib => miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(data, expected)
                .map_err(|e| corrupt(format!("zlib: {e:?}")))?,
            Codec::Lzvn => lzfse(&lzvn_frame(data, expected)?, expected)?,
            Codec::Lzfse => lzfse(data, expected)?,
        }
    };
    if out.len() != expected {
        return Err(corrupt(format!("chunk expanded to {} bytes, expected {expected}", out.len())));
    }
    Ok(out)
}

fn lzfse(frame: &[u8], expected: usize) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(expected);
    lzfse_rust::decode_bytes(frame, &mut out).map_err(|e| corrupt(format!("lzfse: {e}")))?;
    Ok(out)
}

/// decmpfs stores bare LZVN streams; wrap one in an LZFSE `bvxn` block so the
/// LZFSE decoder (which contains an LZVN decoder) can read it.
fn lzvn_frame(payload: &[u8], raw_len: usize) -> Result<Vec<u8>> {
    let raw = u32::try_from(raw_len).map_err(|_| corrupt("LZVN chunk too large"))?;
    let n = u32::try_from(payload.len()).map_err(|_| corrupt("LZVN chunk too large"))?;
    let mut f = Vec::with_capacity(payload.len() + 16);
    f.extend_from_slice(b"bvxn");
    f.extend_from_slice(&raw.to_le_bytes());
    f.extend_from_slice(&n.to_le_bytes());
    f.extend_from_slice(payload);
    f.extend_from_slice(b"bvx$");
    Ok(f)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inline_attr(ctype: u32, size: u64, payload: &[u8]) -> Vec<u8> {
        let mut x = Vec::new();
        x.extend_from_slice(&MAGIC.to_le_bytes());
        x.extend_from_slice(&ctype.to_le_bytes());
        x.extend_from_slice(&size.to_le_bytes());
        x.extend_from_slice(payload);
        x
    }

    fn run(x: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        decompress(x, &mut || panic!("no resource fork expected"), &mut out).unwrap();
        out
    }

    #[test]
    fn zlib_inline() {
        let text = b"hello hello hello hello APFS".repeat(20);
        let z = miniz_oxide::deflate::compress_to_vec_zlib(&text, 6);
        assert_eq!(run(&inline_attr(ZLIB_INLINE, text.len() as u64, &z)), text);
    }

    #[test]
    fn zlib_inline_raw_marker() {
        let mut payload = vec![0xff];
        payload.extend_from_slice(b"abc");
        assert_eq!(run(&inline_attr(ZLIB_INLINE, 3, &payload)), b"abc");
    }

    #[test]
    fn lzvn_inline() {
        let text = b"lzvn lzvn lzvn lzvn round trip ".repeat(8);
        let mut frame = Vec::new();
        lzfse_rust::encode_bytes(&text, &mut frame).unwrap();
        // Small inputs encode as a single bvxn block: header(12) + payload + "bvx$".
        assert_eq!(&frame[0..4], b"bvxn", "encoder chose a different block type");
        let payload_len = u32::from_le_bytes(frame[8..12].try_into().unwrap()) as usize;
        let payload = &frame[12..12 + payload_len];
        assert_eq!(run(&inline_attr(LZVN_INLINE, text.len() as u64, payload)), text);
    }

    #[test]
    fn lzfse_resource_fork() {
        let data: Vec<u8> = (0..150_000u32).map(|i| (i % 251) as u8).collect();
        let mut fork = Vec::new();
        let chunks: Vec<Vec<u8>> = data
            .chunks(CHUNK as usize)
            .map(|c| {
                let mut f = Vec::new();
                lzfse_rust::encode_bytes(c, &mut f).unwrap();
                f
            })
            .collect();
        let mut off = 4 * (chunks.len() as u32 + 1);
        for c in &chunks {
            fork.extend_from_slice(&off.to_le_bytes());
            off += c.len() as u32;
        }
        fork.extend_from_slice(&off.to_le_bytes());
        chunks.iter().for_each(|c| fork.extend_from_slice(c));

        let x = inline_attr(LZFSE_RSRC, data.len() as u64, &[]);
        let mut out = Vec::new();
        decompress(&x, &mut || Ok(fork.clone()), &mut out).unwrap();
        assert_eq!(out, data);
    }
}
