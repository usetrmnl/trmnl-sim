//! GDMA-fed crypto accelerators (AES, SHA in DMA mode) and the RSA/MPI engine.
//! The math is done on the host; the models only move data the way the
//! hardware would (descriptor chains, register blocks, status bits).

use crate::periph::crypto_math::{aes_block, aes_blocks, rsa_op};

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
        let block = self.p.sha.block_len();
        for b in data.chunks_exact(block).take(n) {
            self.p.sha.compress_bytes(b);
        }
        self.irq_dirty = true;
    }

    // ---- RSA / MPI -------------------------------------------------------------------------------

    /// Start register write: 0x80C modexp, 0x810 modmult, 0x814 mult.
    pub fn rsa_start(&mut self, which: u32) {
        let len = (self.p.store_get(RSA + 0x804) & 0x7f) as usize + 1;
        let mut mem = [0u32; 0x800 / 4];
        for (i, w) in mem.iter_mut().enumerate() {
            *w = self.p.store_get(RSA + 4 * i as u32);
        }
        rsa_op(which, len, &mut mem);
        for (i, w) in mem.iter().enumerate() {
            self.p.store_set(RSA + 4 * i as u32, *w);
        }
        self.p.rsa_irq = true;
        self.irq_dirty = true;
    }
}
