/* Integer instructions and edge cases, each forced through inline asm. */
#include "rt.h"

#define OP2(insn, a, b) ({ uint32_t _r; __asm__ volatile(insn " %0, %1, %2" : "=a"(_r) : "a"(a), "a"(b)); _r; })
#define OP1(insn, a) ({ uint32_t _r; __asm__ volatile(insn " %0, %1" : "=a"(_r) : "a"(a)); _r; })
#define OPI(insn, a, i) ({ uint32_t _r; __asm__ volatile(insn " %0, %1, " #i : "=a"(_r) : "a"(a)); _r; })

static uint32_t sll_sar(uint32_t v, uint32_t sar) {
    uint32_t r;
    __asm__ volatile("wsr %2, sar\n sll %0, %1" : "=a"(r) : "a"(v), "a"(sar));
    return r;
}
static uint32_t srl_sar(uint32_t v, uint32_t sar) {
    uint32_t r;
    __asm__ volatile("wsr %2, sar\n srl %0, %1" : "=a"(r) : "a"(v), "a"(sar));
    return r;
}
static uint32_t sra_sar(uint32_t v, uint32_t sar) {
    uint32_t r;
    __asm__ volatile("wsr %2, sar\n sra %0, %1" : "=a"(r) : "a"(v), "a"(sar));
    return r;
}
static uint32_t src_sar(uint32_t hi, uint32_t lo, uint32_t sar) {
    uint32_t r;
    __asm__ volatile("wsr %3, sar\n src %0, %1, %2" : "=a"(r) : "a"(hi), "a"(lo), "a"(sar));
    return r;
}
static uint32_t rsar(void) { return RSR(sar); }

static volatile uint32_t div_faults, last_cause, last_pc;
static void div_hook(struct frame *f) {
    last_cause = f->cause;
    last_pc = f->pc;
    div_faults++;
    f->pc += 3; /* skip the quos/rem */
}

int main(void) {
    /* add/sub/addx/subx */
    CHECK_EQ(OP2("add", 0xffffffff, 2), 1);
    CHECK_EQ(OP2("addx2", 3, 1), 7);
    CHECK_EQ(OP2("addx4", 3, 1), 13);
    CHECK_EQ(OP2("addx8", 0x20000000, 5), 5);
    CHECK_EQ(OP2("sub", 1, 2), 0xffffffff);
    CHECK_EQ(OP2("subx2", 3, 1), 5);
    CHECK_EQ(OP2("subx4", 3, 1), 11);
    CHECK_EQ(OP2("subx8", 3, 1), 23);
    CHECK_EQ(OP1("neg", 5), (uint32_t)-5);
    CHECK_EQ(OP1("neg", 0x80000000), 0x80000000);
    CHECK_EQ(OP1("abs", (uint32_t)-7), 7);
    CHECK_EQ(OP1("abs", 0x80000000), 0x80000000);
    /* logic */
    CHECK_EQ(OP2("and", 0xf0f0, 0xff00), 0xf000);
    CHECK_EQ(OP2("or", 0xf0f0, 0xff00), 0xfff0);
    CHECK_EQ(OP2("xor", 0xf0f0, 0xff00), 0x0ff0);
    /* min/max */
    CHECK_EQ(OP2("min", (uint32_t)-1, 1), (uint32_t)-1);
    CHECK_EQ(OP2("max", (uint32_t)-1, 1), 1);
    CHECK_EQ(OP2("minu", (uint32_t)-1, 1), 1);
    CHECK_EQ(OP2("maxu", (uint32_t)-1, 1), (uint32_t)-1);
    CHECK_EQ(OP2("min", 0x80000000, 0x7fffffff), 0x80000000);
    CHECK_EQ(OP2("salt", (uint32_t)-1, 0), 1);
    CHECK_EQ(OP2("salt", 0, (uint32_t)-1), 0);
    CHECK_EQ(OP2("saltu", (uint32_t)-1, 0), 0);
    CHECK_EQ(OP2("saltu", 0, (uint32_t)-1), 1);
    /* sext / clamps */
    CHECK_EQ(OPI("sext", 0x80, 7), 0xffffff80);
    CHECK_EQ(OPI("sext", 0x7f, 7), 0x7f);
    CHECK_EQ(OPI("sext", 0x12348000, 15), 0xffff8000);
    CHECK_EQ(OPI("sext", 0x00400000, 22), 0xffc00000);
    CHECK_EQ(OPI("clamps", 200, 7), 127);
    CHECK_EQ(OPI("clamps", (uint32_t)-200, 7), (uint32_t)-128);
    CHECK_EQ(OPI("clamps", 100, 7), 100);
    CHECK_EQ(OPI("clamps", 0x7fffffff, 15), 32767);
    CHECK_EQ(OPI("clamps", 0x80000000, 22), (uint32_t)-(1 << 22));
    /* nsa / nsau */
    CHECK_EQ(OP1("nsa", 0), 31);
    CHECK_EQ(OP1("nsa", 0xffffffff), 31);
    CHECK_EQ(OP1("nsa", 1), 30);
    CHECK_EQ(OP1("nsa", 0x40000000), 0);
    CHECK_EQ(OP1("nsa", 0x80000000), 0);
    CHECK_EQ(OP1("nsa", 0xc0000000), 1);
    CHECK_EQ(OP1("nsa", 0xfffffffe), 30);
    CHECK_EQ(OP1("nsau", 0), 32);
    CHECK_EQ(OP1("nsau", 1), 31);
    CHECK_EQ(OP1("nsau", 0x80000000), 0);
    CHECK_EQ(OP1("nsau", 0x00010000), 15);
    /* shifts with immediate */
    CHECK_EQ(OPI("slli", 1, 31), 0x80000000);
    CHECK_EQ(OPI("slli", 3, 1), 6);
    CHECK_EQ(OPI("srli", 0x80000000, 15), 0x10000);
    CHECK_EQ(OPI("srai", 0x80000000, 31), 0xffffffff);
    CHECK_EQ(OPI("srai", 0x40000000, 30), 1);
    CHECK_EQ(OPI("srai", 0x80000000, 0), 0x80000000);
    CHECK_EQ(({ uint32_t r; __asm__ volatile("extui %0, %1, 17, 5" : "=a"(r) : "a"(0xfffe0000u)); r; }), 31);
    CHECK_EQ(({ uint32_t r; __asm__ volatile("extui %0, %1, 4, 16" : "=a"(r) : "a"(0x12345678u)); r; }), 0x4567);
    CHECK_EQ(({ uint32_t r; __asm__ volatile("extui %0, %1, 0, 1" : "=a"(r) : "a"(3u)); r; }), 1);
    /* SAR-based shifts, including SAR = 0 and 32 */
    CHECK_EQ(sll_sar(0x12345678, 32), 0x12345678); /* ssl 0 -> shift by 0 */
    CHECK_EQ(sll_sar(0x12345678, 0), 0);           /* shift by 32 */
    CHECK_EQ(sll_sar(0x12345678, 28), 0x23456780);
    CHECK_EQ(srl_sar(0x80000000, 0), 0x80000000);
    CHECK_EQ(srl_sar(0x80000000, 32), 0);
    CHECK_EQ(srl_sar(0x80000000, 31), 1);
    CHECK_EQ(sra_sar(0x80000000, 32), 0xffffffff);
    CHECK_EQ(sra_sar(0x80000000, 0), 0x80000000);
    CHECK_EQ(sra_sar(0x80000000, 4), 0xf8000000);
    CHECK_EQ(src_sar(0x11111111, 0x22222222, 0), 0x22222222);
    CHECK_EQ(src_sar(0x11111111, 0x22222222, 32), 0x11111111);
    CHECK_EQ(src_sar(0x12345678, 0x9abcdef0, 8), 0x789abcde);
    /* SAR setup instructions */
    __asm__ volatile("ssl %0" :: "a"(0)); CHECK_EQ(rsar(), 32);
    __asm__ volatile("ssl %0" :: "a"(5)); CHECK_EQ(rsar(), 27);
    __asm__ volatile("ssr %0" :: "a"(37)); CHECK_EQ(rsar(), 5);
    __asm__ volatile("ssa8l %0" :: "a"(3)); CHECK_EQ(rsar(), 24);
    __asm__ volatile("ssa8b %0" :: "a"(1)); CHECK_EQ(rsar(), 24);
    __asm__ volatile("ssa8b %0" :: "a"(0)); CHECK_EQ(rsar(), 32);
    __asm__ volatile("ssai 31"); CHECK_EQ(rsar(), 31);
    __asm__ volatile("ssai 17"); CHECK_EQ(rsar(), 17);
    /* multiplies */
    CHECK_EQ(OP2("mull", 0xffffffff, 0xffffffff), 1);
    CHECK_EQ(OP2("muluh", 0xffffffff, 0xffffffff), 0xfffffffe);
    CHECK_EQ(OP2("mulsh", 0xffffffff, 0xffffffff), 0);
    CHECK_EQ(OP2("mulsh", 0x80000000, 0x80000000), 0x40000000);
    CHECK_EQ(OP2("mulsh", 0x80000000, 2), 0xffffffff);
    CHECK_EQ(OP2("mul16u", 0x1ffff, 0x2ffff), 0xfffe0001);
    CHECK_EQ(OP2("mul16s", 0xffff, 0x8000), 0x8000);
    CHECK_EQ(OP2("mul16s", 0x8000, 0x8000), 0x40000000);
    /* divides */
    CHECK_EQ(OP2("quou", 0xffffffff, 2), 0x7fffffff);
    CHECK_EQ(OP2("quos", (uint32_t)-7, 2), (uint32_t)-3);
    CHECK_EQ(OP2("rems", (uint32_t)-7, 2), (uint32_t)-1);
    CHECK_EQ(OP2("remu", 0xffffffff, 10), 5);
    CHECK_EQ(OP2("quos", 0x80000000, 0xffffffff), 0x80000000);
    CHECK_EQ(OP2("rems", 0x80000000, 0xffffffff), 0);
    /* divide by zero -> IntegerDivideByZero (6), EPC1 = the instruction */
    rt_exc_hook = div_hook;
    uint32_t pc_q, r;
    __asm__ volatile("movi %1, 0\n movi %0, 1f\n 1: quou %1, %1, %1" : "=&a"(pc_q), "=&a"(r));
    CHECK_EQ(div_faults, 1);
    CHECK_EQ(last_cause, 6);
    CHECK_EQ(last_pc, pc_q);
    (void)OP2("quos", 5, 0);
    (void)OP2("remu", 5, 0);
    (void)OP2("rems", 5, 0);
    CHECK_EQ(div_faults, 4);
    rt_exc_hook = 0;
    /* moves */
    CHECK_EQ(({ uint32_t r = 1; __asm__("moveqz %0, %1, %2" : "+a"(r) : "a"(9), "a"(0)); r; }), 9);
    CHECK_EQ(({ uint32_t r = 1; __asm__("moveqz %0, %1, %2" : "+a"(r) : "a"(9), "a"(1)); r; }), 1);
    CHECK_EQ(({ uint32_t r = 1; __asm__("movnez %0, %1, %2" : "+a"(r) : "a"(9), "a"(1)); r; }), 9);
    CHECK_EQ(({ uint32_t r = 1; __asm__("movltz %0, %1, %2" : "+a"(r) : "a"(9), "a"(-1)); r; }), 9);
    CHECK_EQ(({ uint32_t r = 1; __asm__("movltz %0, %1, %2" : "+a"(r) : "a"(9), "a"(0)); r; }), 1);
    CHECK_EQ(({ uint32_t r = 1; __asm__("movgez %0, %1, %2" : "+a"(r) : "a"(9), "a"(0)); r; }), 9);
    CHECK_EQ(({ uint32_t r; __asm__("movi %0, -2048" : "=a"(r)); r; }), (uint32_t)-2048);
    CHECK_EQ(({ uint32_t r; __asm__("movi.n %0, -32" : "=a"(r)); r; }), (uint32_t)-32);
    CHECK_EQ(({ uint32_t r; __asm__("movi.n %0, 95" : "=a"(r)); r; }), 95);
    CHECK_EQ(({ uint32_t r; __asm__("addmi %0, %1, -32768" : "=a"(r) : "a"(0)); r; }), (uint32_t)-32768);
    CHECK_EQ(({ uint32_t r; __asm__("addi.n %0, %1, -1" : "=a"(r) : "a"(5)); r; }), 4);
    /* loads/stores of every width, sign extension, unaligned */
    static volatile uint8_t buf[16] __attribute__((aligned(4)));
    for (int i = 0; i < 16; i++) buf[i] = 0x80 + i;
    CHECK_EQ(*(volatile int8_t *)&buf[0], (uint32_t)(int8_t)0x80);
    CHECK_EQ(*(volatile uint16_t *)&buf[2], 0x8382);
    CHECK_EQ(*(volatile int16_t *)&buf[2], 0xffff8382);
    CHECK_EQ(*(volatile uint32_t *)&buf[4], 0x87868584);
    CHECK_EQ(({ uint32_t r; __asm__ volatile("l32i %0, %1, 0" : "=a"(r) : "a"(&buf[1])); r; }), 0x84838281);
    *(volatile uint16_t *)&buf[8] = 0x1234;
    CHECK_EQ(buf[8], 0x34);
    CHECK_EQ(buf[9], 0x12);
    /* l32r (literal) and l32ai/s32ri */
    CHECK_EQ(({ uint32_t r; __asm__ volatile("j 2f\n .align 4\n 1: .word 0xcafef00d\n 2: l32r %0, 1b" : "=a"(r)); r; }),
             0xcafef00d);
    volatile uint32_t w = 5;
    __asm__ volatile("s32ri %1, %0, 0" :: "a"(&w), "a"(77) : "memory");
    CHECK_EQ(({ uint32_t r; __asm__ volatile("l32ai %0, %1, 0" : "=a"(r) : "a"(&w)); r; }), 77);
    /* s32nb, l32e/s32e */
    __asm__ volatile("s32nb %1, %0, 0" :: "a"(&w), "a"(88) : "memory");
    CHECK_EQ(w, 88);
    __asm__ volatile("s32e %1, %0, -64" :: "a"((uint32_t)&w + 64), "a"(99) : "memory");
    CHECK_EQ(({ uint32_t r; __asm__ volatile("l32e %0, %1, -16" : "=a"(r) : "a"((uint32_t)&w + 16)); r; }), 99);
    /* special registers */
    CHECK_EQ(RSR(prid), 0xcdcd);
    CHECK_EQ(RSR(configid0), 0xC2F0FFFE);
    WSR(misc0, 0x1234); CHECK_EQ(RSR(misc0), 0x1234);
    WSR(misc3, 0x5678); CHECK_EQ(RSR(misc3), 0x5678);
    WSR(excsave1, 0xabcd);
    CHECK_EQ(({ uint32_t v = 0x1111; __asm__ volatile("xsr %0, excsave1" : "+a"(v)); v; }), 0xabcd);
    CHECK_EQ(RSR(excsave1), 0x1111);
    WSR(acchi, 0x1ff); CHECK_EQ(RSR(acchi), 0xffffffff);
    WSR(sar, 0x7f); CHECK_EQ(RSR(sar), 0x3f);
    uint32_t c0 = RSR(ccount), c1 = RSR(ccount);
    CHECK(c1 - c0 >= 1 && c1 - c0 < 4);
    uint32_t tp;
    __asm__ volatile("wur %1, threadptr\n rur %0, threadptr" : "=a"(tp) : "a"(0xfeed));
    CHECK_EQ(tp, 0xfeed);
    /* simcall without a simulator returns -1 in a2 */
    return 0;
}
