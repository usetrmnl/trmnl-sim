#include "rt.h"

exc_fn rt_exc_hook;
int_fn rt_int_hook;
volatile uint32_t rt_level4_count;
volatile uint32_t rt_level5_count[2];
volatile uint32_t rt_nmi_count[2];
volatile uint32_t rt_double[4];
volatile uint32_t rt_hi_scratch[8];

void rt_fail(uint32_t line) {
    MMIO_EXIT = line ? line : 0xffff;
    for (;;) {}
}

void rt_fail_eq(uint32_t line, uint32_t actual, uint32_t expected) {
    MMIO_ACTUAL = actual;
    MMIO_EXPECTED = expected;
    rt_fail(line);
}

void rt_puts(const char *s) {
    while (*s) MMIO_PUTC = (uint8_t)*s++;
}

/* Level-1 exception or interrupt (EXCCAUSE 4). */
void rt_level1(struct frame *f) {
    if (f->cause == 4) {
        if (!rt_int_hook) rt_fail(0xE004);
        rt_int_hook(1, f);
    } else {
        if (!rt_exc_hook) rt_fail(0xE000 + f->cause);
        rt_exc_hook(f);
    }
}

void rt_interrupt(struct frame *f) {
    if (!rt_int_hook) rt_fail(0xE100 + f->level);
    rt_int_hook(f->level, f);
}

void *memset(void *d, int c, unsigned n) {
    unsigned char *p = d;
    while (n--) *p++ = (unsigned char)c;
    return d;
}

void *memcpy(void *d, const void *s, unsigned n) {
    unsigned char *p = d;
    const unsigned char *q = s;
    while (n--) *p++ = *q++;
    return d;
}
