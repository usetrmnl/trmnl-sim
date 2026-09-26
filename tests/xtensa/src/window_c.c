/* Window overflow/underflow stress: deep recursion with CALL4/8/12 mixes, alloca
   (MOVSP / alloca exception), function pointers (CALLX). */
#include "rt.h"

uint32_t r4(uint32_t), r8(uint32_t), r12(uint32_t);
uint32_t callx4_fn(uint32_t (*f)(uint32_t), uint32_t n);
uint32_t callx12_fn(uint32_t (*f)(uint32_t), uint32_t n);
uint32_t rotw_test(void);

static uint32_t f8(uint32_t n);
static uint32_t f12(uint32_t n);
static uint32_t f4(uint32_t n) { return n == 0 ? 1 : 3 * f8(n - 1) + ((n << 2) ^ n); }
static uint32_t f8(uint32_t n) {
    if (n == 0) return 1;
    uint32_t a4 = n + 1, a5 = n << 3, a6 = a4 ^ a5, a7 = a6 + n;
    return 3 * f12(n - 1) + a4 + a5 + a6 + a7;
}
static uint32_t f12(uint32_t n) {
    if (n == 0) return 1;
    uint32_t s = 0;
    for (uint32_t k = 3; k <= 11; k++) s += n + k;
    return 3 * f4(n - 1) + s;
}

static uint32_t __attribute__((noinline)) fib(uint32_t n) { return n < 2 ? n : fib(n - 1) + fib(n - 2); }

/* many live values across calls: forces a8-a15 use in every frame */
static uint32_t __attribute__((noinline)) heavy(uint32_t n, uint32_t a, uint32_t b, uint32_t c, uint32_t d,
                                                 uint32_t e) {
    if (n == 0) return a ^ b ^ c ^ d ^ e;
    uint32_t x = heavy(n - 1, b + 1, c * 3, d ^ a, e + n, a - 7);
    return x + a * 5 + b * 7 + c * 11 + d * 13 + e * 17 + n;
}
static uint32_t heavy_ref(uint32_t n, uint32_t a, uint32_t b, uint32_t c, uint32_t d, uint32_t e) {
    uint32_t acc = 0, mul = 1;
    for (;;) {
        if (n == 0) return acc + mul * (a ^ b ^ c ^ d ^ e);
        acc += mul * (a * 5 + b * 7 + c * 11 + d * 13 + e * 17 + n);
        uint32_t na = b + 1, nb = c * 3, nc = d ^ a, nd = e + n, ne = a - 7;
        a = na; b = nb; c = nc; d = nd; e = ne; n--;
    }
}

static uint32_t __attribute__((noinline)) vla(uint32_t n) {
    /* deep recursion first: spills our caller's frame, so the MOVSP that allocates the
       VLA raises an alloca exception */
    if (n % 5 == 0 && fib(12) != 144) rt_fail(__LINE__);
    volatile uint8_t buf[n + 8];
    buf[0] = (uint8_t)n;
    buf[n + 7] = (uint8_t)(n * 3);
    if (n == 0) return buf[0] + buf[7];
    return vla(n - 1) + buf[0] + buf[n + 7];
}

static volatile uint32_t allocas;
static void hook(struct frame *f) { (void)f; rt_fail(0xA110); }

static uint32_t sq(uint32_t x) { return x * x; }

int main(void) {
    rt_exc_hook = hook;
    for (uint32_t n = 0; n < 70; n += 7) {
        CHECK_EQ(r4(n), f4(n));
        CHECK_EQ(r8(n), f8(n));
        CHECK_EQ(r12(n), f12(n));
    }
    CHECK_EQ(fib(20), 6765);
    CHECK_EQ(heavy(50, 1, 2, 3, 4, 5), heavy_ref(50, 1, 2, 3, 4, 5));
    uint32_t v = 0, e = 0;
    for (uint32_t n = 0; n <= 40; n++) e += n + (uint8_t)(n * 3);
    v = vla(40);
    CHECK_EQ(v, e);
    uint32_t (*volatile fp)(uint32_t) = sq;
    CHECK_EQ(callx4_fn(fp, 9), 81);
    CHECK_EQ(callx12_fn(fp, 7), 49);
    CHECK_EQ(callx4_fn(r12, 30), f12(30));
    CHECK_EQ(callx12_fn(r8, 31), f8(31));
    CHECK_EQ(rotw_test(), 0x44);
    return 0;
}
