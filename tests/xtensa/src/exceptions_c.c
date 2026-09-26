/* Level-1 exceptions dispatched to the guest: load/store/fetch faults, illegal
   instructions, SYSCALL, double exception, lazy coprocessor enable (FPU = CP0,
   cop_ai = CP3) as FreeRTOS does it. */
#include "rt.h"

uint32_t double_exc(uint32_t addr);
uint32_t bad_retw(void);
uint32_t bad_sr(void);
void ill_n(void);
void do_syscall(void);
uint32_t fp_touch(uint32_t x);
void qr_copy(void *dst, const void *src);
uint32_t cp3_regs(void);

static volatile uint32_t n_exc, last_cause, last_pc, last_vaddr, skip;
static volatile uint32_t cp_enables;

static void hook(struct frame *f) {
    n_exc++;
    last_cause = f->cause;
    last_pc = f->pc;
    last_vaddr = f->vaddr;
    switch (f->cause) {
    case 32: /* Coprocessor0Disabled: enable and retry */
    case 35: /* Coprocessor3Disabled */
        WSR(cpenable, RSR(cpenable) | (1 << (f->cause - 32)));
        cp_enables++;
        return;
    case 20: /* fetch fault after callx8 to a bad address: return to the caller */
        f->pc = (f->a[8] & 0x3fffffff) | 0x40000000;
        f->a[10] = 0xbad;
        return;
    }
    f->pc += skip;
}

typedef uint32_t (*fn)(void);

int main(void) {
    rt_exc_hook = hook;
    volatile uint32_t *bad = (volatile uint32_t *)FAULT_ADDR;

    skip = 3;
    uint32_t v = 0;
    __asm__ volatile("_l32i %0, %1, 0" : "=a"(v) : "a"(bad));
    CHECK_EQ(n_exc, 1);
    CHECK_EQ(last_cause, 28); /* LoadProhibited */
    CHECK_EQ(last_vaddr, FAULT_ADDR);
    __asm__ volatile("_s32i %0, %1, 4" :: "a"(1), "a"(bad) : "memory");
    CHECK_EQ(last_cause, 29); /* StoreProhibited */
    CHECK_EQ(last_vaddr, FAULT_ADDR + 4);
    __asm__ volatile("l8ui %0, %1, 3" : "=a"(v) : "a"(bad));
    CHECK_EQ(last_cause, 28);
    CHECK_EQ(last_vaddr, FAULT_ADDR + 3);
    CHECK_EQ(n_exc, 3);

    /* instruction fetch from nowhere */
    fn volatile f = (fn)0x70001000;
    CHECK_EQ(f(), 0xbad);
    CHECK_EQ(last_cause, 20);
    CHECK_EQ(last_pc, 0x70001000);
    CHECK_EQ(last_vaddr, 0x70001000);

    /* illegal instructions */
    uint32_t pc_ill;
    __asm__ volatile("movi %0, 1f\n 1: _ill" : "=a"(pc_ill));
    CHECK_EQ(last_cause, 0);
    CHECK_EQ(last_pc, pc_ill);
    skip = 2;
    ill_n();
    CHECK_EQ(last_cause, 0);
    skip = 3;
    CHECK_EQ(bad_retw(), 42);
    CHECK_EQ(last_cause, 0);
    CHECK_EQ(bad_sr(), 7);
    CHECK_EQ(last_cause, 0);
    do_syscall();
    CHECK_EQ(last_cause, 1);
    uint32_t before = n_exc;

    /* double exception */
    uint32_t dpc = double_exc(FAULT_ADDR);
    CHECK_EQ(rt_double[2], 1);
    CHECK_EQ(rt_double[0], dpc);
    CHECK_EQ(rt_double[1], 28);
    CHECK_EQ(n_exc, before); /* did not go through the level-1 path */

    /* lazy FPU enable */
    WSR(cpenable, 0);
    CHECK_EQ(fp_touch(0x3f800000), 0x40000000);
    CHECK_EQ(cp_enables, 1);
    CHECK_EQ(last_cause, 32);
    CHECK_EQ(fp_touch(0x40000000), 0x40800000); /* no further exception */
    CHECK_EQ(cp_enables, 1);
    WSR(cpenable, 0);
    CHECK_EQ(({ uint32_t r; __asm__ volatile("rur %0, fcr" : "=a"(r)); r; }), 0);
    CHECK_EQ(cp_enables, 2);

    /* cop_ai (CP3) */
    static uint32_t src[12] __attribute__((aligned(16))), dst[12] __attribute__((aligned(16)));
    for (int i = 0; i < 12; i++) src[i] = 0x11111111u * i;
    WSR(cpenable, 1);
    qr_copy(&dst[4], &src[0]);
    CHECK_EQ(last_cause, 35);
    CHECK_EQ(cp_enables, 3);
    for (int i = 0; i < 4; i++) CHECK_EQ(dst[i], src[4 + i]);
    CHECK_EQ(cp3_regs(), 0xff + 0xf + 0xf + 2 * 0x12345678);
    return 0;
}
