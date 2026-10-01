//! Host-side math for the crypto accelerators (shared by all ESP32 SoCs).

use aes::cipher::{BlockDecrypt, BlockEncrypt, KeyInit, generic_array::GenericArray};
use num_bigint::BigUint;
use num_traits::{One, Zero};

/// Run an RSA/MPI accelerator operation on its memory blocks.
/// `mem` holds 0x800 bytes as words: M at 0x000, Z (also Rinv) at 0x200, Y at 0x400, X at 0x600.
/// `op` is the start register offset: 0x80C modexp, 0x810 modmult, 0x814 mult; `len` in words.
pub fn rsa_op(op: u32, len: usize, mem: &mut [u32; 0x200]) {
    let num = |mem: &[u32; 0x200], base: usize, words: usize| BigUint::from_slice(&mem[base / 4..base / 4 + words]);
    let store = |mem: &mut [u32; 0x200], base: usize, n: &BigUint, words: usize| {
        let d = n.to_u32_digits();
        for i in 0..words {
            mem[base / 4 + i] = *d.get(i).unwrap_or(&0);
        }
    };
    let (m, z, y, x) = (0x000, 0x200, 0x400, 0x600);
    let len = len.min(128);
    match op {
        0x80C => {
            let (xv, yv, mv) = (num(mem, x, len), num(mem, y, len), num(mem, m, len));
            let r = if mv.is_zero() { BigUint::zero() } else { xv.modpow(&yv, &mv) };
            store(mem, z, &r, len);
        }
        0x810 => {
            // Montgomery: Z = X * Y * Rinv * R^-2 mod M, R = 2^(32*len).
            let (xv, yv, mv, rinv) = (num(mem, x, len), num(mem, y, len), num(mem, m, len), num(mem, z, len));
            let r = if mv.is_zero() {
                BigUint::zero()
            } else {
                let rr = (BigUint::one() << (32 * len)) % &mv;
                match mod_inverse(&rr, &mv) {
                    Some(ri) => (xv * yv % &mv) * rinv % &mv * (&ri * &ri % &mv) % &mv,
                    None => BigUint::zero(),
                }
            };
            store(mem, z, &r, len);
        }
        0x814 => {
            // Operands are len/2 words: X in the X block, Y in the upper half of Z.
            let half = len / 2;
            let xv = num(mem, x, half);
            let yv = num(mem, z + 4 * half, half);
            store(mem, z, &(xv * yv), len);
        }
        _ => {}
    }
}

pub fn mod_inverse(a: &BigUint, m: &BigUint) -> Option<BigUint> {
    use num_bigint::BigInt;
    let (mut t, mut newt) = (BigInt::zero(), BigInt::one());
    let (mut r, mut newr) = (BigInt::from(m.clone()), BigInt::from(a.clone()));
    while !newr.is_zero() {
        let q = &r / &newr;
        (t, newt) = (newt.clone(), t - &q * newt);
        (r, newr) = (newr.clone(), r - &q * newr);
    }
    if r != BigInt::one() {
        return None;
    }
    let m = BigInt::from(m.clone());
    ((t % &m + &m) % &m).to_biguint()
}

pub fn aes_block(key: &[u8], decrypt: bool, block: &mut [u8]) {
    let b = GenericArray::from_mut_slice(block);
    match key.len() {
        32 => {
            let c = aes::Aes256::new_from_slice(key).unwrap();
            if decrypt { c.decrypt_block(b) } else { c.encrypt_block(b) }
        }
        _ => {
            let c = aes::Aes128::new_from_slice(key).unwrap();
            if decrypt { c.decrypt_block(b) } else { c.encrypt_block(b) }
        }
    }
}

fn inc_counter(iv: &mut [u8; 16], inc32: bool) {
    let n = if inc32 { 4 } else { 16 };
    for i in (16 - n..16).rev() {
        iv[i] = iv[i].wrapping_add(1);
        if iv[i] != 0 {
            break;
        }
    }
}

/// Block cipher modes as the C3 AES DMA engine implements them.
pub fn aes_blocks(key: &[u8], decrypt: bool, mode: u32, inc32: bool, iv: &mut [u8; 16], data: &mut [u8]) {
    for blk in data.as_chunks_mut::<16>().0 {
        match mode {
            0 => aes_block(key, decrypt, blk),
            1 => {
                // CBC
                if decrypt {
                    let c = *blk;
                    aes_block(key, true, blk);
                    blk.iter_mut().zip(iv.iter()).for_each(|(b, v)| *b ^= v);
                    *iv = c;
                } else {
                    blk.iter_mut().zip(iv.iter()).for_each(|(b, v)| *b ^= v);
                    aes_block(key, false, blk);
                    iv.copy_from_slice(blk);
                }
            }
            2 => {
                // OFB
                aes_block(key, false, iv);
                blk.iter_mut().zip(iv.iter()).for_each(|(b, v)| *b ^= v);
            }
            3 => {
                // CTR
                let mut ks = *iv;
                aes_block(key, false, &mut ks);
                blk.iter_mut().zip(ks.iter()).for_each(|(b, v)| *b ^= v);
                inc_counter(iv, inc32);
            }
            5 => {
                // CFB128
                let mut ks = *iv;
                aes_block(key, false, &mut ks);
                let c: [u8; 16] = if decrypt { *blk } else { [0; 16] };
                blk.iter_mut().zip(ks.iter()).for_each(|(b, v)| *b ^= v);
                *iv = if decrypt { c } else { *blk };
            }
            4 => {
                // CFB8
                for b in blk.iter_mut() {
                    let mut ks = *iv;
                    aes_block(key, false, &mut ks);
                    let c = if decrypt { *b } else { *b ^ ks[0] };
                    *b ^= ks[0];
                    iv.copy_within(1.., 0);
                    iv[15] = c;
                }
            }
            _ => log::warn!("AES block mode {mode} not supported"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aes128_ecb_known_answer() {
        // FIPS-197 C.1
        let key: Vec<u8> = (0..16).collect();
        let mut b: Vec<u8> = (0..16).map(|i| i * 0x11).collect();
        aes_block(&key, false, &mut b);
        assert_eq!(b, hex("69c4e0d86a7b0430d8cdb78070b4c55a"));
        aes_block(&key, true, &mut b);
        assert_eq!(b, (0..16).map(|i| i * 0x11).collect::<Vec<u8>>());
    }

    #[test]
    fn cbc_roundtrip() {
        let key = [7u8; 16];
        let mut iv = [3u8; 16];
        let plain: Vec<u8> = (0..64).collect();
        let mut d = plain.clone();
        aes_blocks(&key, false, 1, true, &mut iv, &mut d);
        let mut iv2 = [3u8; 16];
        aes_blocks(&key, true, 1, true, &mut iv2, &mut d);
        assert_eq!(d, plain);
        assert_eq!(iv, iv2);
    }

    #[test]
    fn montgomery_modmult_identity() {
        // With Rinv = R^2 mod M the hardware yields X*Y mod M.
        let m = BigUint::from(0xffff_fffbu32) * BigUint::from(0xffff_ffefu64);
        let len = 2;
        let r = (BigUint::one() << (32 * len)) % &m;
        let rinv = &r * &r % &m;
        let (x, y) = (BigUint::from(123456789u64), BigUint::from(987654321u64));
        let ri = mod_inverse(&r, &m).unwrap();
        let z = (&x * &y % &m) * rinv % &m * (&ri * &ri % &m) % &m;
        assert_eq!(z, &x * &y % &m);
    }

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }
}
