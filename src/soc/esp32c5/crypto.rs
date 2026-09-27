//! AHB-DMA-fed crypto accelerators (AES, SHA in DMA mode), the RSA/MPI engine and the ECC
//! and ECDSA accelerators. The math is done on the host; the models only move data the
//! way the hardware would (descriptor chains, register blocks, status bits).

use crate::periph::crypto_math::{aes_block, aes_blocks, rsa_op};
use crate::periph::ec::{Curve, Jac, from_le, to_le};
use num_bigint::BigUint;

use super::bus::C5Bus;

pub const DMA: u32 = 0x6008_0000;
pub const AES: u32 = 0x6008_8000;
pub const RSA: u32 = 0x6008_A000;
pub const ECC: u32 = 0x6008_B000;
pub const ECDSA: u32 = 0x6008_E000;

const PERI_AES: u32 = 6;
const PERI_SHA: u32 = 7;
const CH_STRIDE: u32 = 0xC0;
/// IN_LINK_ADDR_CHn / OUT_LINK_ADDR_CHn: full 32-bit descriptor addresses.
const IN_LINK_ADDR: u32 = DMA + 0x3AC;
const OUT_LINK_ADDR: u32 = DMA + 0x3B8;

// ECC_MULT registers
const ECC_INT_RAW: u32 = ECC + 0x0C;
const ECC_INT_CLR: u32 = ECC + 0x18;
const ECC_CONF: u32 = ECC + 0x1C;
const ECC_K: u32 = ECC + 0x100;
const ECC_PX: u32 = ECC + 0x130;
const ECC_PY: u32 = ECC + 0x160;
const ECC_QX: u32 = ECC + 0x190;
const ECC_QY: u32 = ECC + 0x1C0;
const ECC_QZ: u32 = ECC + 0x1F0;

// ECDSA registers
const ECDSA_CONF: u32 = ECDSA + 0x04;
const ECDSA_INT_RAW: u32 = ECDSA + 0x0C;
const ECDSA_INT_CLR: u32 = ECDSA + 0x18;
const ECDSA_START: u32 = ECDSA + 0x1C;
const ECDSA_STATE: u32 = ECDSA + 0x20;
const ECDSA_RESULT: u32 = ECDSA + 0x24;
const ECDSA_R: u32 = ECDSA + 0x3E0;
const ECDSA_S: u32 = ECDSA + 0x410;
const ECDSA_Z: u32 = ECDSA + 0x440;
const ECDSA_QAX: u32 = ECDSA + 0x470;
const ECDSA_QAY: u32 = ECDSA + 0x4A0;

impl C5Bus {
    fn dma_channel_for(&self, peri: u32, out_dir: bool) -> Option<u32> {
        (0..3).find(|&ch| {
            let sel = if out_dir { 0x100 } else { 0x0A0 };
            self.p.store_get(DMA + sel + ch * CH_STRIDE) & 0x3f == peri
        })
    }

    /// Collect the bytes described by the out-link (memory -> peripheral) of `ch`.
    fn dma_gather(&mut self, ch: u32) -> Vec<u8> {
        let mut addr = self.p.store_get(OUT_LINK_ADDR + 4 * ch);
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
        // OUT_DONE / OUT_EOF / OUT_TOTAL_EOF
        let raw = DMA + 0x30 + 0x10 * ch;
        let v = self.p.store_get(raw);
        self.p.store_set(raw, v | 1 << 0 | 1 << 1 | 1 << 3);
        out
    }

    /// Write `data` into the buffers of the in-link (peripheral -> memory) of `ch`.
    fn dma_scatter(&mut self, ch: u32, data: &[u8]) {
        let mut addr = self.p.store_get(IN_LINK_ADDR + 4 * ch);
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
        let raw = DMA + 0x10 * ch;
        let v = self.p.store_get(raw);
        self.p.store_set(raw, v | 1 << 0 | 1 << 1); // IN_DONE | IN_SUC_EOF
    }

    fn reg_bytes(&self, base: u32, n: usize) -> Vec<u8> {
        (0..n.div_ceil(4) as u32).flat_map(|i| self.p.store_get(base + 4 * i).to_le_bytes()).take(n).collect()
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
        let (Some(out_ch), Some(in_ch)) = (self.dma_channel_for(PERI_AES, true), self.dma_channel_for(PERI_AES, false))
        else {
            log::warn!("AES DMA started without DMA channels");
            self.p.store_set(AES + 0x4C, 2);
            return;
        };
        let mut data = self.dma_gather(out_ch);
        data.resize(nblocks * 16, 0);
        let mut iv: [u8; 16] = self.reg_bytes(AES + 0x50, 16).try_into().unwrap();
        aes_blocks(&key, decrypt, block_mode, inc32, &mut iv, &mut data);
        self.set_reg_bytes(AES + 0x50, &iv);
        self.dma_scatter(in_ch, &data);
        self.p.store_set(AES + 0x4C, 2); // done
        self.p.aes_irq = true;
        self.irq_dirty = true;
    }

    // ---- SHA (DMA mode) ------------------------------------------------------------------------

    pub fn sha_dma(&mut self, start: bool) {
        let Some(ch) = self.dma_channel_for(PERI_SHA, true) else {
            log::warn!("SHA DMA started without a DMA channel");
            return;
        };
        let data = self.dma_gather(ch);
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

    // ---- ECC point multiplication ------------------------------------------------------------------

    pub fn ecc_read(&mut self, a: u32, stored: u32) -> u32 {
        match a {
            _ if a == ECC_INT_RAW => self.p.ecc_irq as u32,
            _ if a == ECC + 0x10 => (self.p.ecc_irq && self.p.store_get(ECC + 0x14) & 1 != 0) as u32,
            _ if a == ECC_CONF => stored & !1, // START self-clears
            _ => stored,
        }
    }

    pub fn ecc_write(&mut self, a: u32, v: u32) {
        match a {
            _ if a == ECC_INT_CLR && v & 1 != 0 => {
                self.p.ecc_irq = false;
                self.irq_dirty = true;
            }
            _ if a == ECC + 0x14 => self.irq_dirty = true,
            _ if a == ECC_CONF && v & 1 != 0 => {
                self.ecc_calc(v);
                self.p.ecc_irq = true;
                self.irq_dirty = true;
            }
            _ => {}
        }
    }

    fn ecc_calc(&mut self, conf: u32) {
        let curve_id = (conf >> 2) & 3;
        let mod_p = conf & 1 << 4 != 0;
        let work_mode = (conf >> 5) & 0xf;
        let Some(c) = Curve::by_id(curve_id) else {
            log::warn!("ECC: unsupported curve {curve_id}");
            return;
        };
        let len = c.len;
        let get = |b: &C5Bus, base: u32| from_le(&b.reg_bytes(base, len));
        let (k, px, py) = (get(self, ECC_K), get(self, ECC_PX), get(self, ECC_PY));
        let (qx, qy, qz) = (get(self, ECC_QX), get(self, ECC_QY), get(self, ECC_QZ));
        let mut verified = None;
        let zero = BigUint::default();
        let put = |b: &mut C5Bus, base: u32, n: &BigUint| b.set_reg_bytes(base, &to_le(n, len));
        match work_mode {
            // point multiplication (2, 3: verify P first)
            0 | 2 | 3 => {
                if work_mode != 0 {
                    verified = Some(c.on_curve(&px, &py));
                }
                if work_mode != 2 {
                    let (rx, ry) = c.mul(&k, &px, &py).unwrap_or((zero.clone(), zero.clone()));
                    put(self, ECC_PX, &rx);
                    put(self, ECC_PY, &ry);
                }
            }
            // Jacobian point multiplication (7: verify P first); result in QX/QY/QZ
            4 | 7 => {
                if work_mode == 7 {
                    verified = Some(c.on_curve(&px, &py));
                }
                let r = c.mul_jac(&k, &Jac::affine(&px, &py));
                let (x, y, z) = r.map_or((zero.clone(), zero.clone(), zero.clone()), |j| (j.x, j.y, j.z));
                put(self, ECC_QX, &x);
                put(self, ECC_QY, &y);
                put(self, ECC_QZ, &z);
            }
            // point addition: P (affine) + Q (Jacobian) -> Q (Jacobian) and P (affine)
            5 => {
                let q = Jac { x: qx, y: qy, z: qz };
                if let Some(r) = c.add_affine_jac(&px, &py, &q) {
                    if let Some((ax, ay)) = c.to_affine(&r) {
                        put(self, ECC_PX, &ax);
                        put(self, ECC_PY, &ay);
                    }
                    put(self, ECC_QX, &r.x);
                    put(self, ECC_QY, &r.y);
                    put(self, ECC_QZ, &r.z);
                }
            }
            // Jacobian point verification
            6 => verified = Some(c.jac_on_curve(&Jac { x: qx, y: qy, z: qz })),
            // modular arithmetic on PX, PY
            8..=11 => {
                let m = if mod_p { &c.p } else { &c.n };
                let (a, b) = (&px % m, &py % m);
                match work_mode {
                    8 => put(self, ECC_PX, &((&a + &b) % m)),
                    9 => put(self, ECC_PX, &((&a + m - &b) % m)),
                    10 => put(self, ECC_PY, &(&a * &b % m)),
                    _ => {
                        // PY = PY * PX^-1
                        let inv = crate::periph::crypto_math::mod_inverse(&a, m).unwrap_or_default();
                        put(self, ECC_PY, &(&b * inv % m));
                    }
                }
            }
            _ => log::warn!("ECC: unsupported work mode {work_mode}"),
        }
        let mut conf = self.p.store_get(ECC_CONF) & !1;
        if let Some(ok) = verified {
            conf = conf & !(1 << 29) | (ok as u32) << 29;
        }
        self.p.store_set(ECC_CONF, conf);
    }

    // ---- ECDSA (signature verification) --------------------------------------------------------

    pub fn ecdsa_read(&mut self, a: u32, stored: u32) -> u32 {
        match a {
            _ if a == ECDSA_INT_RAW => self.p.ecdsa_irq as u32,
            _ if a == ECDSA_START => 0,
            _ => stored,
        }
    }

    pub fn ecdsa_write(&mut self, a: u32, v: u32) {
        match a {
            _ if a == ECDSA_INT_CLR && v != 0 => {
                self.p.ecdsa_irq = false;
                self.irq_dirty = true;
            }
            _ if a == ECDSA_START => {
                if v & 1 != 0 {
                    self.p.store_set(ECDSA_STATE, 1); // LOAD
                } else if v & 2 != 0 {
                    self.ecdsa_load_done();
                } else if v & 4 != 0 {
                    self.p.store_set(ECDSA_STATE, 0); // IDLE
                }
            }
            _ => {}
        }
    }

    fn ecdsa_load_done(&mut self) {
        let conf = self.p.store_get(ECDSA_CONF);
        let (mode, curve_id) = (conf & 3, (conf >> 2) & 3);
        let Some(c) = Curve::by_id(curve_id) else {
            log::warn!("ECDSA: unsupported curve {curve_id}");
            self.p.store_set(ECDSA_STATE, 0);
            return;
        };
        let len = c.len;
        let get = |b: &C5Bus, base: u32| from_le(&b.reg_bytes(base, len));
        match mode {
            0 => {
                let ok = c.ecdsa_verify(
                    &get(self, ECDSA_Z),
                    &get(self, ECDSA_R),
                    &get(self, ECDSA_S),
                    &get(self, ECDSA_QAX),
                    &get(self, ECDSA_QAY),
                );
                self.p.store_set(ECDSA_RESULT, ok as u32);
                self.p.store_set(ECDSA_STATE, 0);
            }
            _ => {
                // Signing and public key export use eFuse/Key Manager keys the simulator
                // doesn't have.
                log::warn!("ECDSA: work mode {mode} (needs a hardware key) is not modelled");
                self.p.store_set(ECDSA_RESULT, 0);
                self.p.store_set(ECDSA_STATE, if mode == 0 { 0 } else { 2 });
            }
        }
        self.p.ecdsa_irq = true;
        self.irq_dirty = true;
    }
}
