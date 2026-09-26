/* GuestCpu HLE glue: the host intercepts hooked*() at their first instruction (before
   ENTRY), calls back into guest functions with begin_call(), and returns with
   return_from_hook() / set_arg+set_pc. See guest_hle in tests.rs for the host side. */
#include "rt.h"

uint32_t __attribute__((noinline)) add7(uint32_t a, uint32_t b, uint32_t c, uint32_t d, uint32_t e, uint32_t f,
                                        uint32_t g) {
    return a + 3 * b + 5 * c + 7 * d + 11 * e + 13 * f + 17 * g;
}
/* real recursion (xor defeats GCC's accumulator/tail-call transformations) */
uint32_t __attribute__((noipa)) rsum(uint32_t n) { return n == 0 ? 0 : (n * n) ^ (rsum(n - 1) + n); }

/* bodies are never executed: the host intercepts the entry */
uint32_t __attribute__((noipa)) hooked(uint32_t a, uint32_t b, uint32_t c) { MMIO_EXIT = __LINE__; return a + b + c; }
uint32_t __attribute__((noipa)) hooked2(uint32_t x) { MMIO_EXIT = __LINE__; return x; }
uint32_t __attribute__((noipa)) hooked3(uint32_t n) { MMIO_EXIT = __LINE__; return n; }

static uint32_t expect_hooked(uint32_t a, uint32_t b, uint32_t c) {
    return add7(a, b, c, a * 2, b * 2, c * 2, a + b + c) ^ rsum((a & 31) + 10);
}

/* recursion with live values in all registers, calling the hook at the bottom so the
   window ring is full of live frames when the host takes over */
static uint32_t __attribute__((noipa)) deep(uint32_t n, uint32_t k1, uint32_t k2, uint32_t k3) {
    volatile uint32_t v1 = k1 * 3 + n, v2 = k2 ^ (n << 4), v3 = k3 + 77;
    uint32_t r;
    if (n == 0) {
        r = hooked(k1, k2, k3);
        CHECK_EQ(r, expect_hooked(k1, k2, k3));
    } else {
        r = deep(n - 1, v2, v3, v1);
    }
    CHECK_EQ(v1, k1 * 3 + n);
    CHECK_EQ(v2, k2 ^ (n << 4));
    CHECK_EQ(v3, k3 + 77);
    return (r ^ v1) + v2;
}

static volatile uint32_t ticks;
static void tick(int level, struct frame *f) {
    (void)level; (void)f;
    WSR(ccompare0, RSR(ccount) + 173);
    ticks++;
}

int main(void) {
    WSR(cpenable, 1);
    rt_int_hook = tick;
    WSR(intenable, 1 << 6);
    WSR(ccompare0, RSR(ccount) + 173);

    CHECK_EQ(hooked(1, 2, 3), expect_hooked(1, 2, 3));
    CHECK_EQ(hooked2(40), 2 * rsum(40));
    CHECK_EQ(hooked3(50), 50 * 51 / 2); /* 50 chained begin_calls from one hook */
    for (uint32_t n = 0; n < 24; n++) deep(n, n + 1, n + 2, n + 3);
    /* function pointer (CALLX8) into a hook */
    uint32_t (*volatile hp)(uint32_t) = hooked2;
    CHECK_EQ(hp(7), 2 * rsum(7));
    WSR(intenable, 0);
    CHECK(ticks > 20);
    return 0;
}
