/* Test runtime interface (see crt0.S). */
#pragma once
#include <stdint.h>

#define MMIO(off) (*(volatile uint32_t *)(0x60000000u + (off)))
#define MMIO_EXIT MMIO(0x0)
#define MMIO_PUTC MMIO(0x4)
#define MMIO_IRQ_LINES MMIO(0x8)
#define MMIO_ACTUAL MMIO(0x10)
#define MMIO_EXPECTED MMIO(0x14)
/* Any access in [0x70000000, 0x80000000) faults (no device there). */
#define FAULT_ADDR 0x70000010u

struct frame {
    uint32_t pc, ps, sar, cause, vaddr, lbeg, lend, lcount;
    uint32_t a[16];
    uint32_t level, br, pad[2];
};

typedef void (*exc_fn)(struct frame *);
typedef void (*int_fn)(int level, struct frame *);
extern exc_fn rt_exc_hook;
extern int_fn rt_int_hook;
extern volatile uint32_t rt_level4_count;
extern volatile uint32_t rt_level5_count[2];
extern volatile uint32_t rt_nmi_count[2];
extern volatile uint32_t rt_double[4];

__attribute__((noreturn)) void rt_fail(uint32_t line);
__attribute__((noreturn)) void rt_fail_eq(uint32_t line, uint32_t actual, uint32_t expected);
void rt_puts(const char *s);

#define CHECK(c) do { if (!(c)) rt_fail(__LINE__); } while (0)
#define CHECK_EQ(a, b) do { uint32_t _a = (uint32_t)(a), _b = (uint32_t)(b); \
    if (_a != _b) rt_fail_eq(__LINE__, _a, _b); } while (0)

#define RSR(sr) ({ uint32_t _v; __asm__ volatile("rsr %0, " #sr : "=a"(_v)); _v; })
#define WSR(sr, v) __asm__ volatile("wsr %0, " #sr "\n rsync" :: "a"((uint32_t)(v)) : "memory")
#define RSIL(level) ({ uint32_t _v; __asm__ volatile("rsil %0, " #level : "=a"(_v) :: "memory"); _v; })

#define PS_UM 0x20
#define PS_EXCM 0x10
#define PS_WOE 0x40000
