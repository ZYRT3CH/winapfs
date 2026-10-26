//! Fletcher-64 object checksum (`o_cksum`), as defined in the APFS reference:
//! computed over the whole object except its first 8 bytes, reading the data
//! as little-endian 32-bit words, modulo 2^32 - 1.

const MOD: u64 = 0xFFFF_FFFF;

fn fletcher64(data: &[u8]) -> (u64, u64) {
    let (mut sum1, mut sum2) = (0u64, 0u64);
    // Reduce every 1024 words so the running sums cannot overflow u64.
    for chunk in data.chunks(4096) {
        for w in chunk.chunks_exact(4) {
            sum1 += u32::from_le_bytes(w.try_into().unwrap()) as u64;
            sum2 += sum1;
        }
        sum1 %= MOD;
        sum2 %= MOD;
    }
    (sum1, sum2)
}

/// Checksum to store in the first 8 bytes of `object`.
pub fn compute(object: &[u8]) -> u64 {
    let (sum1, sum2) = fletcher64(&object[8..]);
    let c1 = MOD - (sum1 + sum2) % MOD;
    let c2 = MOD - (sum1 + c1) % MOD;
    (c2 << 32) | c1
}

pub fn verify(object: &[u8]) -> bool {
    object.len() >= 32 && object.len() % 4 == 0 && stored(object) == compute(object)
}

pub fn stored(object: &[u8]) -> u64 {
    u64::from_le_bytes(object[0..8].try_into().unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let mut block = vec![0u8; 4096];
        for (i, b) in block.iter_mut().enumerate().skip(8) {
            *b = (i * 7 + 3) as u8;
        }
        let c = compute(&block);
        block[0..8].copy_from_slice(&c.to_le_bytes());
        assert!(verify(&block));
        block[100] ^= 1;
        assert!(!verify(&block));
    }

    #[test]
    fn zeroed_block_is_invalid() {
        assert!(!verify(&[0u8; 4096]));
    }
}
