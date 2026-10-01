//! Host-side elliptic-curve math for the ECC and ECDSA accelerators (ESP32-C5): the NIST
//! prime curves P-192, P-256 and P-384 (a = -3), in Jacobian coordinates.

use num_bigint::BigUint;
use num_traits::{One, Zero};

use super::crypto_math::mod_inverse;

pub struct Curve {
    pub p: BigUint,
    pub b: BigUint,
    pub n: BigUint,
    pub gx: BigUint,
    pub gy: BigUint,
    /// Size of a coordinate in bytes.
    pub len: usize,
}

fn hex(s: &str) -> BigUint {
    BigUint::parse_bytes(s.as_bytes(), 16).expect("curve constant")
}

impl Curve {
    /// The curve for the accelerators' curve field: 0 = P-192, 1 = P-256, 2 = P-384.
    pub fn by_id(id: u32) -> Option<Curve> {
        Some(match id {
            0 => Curve {
                p: hex("fffffffffffffffffffffffffffffffeffffffffffffffff"),
                b: hex("64210519e59c80e70fa7e9ab72243049feb8deecc146b9b1"),
                n: hex("ffffffffffffffffffffffff99def836146bc9b1b4d22831"),
                gx: hex("188da80eb03090f67cbf20eb43a18800f4ff0afd82ff1012"),
                gy: hex("07192b95ffc8da78631011ed6b24cdd573f977a11e794811"),
                len: 24,
            },
            1 => Curve {
                p: hex("ffffffff00000001000000000000000000000000ffffffffffffffffffffffff"),
                b: hex("5ac635d8aa3a93e7b3ebbd55769886bc651d06b0cc53b0f63bce3c3e27d2604b"),
                n: hex("ffffffff00000000ffffffffffffffffbce6faada7179e84f3b9cac2fc632551"),
                gx: hex("6b17d1f2e12c4247f8bce6e563a440f277037d812deb33a0f4a13945d898c296"),
                gy: hex("4fe342e2fe1a7f9b8ee7eb4a7c0f9e162bce33576b315ececbb6406837bf51f5"),
                len: 32,
            },
            2 => Curve {
                p: hex(
                    "fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffeffffffff0000000000000000ffffffff",
                ),
                b: hex(
                    "b3312fa7e23ee7e4988e056be3f82d19181d9c6efe8141120314088f5013875ac656398d8a2ed19d2a85c8edd3ec2aef",
                ),
                n: hex(
                    "ffffffffffffffffffffffffffffffffffffffffffffffffc7634d81f4372ddf581a0db248b0a77aecec196accc52973",
                ),
                gx: hex(
                    "aa87ca22be8b05378eb1c71ef320ad746e1d3b628ba79b9859f741e082542a385502f25dbf55296c3a545e3872760ab7",
                ),
                gy: hex(
                    "3617de4a96262c6f5d9e98bf9292dc29f8f41dbd289a147ce9da3113b5f0b8c00a60b1ce1d7e819d7a431d7c90ea0e5f",
                ),
                len: 48,
            },
            _ => return None,
        })
    }

    fn sub(&self, a: &BigUint, b: &BigUint) -> BigUint {
        ((a + &self.p) - (b % &self.p)) % &self.p
    }

    /// y² = x³ - 3x + b (mod p), for affine coordinates.
    pub fn on_curve(&self, x: &BigUint, y: &BigUint) -> bool {
        if x >= &self.p || y >= &self.p {
            return false;
        }
        let p = &self.p;
        let lhs = y * y % p;
        let rhs = self.sub(&((x * x % p) * x % p + &self.b), &(BigUint::from(3u32) * x % p));
        lhs == rhs % p
    }

    /// Jacobian point doubling (a = -3). `None` = the point at infinity.
    fn double(&self, pt: &Option<Jac>) -> Option<Jac> {
        let j = pt.as_ref()?;
        let (x, y, z) = (&j.x, &j.y, &j.z);
        let p = &self.p;
        if y.is_zero() {
            return None;
        }
        let zz = z * z % p;
        // m = 3(x - z²)(x + z²)
        let m = BigUint::from(3u32) * self.sub(x, &zz) % p * ((x + &zz) % p) % p;
        let yy = y * y % p;
        let s = BigUint::from(4u32) * x % p * &yy % p;
        let x3 = self.sub(&(&m * &m % p), &(BigUint::from(2u32) * &s % p));
        let y3 = self.sub(&(&m * self.sub(&s, &x3) % p), &(BigUint::from(8u32) * (&yy * &yy % p) % p));
        let z3 = BigUint::from(2u32) * y % p * z % p;
        Some(Jac { x: x3, y: y3, z: z3 })
    }

    fn add(&self, a: &Option<Jac>, b: &Option<Jac>) -> Option<Jac> {
        let (a, b) = match (a, b) {
            (None, b) => return b.clone(),
            (a, None) => return a.clone(),
            (Some(a), Some(b)) => (a, b),
        };
        let p = &self.p;
        let z1z1 = &a.z * &a.z % p;
        let z2z2 = &b.z * &b.z % p;
        let u1 = &a.x * &z2z2 % p;
        let u2 = &b.x * &z1z1 % p;
        let s1 = &a.y * &b.z % p * &z2z2 % p;
        let s2 = &b.y * &a.z % p * &z1z1 % p;
        if u1 == u2 {
            return if s1 == s2 { self.double(&Some(a.clone())) } else { None };
        }
        let h = self.sub(&u2, &u1);
        let r = self.sub(&s2, &s1);
        let hh = &h * &h % p;
        let hhh = &h * &hh % p;
        let v = &u1 * &hh % p;
        let x3 = self.sub(&self.sub(&(&r * &r % p), &hhh), &(BigUint::from(2u32) * &v % p));
        let y3 = self.sub(&(&r * self.sub(&v, &x3) % p), &(&s1 * &hhh % p));
        let z3 = &a.z * &b.z % p * &h % p;
        Some(Jac { x: x3, y: y3, z: z3 })
    }

    /// k·P for P in Jacobian coordinates.
    pub fn mul_jac(&self, k: &BigUint, pt: &Jac) -> Option<Jac> {
        let base = Some(pt.clone());
        let mut acc: Option<Jac> = None;
        for i in (0..k.bits()).rev() {
            acc = self.double(&acc);
            if k.bit(i) {
                acc = self.add(&acc, &base);
            }
        }
        acc
    }

    pub fn mul(&self, k: &BigUint, x: &BigUint, y: &BigUint) -> Option<(BigUint, BigUint)> {
        self.mul_jac(k, &Jac::affine(x, y)).and_then(|j| self.to_affine(&j))
    }

    pub fn add_affine_jac(&self, x: &BigUint, y: &BigUint, q: &Jac) -> Option<Jac> {
        self.add(&Some(Jac::affine(x, y)), &Some(q.clone()))
    }

    pub fn to_affine(&self, j: &Jac) -> Option<(BigUint, BigUint)> {
        if j.z.is_zero() {
            return None;
        }
        let p = &self.p;
        let zi = mod_inverse(&(&j.z % p), p)?;
        let zi2 = &zi * &zi % p;
        Some((&j.x * &zi2 % p, &j.y * &zi2 % p * &zi % p))
    }

    /// Whether a Jacobian point satisfies the curve equation (Y² = X³ - 3XZ⁴ + bZ⁶).
    pub fn jac_on_curve(&self, j: &Jac) -> bool {
        match self.to_affine(j) {
            Some((x, y)) => self.on_curve(&x, &y),
            None => false,
        }
    }

    /// ECDSA verification of (r, s) over the hash value `z` with public key Q.
    pub fn ecdsa_verify(&self, z: &BigUint, r: &BigUint, s: &BigUint, qx: &BigUint, qy: &BigUint) -> bool {
        let n = &self.n;
        if r.is_zero() || s.is_zero() || r >= n || s >= n || !self.on_curve(qx, qy) {
            return false;
        }
        let Some(w) = mod_inverse(s, n) else { return false };
        let e = z % n;
        let u1 = &e * &w % n;
        let u2 = r * &w % n;
        let a = self.mul_jac(&u1, &Jac::affine(&self.gx, &self.gy));
        let b = self.mul_jac(&u2, &Jac::affine(qx, qy));
        match self.add(&a, &b).and_then(|j| self.to_affine(&j)) {
            Some((x, _)) => &(x % n) == r,
            None => false,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Jac {
    pub x: BigUint,
    pub y: BigUint,
    pub z: BigUint,
}

impl Jac {
    pub fn affine(x: &BigUint, y: &BigUint) -> Jac {
        Jac { x: x.clone(), y: y.clone(), z: BigUint::one() }
    }
}

/// A little-endian byte field of `len` bytes as a number.
pub fn from_le(b: &[u8]) -> BigUint {
    BigUint::from_bytes_le(b)
}

/// A number as `len` little-endian bytes (truncated or zero-padded).
pub fn to_le(n: &BigUint, len: usize) -> Vec<u8> {
    let mut v = n.to_bytes_le();
    v.resize(len, 0);
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generators_are_on_their_curves_and_have_order_n() {
        for id in 0..3 {
            let c = Curve::by_id(id).unwrap();
            assert!(c.on_curve(&c.gx, &c.gy), "curve {id}");
            assert!(c.mul(&c.n, &c.gx, &c.gy).is_none(), "n·G is the point at infinity (curve {id})");
            let (x2, y2) = c.mul(&BigUint::from(2u32), &c.gx, &c.gy).unwrap();
            assert!(c.on_curve(&x2, &y2));
        }
    }

    #[test]
    fn p256_known_multiple() {
        // 2·G on P-256
        let c = Curve::by_id(1).unwrap();
        let (x, y) = c.mul(&BigUint::from(2u32), &c.gx, &c.gy).unwrap();
        assert_eq!(x, hex("7cf27b188d034f7e8a52380304b51ac3c08969e277f21b35a60b48fc47669978"));
        assert_eq!(y, hex("07775510db8ed040293d9ac69f7430dbba7dade63ce982299e04b79d227873d1"));
    }

    #[test]
    fn ecdsa_sign_verify_round_trip() {
        let c = Curve::by_id(1).unwrap();
        let d = BigUint::from(0x1234_5678_9abc_def0u64);
        let (qx, qy) = c.mul(&d, &c.gx, &c.gy).unwrap();
        let z = BigUint::from(0xdead_beefu32);
        let k = BigUint::from(0x4242_4242u32);
        let (rx, _) = c.mul(&k, &c.gx, &c.gy).unwrap();
        let r = rx % &c.n;
        let kinv = mod_inverse(&k, &c.n).unwrap();
        let s = kinv * (&z + &r * &d) % &c.n;
        assert!(c.ecdsa_verify(&z, &r, &s, &qx, &qy));
        assert!(!c.ecdsa_verify(&(z + 1u32), &r, &s, &qx, &qy));
    }
}
