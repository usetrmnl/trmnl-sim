/* S32C1I / SCOMPARE1: compare-and-swap, GCC __atomic builtins, and atomic increments
   racing with an interrupt handler that does the same. */
#include "rt.h"

static inline uint32_t cas(volatile uint32_t *p, uint32_t expect, uint32_t new) {
    __asm__ volatile("wsr %2, scompare1\n s32c1i %0, %1, 0" : "+a"(new) : "a"(p), "a"(expect) : "memory");
    return new; /* old value */
}

static volatile uint32_t counter, irq_adds;

static void handler(int level, struct frame *f) {
    (void)level; (void)f;
    WSR(ccompare0, RSR(ccount) + 211);
    __atomic_fetch_add(&counter, 1, __ATOMIC_SEQ_CST);
    irq_adds++;
}

int main(void) {
    volatile uint32_t x = 5;
    CHECK_EQ(cas(&x, 5, 9), 5); /* success: returns old, stores new */
    CHECK_EQ(x, 9);
    CHECK_EQ(cas(&x, 5, 7), 9); /* failure: returns current, no store */
    CHECK_EQ(x, 9);
    CHECK_EQ(RSR(scompare1), 5);

    uint32_t e = 9;
    CHECK(__atomic_compare_exchange_n(&x, &e, 11, 0, __ATOMIC_SEQ_CST, __ATOMIC_SEQ_CST));
    CHECK_EQ(x, 11);
    e = 1;
    CHECK(!__atomic_compare_exchange_n(&x, &e, 12, 0, __ATOMIC_SEQ_CST, __ATOMIC_SEQ_CST));
    CHECK_EQ(e, 11);
    CHECK_EQ(__atomic_fetch_add(&x, 4, __ATOMIC_SEQ_CST), 11);
    CHECK_EQ(__atomic_exchange_n(&x, 100, __ATOMIC_SEQ_CST), 15);
    CHECK_EQ(__atomic_fetch_or(&x, 3, __ATOMIC_SEQ_CST), 100);
    CHECK_EQ(x, 103);
    /* byte/halfword atomics are synthesised from word-sized s32c1i */
    static volatile uint8_t b[4] __attribute__((aligned(4))) = {1, 2, 3, 4};
    CHECK_EQ(__atomic_fetch_add(&b[2], 10, __ATOMIC_SEQ_CST), 3);
    CHECK_EQ(b[2], 13);
    CHECK_EQ(b[3], 4);

    /* FreeRTOS-style spinlock: owner = cpu id via CAS against "free" (0) */
    volatile uint32_t lock = 0;
    CHECK_EQ(cas(&lock, 0, 0xcdcd), 0);
    CHECK_EQ(cas(&lock, 0, 0xabab), 0xcdcd);
    CHECK_EQ(cas(&lock, 0xcdcd, 0), 0xcdcd);
    CHECK_EQ(lock, 0);

    /* race with an interrupt handler */
    rt_int_hook = handler;
    WSR(intenable, 1 << 6);
    WSR(ccompare0, RSR(ccount) + 211);
    for (int i = 0; i < 20000; i++) __atomic_fetch_add(&counter, 1, __ATOMIC_SEQ_CST);
    WSR(intenable, 0);
    CHECK(irq_adds > 50);
    CHECK_EQ(counter, 20000 + irq_adds);
    return 0;
}
