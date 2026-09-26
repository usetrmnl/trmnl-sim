#!/usr/bin/env python3
"""Generate tests/xtensa/src/fpu_vectors.h: FPU test vectors with expected results
computed on the host with exact rational arithmetic (fractions.Fraction) and explicit
IEEE-754 binary32 rounding, so no host FPU quirks are involved."""
import math, os, random, struct
from fractions import Fraction as F

OUT = os.path.join(os.path.dirname(__file__), "..", "src", "fpu_vectors.h")

RNE, RTZ, RUP, RDN = 0, 1, 2, 3
QNAN = 0x7FC00000
NAN_ANY = 0xFFFFFFFF  # marker: any NaN accepted

def bits(x):
    return struct.unpack("<I", struct.pack("<f", x))[0]

def val(b):
    return struct.unpack("<f", struct.pack("<I", b))[0]

def is_nan(b):
    return (b & 0x7FFFFFFF) > 0x7F800000

def is_inf(b):
    return (b & 0x7FFFFFFF) == 0x7F800000

def sign(b):
    return b >> 31

def frac(b):
    return F(val(b))  # exact (binary32 -> double -> Fraction)

MAXF = F(val(0x7F7FFFFF))

def round_f32(x, mode, zero_sign=0):
    """Exact rational -> binary32 bits with the given rounding mode."""
    if x == 0:
        return 0x80000000 if zero_sign else 0
    s = 1 if x < 0 else 0
    ax = -x if s else x
    # exponent e with 2^e <= ax < 2^(e+1)
    e = ax.numerator.bit_length() - ax.denominator.bit_length()
    if F(2) ** e > ax:
        e -= 1
    q = F(2) ** max(e - 23, -149)
    n = ax / q
    m = n.numerator // n.denominator
    rem = n - m
    if rem != 0:
        if mode == RNE:
            if rem > F(1, 2) or (rem == F(1, 2) and m % 2 == 1):
                m += 1
        elif mode == RTZ:
            pass
        elif mode == RUP:
            if not s:
                m += 1
        elif mode == RDN:
            if s:
                m += 1
    r = m * q
    if r > MAXF:
        away = mode == RNE or (mode == RUP and not s) or (mode == RDN and s)
        r_bits = 0x7F800000 if away else 0x7F7FFFFF
        return r_bits | (s << 31)
    if r == 0:
        return s << 31
    return bits(float(r)) | (s << 31)

def add(a, b, mode=RNE):
    if is_nan(a) or is_nan(b):
        return NAN_ANY
    if is_inf(a) or is_inf(b):
        if is_inf(a) and is_inf(b) and sign(a) != sign(b):
            return NAN_ANY
        return a if is_inf(a) else b
    x = frac(a) + frac(b)
    if x == 0:
        if frac(a) == 0 and frac(b) == 0 and sign(a) == sign(b):
            return a
        return 0x80000000 if mode == RDN else 0
    return round_f32(x, mode)

def neg(b):
    return b ^ 0x80000000

def mul(a, b, mode=RNE):
    if is_nan(a) or is_nan(b):
        return NAN_ANY
    s = sign(a) ^ sign(b)
    if is_inf(a) or is_inf(b):
        other = b if is_inf(a) else a
        if not is_inf(other) and frac(other) == 0:
            return NAN_ANY
        return 0x7F800000 | (s << 31)
    x = frac(a) * frac(b)
    return round_f32(x, mode, zero_sign=s)

def madd(acc, a, b, mode=RNE):
    """acc + a*b, fused."""
    if is_nan(acc) or is_nan(a) or is_nan(b):
        return NAN_ANY
    p_special = is_inf(a) or is_inf(b)
    if p_special:
        p = mul(a, b)
        if p == NAN_ANY:
            return NAN_ANY
        return add(acc, p)
    if is_inf(acc):
        return acc
    ps = sign(a) ^ sign(b)
    x = frac(acc) + frac(a) * frac(b)
    if x == 0:
        if frac(acc) == 0 and frac(a) * frac(b) == 0 and sign(acc) == ps:
            return acc
        return 0x80000000 if mode == RDN else 0
    return round_f32(x, mode)

def div(a, b):
    if is_nan(a) or is_nan(b):
        return NAN_ANY
    s = sign(a) ^ sign(b)
    if is_inf(a):
        return NAN_ANY if is_inf(b) else 0x7F800000 | (s << 31)
    if is_inf(b):
        return s << 31
    if frac(b) == 0:
        return NAN_ANY if frac(a) == 0 else 0x7F800000 | (s << 31)
    return round_f32(frac(a) / frac(b), RNE, zero_sign=s)

def sqrt(a):
    if is_nan(a):
        return NAN_ANY
    if a & 0x7FFFFFFF == 0:
        return a
    if sign(a):
        return NAN_ANY
    if is_inf(a):
        return a
    # correctly rounded: double sqrt then round (53 >= 2*24+2)
    return round_f32(F(math.sqrt(val(a))), RNE)

def to_int(a, scale, how):
    if is_nan(a):
        return 0x7FFFFFFF
    if is_inf(a):
        return 0x80000000 if sign(a) else 0x7FFFFFFF
    x = frac(a) * 2 ** scale
    if how == "round":
        fl = math.floor(x)
        r = x - fl
        v = fl + (1 if r > F(1, 2) or (r == F(1, 2) and fl % 2) else 0)
    elif how == "trunc":
        v = int(x)
    elif how == "floor":
        v = math.floor(x)
    else:
        v = math.ceil(x)
    if v > 0x7FFFFFFF:
        return 0x7FFFFFFF
    if v < -0x80000000:
        return 0x80000000
    return v & 0xFFFFFFFF

def utrunc(a, scale):
    if is_nan(a):
        return 0xFFFFFFFF
    if sign(a):
        return to_int(a, scale, "trunc")
    if is_inf(a):
        return 0xFFFFFFFF
    v = int(frac(a) * 2 ** scale)
    return 0xFFFFFFFF if v > 0xFFFFFFFF else v

def from_int(v, signed, scale, mode=RNE):
    x = F(v - (1 << 32) if signed and v >= 0x80000000 else v) / 2 ** scale
    return round_f32(x, mode) if x != 0 else 0

def cmp(a, b):
    un = is_nan(a) or is_nan(b)
    if un:
        return dict(un=1, oeq=0, ueq=1, olt=0, ult=1, ole=0, ule=1)
    x, y = val(a), val(b)  # host double compare is exact for binary32 values incl. inf
    return dict(un=0, oeq=int(x == y), ueq=int(x == y), olt=int(x < y), ult=int(x < y), ole=int(x <= y),
                ule=int(x <= y))

SPECIAL = [0x00000000, 0x80000000, 0x3F800000, 0xBF800000, 0x3FC00000, bits(0.1), bits(-0.1), 0x7F7FFFFF, 0xFF7FFFFF,
           0x00800000, 0x80800000, 0x00000001, 0x80000001, 0x007FFFFF, 0x7F800000, 0xFF800000, QNAN, bits(math.pi),
           bits(1 / 3), 0x4B800000, 0x4B800001, 0x4F000000, 0xCF000000, bits(1e10), 0x3F000000, 0x40200000,
           0xC0200000, 0x40600000, bits(-7.25), bits(123456.789), bits(1e-30), 0x7F000000, 0x3F7FFFFF]

def main():
    rnd = random.Random(1234)
    randoms = [rnd.getrandbits(32) for _ in range(24)]
    randoms = [r for r in randoms if not is_nan(r)]
    randoms += [bits(rnd.uniform(-1000, 1000)) for _ in range(24)]
    vals = SPECIAL + randoms
    ops = []  # (name, a, b, c, mode, expected)

    def E(name, a, b, c, mode, exp):
        ops.append((name, a, b, c, mode, exp))

    pairs = [(a, b) for a in vals[:33] for b in vals[:33]]
    pairs += [(rnd.choice(vals), rnd.choice(vals)) for _ in range(300)]
    for a, b in pairs:
        E("ADD", a, b, 0, RNE, add(a, b))
        E("SUB", a, b, 0, RNE, add(a, neg(b)))
        E("MUL", a, b, 0, RNE, mul(a, b))
        E("DIV", a, b, 0, RNE, div(a, b))
        c = cmp(a, b)
        E("CMP", a, b, 0, RNE, c["un"] | c["oeq"] << 1 | c["ueq"] << 2 | c["olt"] << 3 | c["ult"] << 4 |
          c["ole"] << 5 | c["ule"] << 6)
    for _ in range(400):
        acc, a, b = rnd.choice(vals), rnd.choice(vals), rnd.choice(vals)
        E("MADD", acc, a, b, RNE, madd(acc, a, b))
        E("MSUB", acc, a, b, RNE, madd(acc, neg(a), b))
    # fused vs unfused matters here: 1 + x*x - ... exactness
    for x in [0x3F800001, 0x3F7FFFFF, 0x3FAAAAAB, 0x40490FDB]:
        E("MADD", neg(mul(x, x)), x, x, RNE, madd(neg(mul(x, x)), x, x))
    # directed rounding modes
    for mode in (RTZ, RUP, RDN):
        for _ in range(120):
            a, b = rnd.choice(vals), rnd.choice(vals)
            E("ADD", a, b, 0, mode, add(a, b, mode))
            E("MUL", a, b, 0, mode, mul(a, b, mode))
            acc = rnd.choice(vals)
            E("MADD", acc, a, b, mode, madd(acc, a, b, mode))
        for v in [16777217, 0xFFFFFFFF, 0x80000001, 33554435, 7]:
            E("FLOAT", v, 0, 0, mode, from_int(v, True, 0, mode))
            E("UFLOAT", v, 0, 0, mode, from_int(v, False, 0, mode))
    for a in vals:
        E("SQRT", a, 0, 0, RNE, sqrt(a))
        for scale in (0, 3, 15):
            E("TRUNC", a, scale, 0, RNE, to_int(a, scale, "trunc"))
            E("ROUND", a, scale, 0, RNE, to_int(a, scale, "round"))
            E("FLOOR", a, scale, 0, RNE, to_int(a, scale, "floor"))
            E("CEIL", a, scale, 0, RNE, to_int(a, scale, "ceil"))
            E("UTRUNC", a, scale, 0, RNE, utrunc(a, scale))
    ints = [0, 1, 0xFFFFFFFF, 0x7FFFFFFF, 0x80000000, 16777217, 12345, 0xFFFFFF00, 3]
    ints += [rnd.getrandbits(32) for _ in range(20)]
    for v in ints:
        for scale in (0, 1, 8, 15):
            E("FLOAT", v, scale, 0, RNE, from_int(v, True, scale))
            E("UFLOAT", v, scale, 0, RNE, from_int(v, False, scale))
    # rounding to nearest-even ties in ROUND.S
    for x in [0.5, 1.5, 2.5, -0.5, -1.5, -2.5, 3.5, 1e9 + 0.5]:
        E("ROUND", bits(x), 0, 0, RNE, to_int(bits(x), 0, "round"))

    names = ["ADD", "SUB", "MUL", "MADD", "MSUB", "DIV", "SQRT", "CMP", "TRUNC", "ROUND", "FLOOR", "CEIL", "UTRUNC",
             "FLOAT", "UFLOAT"]
    with open(OUT, "w") as f:
        f.write("/* Generated by tests/xtensa/scripts/gen_fpu_vectors.py -- do not edit. */\n")
        f.write("enum { " + ", ".join(f"OP_{n}" for n in names) + " };\n")
        f.write("#define NAN_ANY 0xffffffffu\n")
        f.write("struct fvec { uint8_t op, mode; uint32_t a, b, c, expect; };\n")
        f.write(f"static const struct fvec fvecs[{len(ops)}] = {{\n")
        for n, a, b, c, mode, e in ops:
            f.write(f"  {{OP_{n}, {mode}, 0x{a:08x}u, 0x{b:08x}u, 0x{c:08x}u, 0x{e:08x}u}},\n")
        f.write("};\n")
    print(f"{len(ops)} vectors -> {OUT}")

main()
