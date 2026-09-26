/* FPU: arithmetic/rounding/NaN/compare/conversion vectors computed on the host
   (scripts/gen_fpu_vectors.py), the libgcc division/sqrt sequences, moves, loads. */
#include "rt.h"
#include "fpu_vectors.h"

extern float __divsf3(float, float);
extern float __ieee754_sqrtf(float);
extern float __recipsf2(float);
extern float __rsqrtsf2(float);

typedef union { float f; uint32_t u; } fu;
static inline float F(uint32_t u) { fu x; x.u = u; return x.f; }
static inline uint32_t U(float f) { fu x; x.f = f; return x.u; }

#define FOP2(insn, a, b) ({ uint32_t _r; __asm__ volatile("wfr f1, %1\n wfr f2, %2\n " insn " f0, f1, f2\n rfr %0, f0" \
    : "=a"(_r) : "a"(a), "a"(b) : "f0", "f1", "f2"); _r; })
#define FMADD(insn, acc, a, b) ({ uint32_t _r; __asm__ volatile("wfr f0, %1\n wfr f1, %2\n wfr f2, %3\n " insn " f0, f1, f2\n rfr %0, f0" \
    : "=a"(_r) : "a"(acc), "a"(a), "a"(b) : "f0", "f1", "f2"); _r; })
#define FTOI(insn, a, sc) ({ uint32_t _r; __asm__ volatile("wfr f1, %1\n " insn " %0, f1, " #sc : "=a"(_r) : "a"(a) : "f1"); _r; })
#define ITOF(insn, a, sc) ({ uint32_t _r; __asm__ volatile(insn " f1, %1, " #sc "\n rfr %0, f1" : "=a"(_r) : "a"(a) : "f1"); _r; })

#define SCALED(macro, insn, a, sc) \
    ((sc) == 0 ? macro(insn, a, 0) : (sc) == 1 ? macro(insn, a, 1) : (sc) == 3 ? macro(insn, a, 3) : \
     (sc) == 8 ? macro(insn, a, 8) : macro(insn, a, 15))

static uint32_t cmp_all(uint32_t a, uint32_t b) {
    uint32_t r;
    __asm__ volatile(
        "wfr f1, %1\n wfr f2, %2\n movi %0, 0\n wsr %0, br\n"
        "un.s b0, f1, f2\n oeq.s b1, f1, f2\n ueq.s b2, f1, f2\n olt.s b3, f1, f2\n"
        "ult.s b4, f1, f2\n ole.s b5, f1, f2\n ule.s b6, f1, f2\n rsr %0, br"
        : "=&a"(r) : "a"(a), "a"(b) : "f1", "f2");
    return r;
}

static void set_rm(uint32_t mode) { __asm__ volatile("wur %0, fcr" :: "a"(mode)); }

static int is_nan(uint32_t v) { return (v & 0x7fffffff) > 0x7f800000; }

static uint32_t run(const struct fvec *v) {
    uint32_t a = v->a, b = v->b, c = v->c;
    switch (v->op) {
    case OP_ADD: return FOP2("add.s", a, b);
    case OP_SUB: return FOP2("sub.s", a, b);
    case OP_MUL: return FOP2("mul.s", a, b);
    case OP_MADD: return FMADD("madd.s", a, b, c);
    case OP_MSUB: return FMADD("msub.s", a, b, c);
    case OP_DIV: return U(__divsf3(F(a), F(b)));
    case OP_SQRT: return U(__ieee754_sqrtf(F(a)));
    case OP_CMP: return cmp_all(a, b);
    case OP_TRUNC: return SCALED(FTOI, "trunc.s", a, b);
    case OP_ROUND: return SCALED(FTOI, "round.s", a, b);
    case OP_FLOOR: return SCALED(FTOI, "floor.s", a, b);
    case OP_CEIL: return SCALED(FTOI, "ceil.s", a, b);
    case OP_UTRUNC: return SCALED(FTOI, "utrunc.s", a, b);
    case OP_FLOAT: return SCALED(ITOF, "float.s", a, b);
    case OP_UFLOAT: return SCALED(ITOF, "ufloat.s", a, b);
    }
    rt_fail(__LINE__);
}

static int ulp_close(uint32_t x, uint32_t y) { return x == y || x + 1 == y || y + 1 == x; }

volatile uint32_t failed_index;

int main(void) {
    WSR(cpenable, 1);
    for (unsigned i = 0; i < sizeof fvecs / sizeof fvecs[0]; i++) {
        const struct fvec *v = &fvecs[i];
        set_rm(v->mode);
        uint32_t r = run(v);
        set_rm(0);
        int ok = v->expect == NAN_ANY ? is_nan(r) : r == v->expect;
        if (!ok) {
            failed_index = i;
            MMIO(0x18) = i;
            MMIO(0x1c) = v->op;
            rt_fail_eq(__LINE__, r, v->expect);
        }
    }
    /* reciprocal / rsqrt helper sequences: within 1 ulp */
    CHECK(ulp_close(U(__recipsf2(3.0f)), U(1.0f / 3.0f)));
    CHECK_EQ(U(__recipsf2(4.0f)), U(0.25f));
    CHECK(ulp_close(U(__rsqrtsf2(2.0f)), 0x3f3504f3));
    CHECK_EQ(U(__rsqrtsf2(4.0f)), U(0.5f));
    /* compiler-generated float code */
    volatile float x = 1.5f, y = -2.25f;
    CHECK_EQ(U(x * y + x), U(-1.875f));
    CHECK_EQ((int)(x * 10.0f), 15);
    CHECK_EQ(U((float)(volatile int){-7}), U(-7.0f));
    CHECK(x > y && !(x < y) && x != y);
    /* moves, sign ops, constants */
    uint32_t r;
    __asm__ volatile("wfr f3, %1\n neg.s f4, f3\n abs.s f5, f4\n mov.s f6, f5\n rfr %0, f6" : "=a"(r) : "a"(0xbfc00000u)
                     : "f3", "f4", "f5", "f6");
    CHECK_EQ(r, 0x3fc00000);
    __asm__ volatile("wfr f3, %1\n neg.s f4, f3\n rfr %0, f4" : "=a"(r) : "a"(0x7fc00001u) : "f3", "f4");
    CHECK_EQ(r, 0xffc00001); /* sign ops do not touch NaN payloads */
    __asm__ volatile("const.s f7, 0\n const.s f8, 1\n const.s f9, 2\n const.s f10, 3\n"
                     "add.s f7, f7, f8\n add.s f7, f7, f9\n add.s f7, f7, f10\n rfr %0, f7" : "=a"(r) :: "f7", "f8", "f9", "f10");
    CHECK_EQ(r, U(3.5f));
#define FMOV(insn, init, src, cond) ({ uint32_t _r; __asm__ volatile("wfr f1, %1\n wfr f2, %2\n " insn " f1, f2, %3\n rfr %0, f1" \
        : "=a"(_r) : "a"(init), "a"(src), "a"(cond) : "f1", "f2"); _r; })
    CHECK_EQ(FMOV("moveqz.s", 1, 2, 0), 2);
    CHECK_EQ(FMOV("moveqz.s", 1, 2, 5), 1);
    CHECK_EQ(FMOV("movnez.s", 1, 2, 5), 2);
    CHECK_EQ(FMOV("movltz.s", 1, 2, -1), 2);
    CHECK_EQ(FMOV("movltz.s", 1, 2, 0), 1);
    CHECK_EQ(FMOV("movgez.s", 1, 2, 0), 2);
    WSR(br, 1 << 5);
    CHECK_EQ(({ uint32_t _r; __asm__ volatile("wfr f1, %1\n wfr f2, %2\n movt.s f1, f2, b5\n rfr %0, f1" : "=a"(_r)
                                               : "a"(1), "a"(2) : "f1", "f2"); _r; }), 2);
    CHECK_EQ(({ uint32_t _r; __asm__ volatile("wfr f1, %1\n wfr f2, %2\n movf.s f1, f2, b5\n rfr %0, f1" : "=a"(_r)
                                               : "a"(1), "a"(2) : "f1", "f2"); _r; }), 1);
    /* loads and stores */
    static volatile uint32_t mem[4] = {0x3f800000, 0x40000000, 0x40400000, 0};
    uint32_t p = (uint32_t)&mem[0], p2;
    __asm__ volatile("lsi f1, %2, 4\n lsip f2, %1, 8\n ssi f1, %2, 12\n" : "=a"(p2), "+a"(p) : "a"(&mem[0]) : "f1", "f2", "memory");
    CHECK_EQ(p, (uint32_t)&mem[2]);
    CHECK_EQ(mem[3], 0x40000000);
    p = (uint32_t)&mem[0];
    __asm__ volatile("lsx f1, %0, %1\n lsxp f2, %0, %1\n ssx f2, %0, %1\n ssip f1, %0, 4" : "+a"(p) : "a"(4) : "f1", "f2", "memory");
    CHECK_EQ(p, (uint32_t)&mem[2]);
    CHECK_EQ(mem[2], 0x40000000);
    CHECK_EQ(mem[1], 0x40000000);
    p = (uint32_t)&mem[0];
    __asm__ volatile("wfr f3, %1\n ssxp f3, %0, %2" : "+a"(p) : "a"(0x12345678), "a"(12) : "f3", "memory");
    CHECK_EQ(mem[3], 0x12345678);
    CHECK_EQ(p, (uint32_t)&mem[3]);
    /* FCR/FSR */
    __asm__ volatile("wur %1, fcr\n rur %0, fcr" : "=a"(r) : "a"(0xffffffff));
    CHECK_EQ(r, 0x7f);
    __asm__ volatile("wur %1, fsr\n rur %0, fsr" : "=a"(r) : "a"(0xffffffff));
    CHECK_EQ(r, 0xf80);
    set_rm(0);
    __asm__ volatile("wur %0, fsr" :: "a"(0));
    return 0;
}
