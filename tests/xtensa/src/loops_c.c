#include "rt.h"

uint32_t loop_n(uint32_t), loop_narrow(uint32_t), loopnez_n(uint32_t), loopgtz_n(uint32_t);
uint32_t loop_break(uint32_t, uint32_t), loop_backbranch(uint32_t), loop_nested(uint32_t, uint32_t);
uint32_t loop_regs(void);

/* compiler-generated loops (GCC emits LOOP for counted loops at -O2) */
static uint32_t __attribute__((noinline)) sum_to(uint32_t n) {
    uint32_t s = 0;
    for (uint32_t i = 0; i < n; i++) s += i * i;
    return s;
}

int main(void) {
    CHECK_EQ(loop_n(1), 1);
    CHECK_EQ(loop_n(2), 2);
    CHECK_EQ(loop_n(1000), 1000);
    CHECK_EQ(loop_narrow(7), 14);
    CHECK_EQ(loopnez_n(0), 0);
    CHECK_EQ(loopnez_n(5), 5);
    CHECK_EQ(loopgtz_n(0), 0);
    CHECK_EQ(loopgtz_n((uint32_t)-5), 0);
    CHECK_EQ(loopgtz_n(9), 9);
    CHECK_EQ(loop_break(10, 20), 10);
    CHECK_EQ(loop_break(10, 4), 1004);
    CHECK_EQ(loop_backbranch(7), 21);
    CHECK_EQ(loop_nested(6, 7), 42);
    CHECK_EQ(loop_nested(1, 1), 1);
    /* 5 iterations: LCOUNT reads 0 in the last one; body is rsr (3) + nop.n (2) bytes */
    CHECK_EQ(loop_regs(), 0 + 5);
    uint32_t e = 0;
    for (uint32_t i = 0; i < 1234; i++) e += i * i;
    CHECK_EQ(sum_to(1234), e);
    return 0;
}
