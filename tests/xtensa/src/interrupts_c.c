/* Interrupts: software (INTSET), timers (CCOMPARE0/1/2), external level/edge lines and
   the NMI via the harness, levels and masking (INTENABLE, PS.INTLEVEL, PS.EXCM),
   nesting, RFI, WAITI, and hardware loops being interrupted. */
#include "rt.h"

uint32_t excm_test(void);
uint32_t hw_loop(uint32_t n);
void do_waiti(void);

static const uint32_t level_mask[8] = {0, 0x000637FF, 0x00380000, 0x28C08800, 0x53000000, 0x84010000, 0, 0x4000};

static volatile uint32_t count[32];
static volatile uint32_t order[32], norder;
static volatile uint32_t nest_test, nest_seen, rearm_period;
static volatile uint32_t level_ps[8];

static void handler(int level, struct frame *f) {
    (void)f;
    level_ps[level] = RSR(ps);
    for (;;) {
        uint32_t pend = RSR(interrupt) & RSR(intenable) & level_mask[level];
        if (!pend) break;
        int b = __builtin_ctz(pend);
        count[b]++;
        if (norder < 32) order[norder++] = b;
        switch (b) {
        case 6:
            if (rearm_period) WSR(ccompare0, RSR(ccount) + rearm_period);
            else WSR(ccompare0, RSR(ccount) - 1); /* far away; clears int 6 */
            break;
        case 15: WSR(ccompare1, RSR(ccount) - 1); break;
        case 7:
            WSR(intclear, 1 << 7);
            if (nest_test) {
                WSR(intset, 1 << 29); /* level 3 preempts this level-1 handler */
                for (volatile int i = 0; i < 10; i++) {}
                nest_seen = count[29];
            }
            break;
        case 29: WSR(intclear, 1 << 29); break;
        case 10: WSR(intclear, 1 << 10); break;
        case 0: MMIO_IRQ_LINES = 0; for (volatile int i = 0; i < 2; i++) {} break;
        case 19: MMIO_IRQ_LINES = 0; for (volatile int i = 0; i < 2; i++) {} break;
        default: rt_fail(0x1000 + b);
        }
    }
}

static void spin(int n) { for (volatile int i = 0; i < n; i++) {} }

int main(void) {
    rt_int_hook = handler;
    WSR(ccompare0, RSR(ccount) - 1);
    WSR(ccompare1, RSR(ccount) - 1);
    WSR(ccompare2, RSR(ccount) - 1);
    WSR(intenable, (1 << 6) | (1 << 7) | (1 << 29) | (1 << 15) | (1 << 16) | (1 << 0) | (1 << 10) | (1 << 19));

    /* software interrupt, taken immediately */
    WSR(intset, 1 << 7);
    CHECK_EQ(count[7], 1);
    CHECK_EQ(RSR(interrupt) & (1 << 7), 0);
    CHECK_EQ(level_ps[1] & 0xf, 1); /* handler ran at INTLEVEL 1 (set by the runtime) */

    /* masked by PS.INTLEVEL */
    uint32_t old = RSIL(1);
    WSR(intset, 1 << 7);
    spin(10);
    CHECK_EQ(count[7], 1);
    CHECK(RSR(interrupt) & (1 << 7));
    WSR(ps, old);
    CHECK_EQ(count[7], 2);

    /* masked by INTENABLE */
    WSR(intenable, RSR(intenable) & ~(1 << 7));
    WSR(intset, 1 << 7);
    spin(10);
    CHECK_EQ(count[7], 2);
    WSR(intenable, RSR(intenable) | (1 << 7));
    CHECK_EQ(count[7], 3);

    /* INTSET only sets software interrupts; INTCLEAR does not clear level lines */
    WSR(intset, 1 << 0);
    CHECK_EQ(RSR(interrupt) & 1, 0);

    /* level 3 but INTLEVEL 2 -> taken; INTLEVEL 3 -> masked */
    old = RSIL(2);
    WSR(intset, 1 << 29);
    CHECK_EQ(count[29], 1);
    CHECK_EQ(level_ps[3] & 0xf, 3);
    RSIL(3);
    WSR(intset, 1 << 29);
    spin(5);
    CHECK_EQ(count[29], 1);
    WSR(ps, old);
    CHECK_EQ(count[29], 2);

    /* nesting: level 3 preempts the level-1 handler */
    nest_test = 1;
    norder = 0;
    WSR(intset, 1 << 7);
    nest_test = 0;
    CHECK_EQ(nest_seen, 3);
    CHECK_EQ(norder, 2);
    CHECK_EQ(order[0], 7);
    CHECK_EQ(order[1], 29);

    /* CCOMPARE0 timer (level 1) */
    uint32_t c6 = count[6];
    WSR(ccompare0, RSR(ccount) + 500);
    for (int i = 0; i < 100000 && count[6] == c6; i++) {}
    CHECK_EQ(count[6], c6 + 1);
    /* CCOMPARE1 timer (level 3) */
    WSR(ccompare1, RSR(ccount) + 300);
    for (int i = 0; i < 100000 && count[15] == 0; i++) {}
    CHECK_EQ(count[15], 1);

    /* PS.EXCM masks levels <= 3 but not level 5 (CCOMPARE2, int 16) */
    uint32_t sampled = excm_test();
    CHECK_EQ(sampled & ((1 << 7) | (1 << 29)), (1 << 7) | (1 << 29));
    CHECK_EQ(rt_level5_count[0], 1);
    CHECK(rt_level5_count[1] & PS_EXCM); /* EPS5 captured EXCM=1 */
    CHECK_EQ(count[7], 5);
    CHECK_EQ(count[29], 4);

    /* external level-triggered line 0 (level 1) and line 19 (level 2) */
    MMIO_IRQ_LINES = 1 << 0;
    spin(5);
    CHECK_EQ(count[0], 1);
    MMIO_IRQ_LINES = 1 << 19;
    spin(5);
    CHECK_EQ(count[19], 1);
    CHECK_EQ(level_ps[2] & 0xf, 2);
    /* edge-triggered line 10 latches on the rising edge */
    old = RSIL(1);
    MMIO_IRQ_LINES = 1 << 10;
    MMIO_IRQ_LINES = 0;
    spin(3);
    CHECK(RSR(interrupt) & (1 << 10));
    CHECK_EQ(count[10], 0);
    WSR(ps, old);
    CHECK_EQ(count[10], 1);

    /* NMI (line 14, level 7) is taken even at INTLEVEL 15 */
    old = RSIL(15);
    MMIO_IRQ_LINES = 1 << 14;
    MMIO_IRQ_LINES = 0;
    spin(3);
    CHECK_EQ(rt_nmi_count[0], 1);
    CHECK_EQ(rt_nmi_count[1] & 0xf, 15); /* EPS7.INTLEVEL */
    WSR(ps, old);

    /* WAITI: sleep until the timer fires */
    c6 = count[6];
    WSR(ccompare0, RSR(ccount) + 5000);
    do_waiti();
    CHECK_EQ(count[6], c6 + 1);

    /* hardware loop interrupted by a periodic timer: iteration count must be exact */
    c6 = count[6];
    rearm_period = 97;
    WSR(ccompare0, RSR(ccount) + 97);
    uint32_t n = hw_loop(20000);
    rearm_period = 0;
    WSR(ccompare0, RSR(ccount) - 1);
    CHECK_EQ(n, 20000);
    CHECK(count[6] - c6 > 100);
    return 0;
}
