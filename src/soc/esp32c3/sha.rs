//! SHA accelerator (SHA-1, SHA-224, SHA-256), register-block mode. The digest
//! registers (H_MEM) and message registers (M_MEM) hold big-endian words, i.e.
//! byte order in memory matches the digest/message byte order.

#[derive(Default)]
pub struct Sha {
    pub mode: u32,
    pub h: [u32; 8],
    pub m: [u32; 16],
    pub block_num: u32,
    pub irq_ena: u32,
    pub irq_raw: bool,
}

const SHA1_IV: [u32; 5] = [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0];
const SHA224_IV: [u32; 8] =
    [0xc1059ed8, 0x367cd507, 0x3070dd17, 0xf70e5939, 0xffc00b31, 0x68581511, 0x64f98fa7, 0xbefa4fa4];
const SHA256_IV: [u32; 8] =
    [0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19];

impl Sha {
    pub fn irq(&self) -> bool {
        self.irq_raw && self.irq_ena & 1 != 0
    }

    pub fn read(&self, off: u32) -> u32 {
        match off {
            0x00 => self.mode,
            0x0C => self.block_num,
            0x18 => 0, // not busy
            0x28 => self.irq_ena,
            0x2C => 0x2019_0402,
            0x40..=0x5C => self.h[((off - 0x40) / 4) as usize].swap_bytes(),
            0x80..=0xBC => self.m[((off - 0x80) / 4) as usize].swap_bytes(),
            _ => 0,
        }
    }

    pub fn write(&mut self, off: u32, v: u32) {
        match off {
            0x00 => self.mode = v & 7,
            0x0C => self.block_num = v,
            0x10 => {
                self.init();
                self.compress_m();
            }
            0x14 => self.compress_m(),
            0x24 => self.irq_raw = false,
            0x28 => self.irq_ena = v,
            0x40..=0x5C => self.h[((off - 0x40) / 4) as usize] = v.swap_bytes(),
            0x80..=0xBC => self.m[((off - 0x80) / 4) as usize] = v.swap_bytes(),
            _ => {}
        }
    }

    pub fn init(&mut self) {
        self.h = [0; 8];
        match self.mode {
            0 => self.h[..5].copy_from_slice(&SHA1_IV),
            1 => self.h = SHA224_IV,
            _ => self.h = SHA256_IV,
        }
    }

    fn compress_m(&mut self) {
        let m = self.m;
        self.compress(&m);
    }

    /// Process one 64-byte block given as big-endian words.
    pub fn compress(&mut self, m: &[u32; 16]) {
        if self.mode == 0 {
            sha1_compress(&mut self.h, m);
        } else {
            sha256_compress(&mut self.h, m);
        }
        self.irq_raw = true;
    }

    pub fn compress_bytes(&mut self, block: &[u8]) {
        let mut m = [0u32; 16];
        for (i, w) in m.iter_mut().enumerate() {
            *w = u32::from_be_bytes(block[i * 4..i * 4 + 4].try_into().unwrap());
        }
        self.compress(&m);
    }
}

fn sha1_compress(h: &mut [u32; 8], m: &[u32; 16]) {
    let mut w = [0u32; 80];
    w[..16].copy_from_slice(m);
    for i in 16..80 {
        w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
    }
    let (mut a, mut b, mut c, mut d, mut e) = (h[0], h[1], h[2], h[3], h[4]);
    for (i, &wi) in w.iter().enumerate() {
        let (f, k) = match i {
            0..=19 => ((b & c) | (!b & d), 0x5A827999),
            20..=39 => (b ^ c ^ d, 0x6ED9EBA1),
            40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDC),
            _ => (b ^ c ^ d, 0xCA62C1D6),
        };
        let t = a.rotate_left(5).wrapping_add(f).wrapping_add(e).wrapping_add(k).wrapping_add(wi);
        e = d;
        d = c;
        c = b.rotate_left(30);
        b = a;
        a = t;
    }
    for (x, y) in h.iter_mut().zip([a, b, c, d, e]) {
        *x = x.wrapping_add(y);
    }
}

const K256: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5, 0xd807aa98,
    0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786,
    0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8,
    0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13,
    0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819,
    0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a,
    0x5b9cca4f, 0x682e6ff3, 0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
    0xc67178f2,
];

fn sha256_compress(h: &mut [u32; 8], m: &[u32; 16]) {
    let mut w = [0u32; 64];
    w[..16].copy_from_slice(m);
    for i in 16..64 {
        let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
        let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
        w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
    }
    let mut s = *h;
    for i in 0..64 {
        let [a, b, c, d, e, f, g, hh] = s;
        let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
        let ch = (e & f) ^ (!e & g);
        let t1 = hh.wrapping_add(s1).wrapping_add(ch).wrapping_add(K256[i]).wrapping_add(w[i]);
        let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
        let maj = (a & b) ^ (a & c) ^ (b & c);
        let t2 = s0.wrapping_add(maj);
        s = [t1.wrapping_add(t2), a, b, c, d.wrapping_add(t1), e, f, g];
    }
    for (x, y) in h.iter_mut().zip(s) {
        *x = x.wrapping_add(y);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest(mode: u32, msg: &[u8]) -> Vec<u8> {
        let mut s = Sha { mode, ..Default::default() };
        s.init();
        let mut data = msg.to_vec();
        let bitlen = (msg.len() as u64) * 8;
        data.push(0x80);
        while data.len() % 64 != 56 {
            data.push(0);
        }
        data.extend_from_slice(&bitlen.to_be_bytes());
        for b in data.chunks(64) {
            s.compress_bytes(b);
        }
        let n = match mode {
            0 => 5,
            1 => 7,
            _ => 8,
        };
        s.h[..n].iter().flat_map(|w| w.to_be_bytes()).collect()
    }

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    #[test]
    fn known_answers() {
        assert_eq!(hex(&digest(0, b"abc")), "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(hex(&digest(2, b"abc")), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        assert_eq!(hex(&digest(1, b"abc")), "23097d223405d8228642a477bda255b32aadbce4bda0b3f7e36c9da7");
    }
}
