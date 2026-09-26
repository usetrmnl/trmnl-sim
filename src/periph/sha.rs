//! SHA accelerator, register-block and DMA modes. Modes 0-2 (SHA-1, SHA-224, SHA-256)
//! exist on every chip; the ESP32-S3 adds the SHA-512 family: 3 SHA-384, 4 SHA-512,
//! 5 SHA-512/224, 6 SHA-512/256 and 7 SHA-512/t (t from T_LENGTH). The digest registers
//! (H_MEM, up to 64 bytes) and message registers (M_MEM, up to 128 bytes) hold big-endian
//! words, i.e. byte order in memory matches the digest/message byte order; a 64-bit
//! SHA-512 word is two registers, high half first.

#[derive(Default)]
pub struct Sha {
    pub mode: u32,
    /// Digest state as big-endian 32-bit words (SHA-512 words split high, low).
    pub h: [u32; 16],
    pub m: [u32; 32],
    pub block_num: u32,
    /// T_LENGTH: the `t` of SHA-512/t.
    pub t_length: u32,
    pub irq_ena: u32,
    pub irq_raw: bool,
}

const SHA1_IV: [u32; 5] = [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0];
const SHA224_IV: [u32; 8] =
    [0xc1059ed8, 0x367cd507, 0x3070dd17, 0xf70e5939, 0xffc00b31, 0x68581511, 0x64f98fa7, 0xbefa4fa4];
const SHA256_IV: [u32; 8] =
    [0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19];
const SHA384_IV: [u64; 8] = [
    0xcbbb9d5dc1059ed8,
    0x629a292a367cd507,
    0x9159015a3070dd17,
    0x152fecd8f70e5939,
    0x67332667ffc00b31,
    0x8eb44a8768581511,
    0xdb0c2e0d64f98fa7,
    0x47b5481dbefa4fa4,
];
const SHA512_IV: [u64; 8] = [
    0x6a09e667f3bcc908,
    0xbb67ae8584caa73b,
    0x3c6ef372fe94f82b,
    0xa54ff53a5f1d36f1,
    0x510e527fade682d1,
    0x9b05688c2b3e6c1f,
    0x1f83d9abfb41bd6b,
    0x5be0cd19137e2179,
];

impl Sha {
    pub fn irq(&self) -> bool {
        self.irq_raw && self.irq_ena & 1 != 0
    }

    /// Whether the mode is one of the SHA-512 family (128-byte blocks, 64-bit words).
    fn wide(&self) -> bool {
        self.mode >= 3
    }

    /// Bytes per message block for the current mode.
    pub fn block_len(&self) -> usize {
        if self.wide() { 128 } else { 64 }
    }

    pub fn read(&self, off: u32) -> u32 {
        match off {
            0x00 => self.mode,
            0x08 => self.t_length,
            0x0C => self.block_num,
            0x18 => 0, // not busy
            0x28 => self.irq_ena,
            0x2C => 0x2019_0402,
            0x40..=0x7C => self.h[((off - 0x40) / 4) as usize].swap_bytes(),
            0x80..=0xFC => self.m[((off - 0x80) / 4) as usize].swap_bytes(),
            _ => 0,
        }
    }

    pub fn write(&mut self, off: u32, v: u32) {
        match off {
            0x00 => self.mode = v & 7,
            0x08 => self.t_length = v & 0x1ff,
            0x0C => self.block_num = v,
            0x10 => {
                self.init();
                self.compress_m();
            }
            0x14 => self.compress_m(),
            0x24 => self.irq_raw = false,
            0x28 => self.irq_ena = v,
            0x40..=0x7C => self.h[((off - 0x40) / 4) as usize] = v.swap_bytes(),
            0x80..=0xFC => self.m[((off - 0x80) / 4) as usize] = v.swap_bytes(),
            _ => {}
        }
    }

    pub fn init(&mut self) {
        self.h = [0; 16];
        match self.mode {
            0 => self.h[..5].copy_from_slice(&SHA1_IV),
            1 => self.h[..8].copy_from_slice(&SHA224_IV),
            2 => self.h[..8].copy_from_slice(&SHA256_IV),
            3 => self.set_h64(&SHA384_IV),
            4 => self.set_h64(&SHA512_IV),
            5 => self.set_h64(&sha512t_iv(224)),
            6 => self.set_h64(&sha512t_iv(256)),
            _ => self.set_h64(&sha512t_iv(self.t_length)),
        }
    }

    fn h64(&self) -> [u64; 8] {
        std::array::from_fn(|i| (self.h[2 * i] as u64) << 32 | self.h[2 * i + 1] as u64)
    }

    fn set_h64(&mut self, h: &[u64; 8]) {
        for (i, w) in h.iter().enumerate() {
            self.h[2 * i] = (w >> 32) as u32;
            self.h[2 * i + 1] = *w as u32;
        }
    }

    fn compress_m(&mut self) {
        let m = self.m;
        self.compress(&m);
    }

    /// Process one block given as big-endian words (16 of them, or 32 in SHA-512 modes).
    pub fn compress(&mut self, m: &[u32; 32]) {
        match self.mode {
            0 => sha1_compress(&mut self.h, m[..16].try_into().unwrap()),
            1 | 2 => sha256_compress(&mut self.h, m[..16].try_into().unwrap()),
            _ => {
                let mut h = self.h64();
                let w: [u64; 16] = std::array::from_fn(|i| (m[2 * i] as u64) << 32 | m[2 * i + 1] as u64);
                sha512_compress(&mut h, &w);
                self.set_h64(&h);
            }
        }
        self.irq_raw = true;
    }

    /// Process one block of `block_len()` bytes.
    pub fn compress_bytes(&mut self, block: &[u8]) {
        let mut m = [0u32; 32];
        for (w, b) in m.iter_mut().zip(block.chunks_exact(4)) {
            *w = u32::from_be_bytes(b.try_into().unwrap());
        }
        self.compress(&m);
    }
}

/// FIPS 180-4 5.3.6: the initial hash value of SHA-512/t.
fn sha512t_iv(t: u32) -> [u64; 8] {
    let mut h = SHA512_IV.map(|w| w ^ 0xa5a5a5a5a5a5a5a5);
    let msg = format!("SHA-512/{t}");
    let mut block = [0u8; 128];
    block[..msg.len()].copy_from_slice(msg.as_bytes());
    block[msg.len()] = 0x80;
    block[120..].copy_from_slice(&(msg.len() as u64 * 8).to_be_bytes());
    let w: [u64; 16] = std::array::from_fn(|i| u64::from_be_bytes(block[i * 8..i * 8 + 8].try_into().unwrap()));
    sha512_compress(&mut h, &w);
    h
}

fn sha1_compress(h: &mut [u32; 16], m: &[u32; 16]) {
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

fn sha256_compress(h: &mut [u32; 16], m: &[u32; 16]) {
    let mut w = [0u32; 64];
    w[..16].copy_from_slice(m);
    for i in 16..64 {
        let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
        let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
        w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
    }
    let mut s: [u32; 8] = h[..8].try_into().unwrap();
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

const K512: [u64; 80] = [
    0x428a2f98d728ae22,
    0x7137449123ef65cd,
    0xb5c0fbcfec4d3b2f,
    0xe9b5dba58189dbbc,
    0x3956c25bf348b538,
    0x59f111f1b605d019,
    0x923f82a4af194f9b,
    0xab1c5ed5da6d8118,
    0xd807aa98a3030242,
    0x12835b0145706fbe,
    0x243185be4ee4b28c,
    0x550c7dc3d5ffb4e2,
    0x72be5d74f27b896f,
    0x80deb1fe3b1696b1,
    0x9bdc06a725c71235,
    0xc19bf174cf692694,
    0xe49b69c19ef14ad2,
    0xefbe4786384f25e3,
    0x0fc19dc68b8cd5b5,
    0x240ca1cc77ac9c65,
    0x2de92c6f592b0275,
    0x4a7484aa6ea6e483,
    0x5cb0a9dcbd41fbd4,
    0x76f988da831153b5,
    0x983e5152ee66dfab,
    0xa831c66d2db43210,
    0xb00327c898fb213f,
    0xbf597fc7beef0ee4,
    0xc6e00bf33da88fc2,
    0xd5a79147930aa725,
    0x06ca6351e003826f,
    0x142929670a0e6e70,
    0x27b70a8546d22ffc,
    0x2e1b21385c26c926,
    0x4d2c6dfc5ac42aed,
    0x53380d139d95b3df,
    0x650a73548baf63de,
    0x766a0abb3c77b2a8,
    0x81c2c92e47edaee6,
    0x92722c851482353b,
    0xa2bfe8a14cf10364,
    0xa81a664bbc423001,
    0xc24b8b70d0f89791,
    0xc76c51a30654be30,
    0xd192e819d6ef5218,
    0xd69906245565a910,
    0xf40e35855771202a,
    0x106aa07032bbd1b8,
    0x19a4c116b8d2d0c8,
    0x1e376c085141ab53,
    0x2748774cdf8eeb99,
    0x34b0bcb5e19b48a8,
    0x391c0cb3c5c95a63,
    0x4ed8aa4ae3418acb,
    0x5b9cca4f7763e373,
    0x682e6ff3d6b2b8a3,
    0x748f82ee5defb2fc,
    0x78a5636f43172f60,
    0x84c87814a1f0ab72,
    0x8cc702081a6439ec,
    0x90befffa23631e28,
    0xa4506cebde82bde9,
    0xbef9a3f7b2c67915,
    0xc67178f2e372532b,
    0xca273eceea26619c,
    0xd186b8c721c0c207,
    0xeada7dd6cde0eb1e,
    0xf57d4f7fee6ed178,
    0x06f067aa72176fba,
    0x0a637dc5a2c898a6,
    0x113f9804bef90dae,
    0x1b710b35131c471b,
    0x28db77f523047d84,
    0x32caab7b40c72493,
    0x3c9ebe0a15c9bebc,
    0x431d67c49c100d4c,
    0x4cc5d4becb3e42b6,
    0x597f299cfc657e2a,
    0x5fcb6fab3ad6faec,
    0x6c44198c4a475817,
];

fn sha512_compress(h: &mut [u64; 8], m: &[u64; 16]) {
    let mut w = [0u64; 80];
    w[..16].copy_from_slice(m);
    for i in 16..80 {
        let s0 = w[i - 15].rotate_right(1) ^ w[i - 15].rotate_right(8) ^ (w[i - 15] >> 7);
        let s1 = w[i - 2].rotate_right(19) ^ w[i - 2].rotate_right(61) ^ (w[i - 2] >> 6);
        w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
    }
    let mut s = *h;
    for i in 0..80 {
        let [a, b, c, d, e, f, g, hh] = s;
        let s1 = e.rotate_right(14) ^ e.rotate_right(18) ^ e.rotate_right(41);
        let ch = (e & f) ^ (!e & g);
        let t1 = hh.wrapping_add(s1).wrapping_add(ch).wrapping_add(K512[i]).wrapping_add(w[i]);
        let s0 = a.rotate_right(28) ^ a.rotate_right(34) ^ a.rotate_right(39);
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
        digest_t(mode, 0, msg)
    }

    /// Pad and hash `msg` block by block, like the IDF driver does.
    fn digest_t(mode: u32, t: u32, msg: &[u8]) -> Vec<u8> {
        let mut s = Sha { mode, t_length: t, ..Default::default() };
        s.init();
        let block = s.block_len();
        let len_bytes = block / 8; // 64-bit length (128-bit for SHA-512)
        let mut data = msg.to_vec();
        data.push(0x80);
        while data.len() % block != block - len_bytes {
            data.push(0);
        }
        data.extend_from_slice(&vec![0; len_bytes - 8]);
        data.extend_from_slice(&((msg.len() as u64) * 8).to_be_bytes());
        for b in data.chunks(block) {
            s.compress_bytes(b);
        }
        let bytes = match mode {
            0 => 20,
            1 => 28,
            2 => 32,
            3 => 48,
            4 => 64,
            5 => 28,
            6 => 32,
            _ => t as usize / 8,
        };
        s.h.iter().flat_map(|w| w.to_be_bytes()).take(bytes).collect()
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

    #[test]
    fn known_answers_sha512_family() {
        assert_eq!(
            hex(&digest(3, b"abc")),
            "cb00753f45a35e8bb5a03d699ac65007272c32ab0eded1631a8b605a43ff5bed8086072ba1e7cc2358baeca134c825a7"
        );
        assert_eq!(
            hex(&digest(4, b"abc")),
            "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a\
             2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f"
        );
        assert_eq!(hex(&digest(5, b"abc")), "4634270f707b6a54daae7530460842e20e37ed265ceee9a43e8924aa");
        assert_eq!(hex(&digest(6, b"abc")), "53048e2681941ef99b2e29b76b4c7dabe4c2d0c634fc6d46e0e2f13107e7af23");
        // SHA-512/t with t from T_LENGTH matches the fixed-IV modes.
        assert_eq!(digest_t(7, 256, b"abc"), digest(6, b"abc"));
        // Two blocks: the 112-byte FIPS 180-2 message.
        let two = b"abcdefghbcdefghicdefghijdefghijkefghijklfghijklmghijklmnhijklmnoijklmnopjklmnopqklmnopqrlmnopqrsmnopqrstnopqrstu";
        assert_eq!(
            hex(&digest(3, two)),
            "09330c33f71147e83d192fc782cd1b4753111b173b3b05d22fa08086e3b0f712fcc7c71a557e2db966c3e9fa91746039"
        );
    }

    #[test]
    fn sha512_registers_are_big_endian_word_pairs() {
        let mut s = Sha::default();
        s.write(0x00, 4);
        s.init();
        // H0 = 0x6a09e667f3bcc908: memory bytes 6a 09 e6 67 f3 bc c9 08.
        assert_eq!(s.read(0x40).to_le_bytes(), [0x6a, 0x09, 0xe6, 0x67]);
        assert_eq!(s.read(0x44).to_le_bytes(), [0xf3, 0xbc, 0xc9, 0x08]);
        assert_eq!(s.block_len(), 128);
    }
}
