//! GDMA-fed crypto accelerators (AES, SHA in DMA mode) and the RSA/MPI engine.
//! The math is done on the host; the models only move data the way the
//! hardware would (descriptor chains, register blocks, status bits).

use aes::cipher::{BlockDecrypt, BlockEncrypt, KeyInit, generic_array::GenericArray};
use num_bigint::BigUint;
use num_traits::{One, Zero};

use super::bus::C3Bus;

pub const GDMA: u32 = 0x6003_F000;
pub const AES: u32 = 0x6003_A000;
pub const RSA: u32 = 0x6003_C000;

const PERI_AES: u32 = 6;
const PERI_SHA: u32 = 7;
const CH_STRIDE: u32 = 0xC0;
const DRAM_HI: u32 = 0x3FC0_0000;

impl C3Bus {
    fn gdma_channel_for(&self, peri: u32, out_dir: bool) -> Option<u32> {
        (0..3).find(|&ch| {
            let sel = if out_dir { 0x100 } else { 0x0A0 };
            self.p.store_get(GDMA + sel + ch * CH_STRIDE) & 0x3f == peri
        })
    }

    /// Collect the bytes described by the out-link (memory -> peripheral) of `ch`.
    fn gdma_gather(&mut self, ch: u32) -> Vec<u8> {
        let link = self.p.store_get(GDMA + 0x0E0 + ch * CH_STRIDE);
        let mut addr = DRAM_HI | (link & 0xfffff);
        let mut out = Vec::new();
        for _ in 0..4096 {
            let Some(w0) = self.peek32(addr) else { break };
            let buf = self.peek32(addr + 4).unwrap_or(0);
            let next = self.peek32(addr + 8).unwrap_or(0);
            let len = ((w0 >> 12) & 0xfff) as usize;
            if let Some(b) = self.peek_bytes(buf, len) {
                out.extend_from_slice(&b);
            }
            // Hand the descriptor back to software.
            self.poke32(addr, w0 & !(1 << 31));
            if w0 & (1 << 30) != 0 || next == 0 {
                break;
            }
            addr = next;
        }
        // OUT_EOF / OUT_TOTAL_EOF / OUT_DONE
        let raw = GDMA + 0x10 * ch;
        let v = self.p.store_get(raw);
        self.p.store_set(raw, v | 1 << 3 | 1 << 4 | 1 << 6);
        out
    }

    /// Write `data` into the buffers of the in-link (peripheral -> memory) of `ch`.
    fn gdma_scatter(&mut self, ch: u32, data: &[u8]) {
        let link = self.p.store_get(GDMA + 0x080 + ch * CH_STRIDE);
        let mut addr = DRAM_HI | (link & 0xfffff);
        let mut pos = 0;
        for _ in 0..4096 {
            let Some(w0) = self.peek32(addr) else { break };
            let buf = self.peek32(addr + 4).unwrap_or(0);
            let next = self.peek32(addr + 8).unwrap_or(0);
            let size = (w0 & 0xfff) as usize;
            let n = size.min(data.len() - pos);
            self.load_bytes(buf, &data[pos..pos + n]);
            pos += n;
            let last = pos >= data.len() || next == 0;
            let mut nw = (w0 & 0xfff) | (n as u32) << 12;
            if last {
                nw |= 1 << 30;
            }
            self.poke32(addr, nw);
            if last {
                break;
            }
            addr = next;
        }
        let raw = GDMA + 0x10 * ch;
        let v = self.p.store_get(raw);
        self.p.store_set(raw, v | 1 << 0 | 1 << 1); // IN_DONE | IN_SUC_EOF
    }

    fn reg_bytes(&self, base: u32, n: usize) -> Vec<u8> {
        (0..n as u32 / 4).flat_map(|i| self.p.store_get(base + 4 * i).to_le_bytes()).collect()
    }

    fn set_reg_bytes(&mut self, base: u32, b: &[u8]) {
        for (i, c) in b.chunks(4).enumerate() {
            let mut w = [0u8; 4];
            w[..c.len()].copy_from_slice(c);
            self.p.store_set(base + 4 * i as u32, u32::from_le_bytes(w));
        }
    }

    // ---- AES ---------------------------------------------------------------------------------

    pub fn aes_trigger(&mut self) {
        let mode = self.p.store_get(AES + 0x40) & 7;
        let key_len = if mode & 2 != 0 { 32 } else { 16 };
        let decrypt = mode & 4 != 0;
        let key = self.reg_bytes(AES, key_len);
        let dma = self.p.store_get(AES + 0x90) & 1 != 0;
        if !dma {
            let mut block = self.reg_bytes(AES + 0x20, 16);
            aes_block(&key, decrypt, &mut block);
            self.set_reg_bytes(AES + 0x30, &block);
            self.p.store_set(AES + 0x4C, 0);
            return;
        }
        let block_mode = self.p.store_get(AES + 0x94) & 7;
        let nblocks = self.p.store_get(AES + 0x98) as usize;
        let inc32 = self.p.store_get(AES + 0x9C) & 1 == 0;
        let (Some(out_ch), Some(in_ch)) =
            (self.gdma_channel_for(PERI_AES, true), self.gdma_channel_for(PERI_AES, false))
        else {
            log::warn!("AES DMA started without GDMA channels");
            self.p.store_set(AES + 0x4C, 2);
            return;
        };
        let mut data = self.gdma_gather(out_ch);
        data.resize(nblocks * 16, 0);
        let mut iv: [u8; 16] = self.reg_bytes(AES + 0x50, 16).try_into().unwrap();
        aes_blocks(&key, decrypt, block_mode, inc32, &mut iv, &mut data);
        self.set_reg_bytes(AES + 0x50, &iv);
        self.gdma_scatter(in_ch, &data);
        self.p.store_set(AES + 0x4C, 2); // done
        self.p.aes_irq = true;
        self.irq_dirty = true;
    }

    // ---- SHA (DMA mode) ------------------------------------------------------------------------

    pub fn sha_dma(&mut self, start: bool) {
        let Some(ch) = self.gdma_channel_for(PERI_SHA, true) else {
            log::warn!("SHA DMA started without a GDMA channel");
            return;
        };
        let data = self.gdma_gather(ch);
        let n = self.p.sha.block_num as usize;
        if start {
            self.p.sha.init();
        }
        for b in data.chunks_exact(64).take(n) {
            self.p.sha.compress_bytes(b);
        }
        self.irq_dirty = true;
    }

    // ---- RSA / MPI -------------------------------------------------------------------------------

    fn rsa_num(&self, base: u32, words: usize) -> BigUint {
        let mut v = Vec::with_capacity(words);
        for i in 0..words as u32 {
            v.push(self.p.store_get(base + 4 * i));
        }
        BigUint::from_slice(&v)
    }

    fn rsa_store(&mut self, base: u32, n: &BigUint, words: usize) {
        let d = n.to_u32_digits();
        for i in 0..words {
            self.p.store_set(base + 4 * i as u32, *d.get(i).unwrap_or(&0));
        }
    }

    /// Start register write: 0x80C modexp, 0x810 modmult, 0x814 mult.
    pub fn rsa_start(&mut self, which: u32) {
        let len = (self.p.store_get(RSA + 0x804) & 0x7f) as usize + 1;
        let (m, z, y, x) = (RSA, RSA + 0x200, RSA + 0x400, RSA + 0x600);
        match which {
            0x80C => {
                let (xv, yv, mv) = (self.rsa_num(x, len), self.rsa_num(y, len), self.rsa_num(m, len));
                let r = if mv.is_zero() { BigUint::zero() } else { xv.modpow(&yv, &mv) };
                self.rsa_store(z, &r, len);
            }
            0x810 => {
                // Montgomery: Z = X * Y * Rinv * R^-2 mod M, R = 2^(32*len).
                let (xv, yv, mv, rinv) =
                    (self.rsa_num(x, len), self.rsa_num(y, len), self.rsa_num(m, len), self.rsa_num(z, len));
                let r = if mv.is_zero() {
                    BigUint::zero()
                } else {
                    let rr = (BigUint::one() << (32 * len)) % &mv;
                    match mod_inverse(&rr, &mv) {
                        Some(ri) => (xv * yv % &mv) * rinv % &mv * (&ri * &ri % &mv) % &mv,
                        None => BigUint::zero(),
                    }
                };
                self.rsa_store(z, &r, len);
            }
            0x814 => {
                // Operands are len/2 words: X in X block, Y in the upper half of Z.
                let half = len / 2;
                let xv = self.rsa_num(x, half);
                let yv = self.rsa_num(z + 4 * half as u32, half);
                self.rsa_store(z, &(xv * yv), len);
            }
            _ => {}
        }
        self.p.rsa_irq = true;
        self.irq_dirty = true;
    }
}

fn mod_inverse(a: &BigUint, m: &BigUint) -> Option<BigUint> {
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

fn aes_block(key: &[u8], decrypt: bool, block: &mut [u8]) {
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
fn aes_blocks(key: &[u8], decrypt: bool, mode: u32, inc32: bool, iv: &mut [u8; 16], data: &mut [u8]) {
    for blk in data.chunks_exact_mut(16) {
        match mode {
            0 => aes_block(key, decrypt, blk),
            1 => {
                // CBC
                if decrypt {
                    let c: [u8; 16] = blk.try_into().unwrap();
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
                let c: [u8; 16] = if decrypt { blk.try_into().unwrap() } else { [0; 16] };
                blk.iter_mut().zip(ks.iter()).for_each(|(b, v)| *b ^= v);
                *iv = if decrypt { c } else { blk.try_into().unwrap() };
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
