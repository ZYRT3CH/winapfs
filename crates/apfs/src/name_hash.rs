//! The 22-bit hash stored with each directory entry name
//! (`j_drec_hashed_key_t::name_len_and_hash`). Per the APFS reference: NFD,
//! as UTF-32, CRC-32C, complemented, low 22 bits. Case-insensitive volumes
//! hash the case-folded name.

use unicode_normalization::UnicodeNormalization;

/// CRC-32C (Castagnoli), reflected polynomial 0x82F63B78, without the final
/// complement (so callers can follow the reference literally).
fn crc32c_raw(mut crc: u32, data: &[u8]) -> u32 {
    static TABLE: std::sync::OnceLock<[u32; 256]> = std::sync::OnceLock::new();
    let table = TABLE.get_or_init(|| {
        let mut t = [0u32; 256];
        for (i, e) in t.iter_mut().enumerate() {
            let mut c = i as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0x82F6_3B78 ^ (c >> 1) } else { c >> 1 };
            }
            *e = c;
        }
        t
    });
    for &b in data {
        crc = table[((crc ^ b as u32) & 0xff) as usize] ^ (crc >> 8);
    }
    crc
}

/// Folds case the way a case-insensitive volume compares names.
pub fn fold_case(name: &str) -> String {
    name.chars().flat_map(char::to_lowercase).collect()
}

/// The name hash for a directory entry.
pub fn name_hash(name: &str, case_insensitive: bool) -> u32 {
    let folded;
    let name = if case_insensitive {
        folded = fold_case(name);
        &folded
    } else {
        name
    };
    let mut utf32 = Vec::with_capacity(name.len() * 4);
    for c in name.nfd() {
        utf32.extend((c as u32).to_le_bytes());
    }
    // A standard CRC-32C starts at !0 and complements at the end; the
    // reference then complements again, so the two cancel.
    crc32c_raw(!0, &utf32) & 0x3f_ffff
}

/// `name_len_and_hash` for a name: length including the NUL, and the hash.
pub fn name_len_and_hash(name: &str, case_insensitive: bool) -> u32 {
    ((name.len() as u32 + 1) & 0x3ff) | (name_hash(name, case_insensitive) << 10)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32c_check_value() {
        // Standard CRC-32C check value for "123456789".
        assert_eq!(!crc32c_raw(!0, b"123456789"), 0xE306_9283);
    }
}
