/* Every branch instruction, taken and not taken; boolean registers. */
#include "rt.h"

#define BR2(insn, a, b) ({ uint32_t _r; __asm__ volatile(insn " %1, %2, 1f\n movi %0, 0\n j 2f\n 1: movi %0, 1\n 2:" \
    : "=&a"(_r) : "a"((uint32_t)(a)), "a"((uint32_t)(b))); _r; })
#define BRI(insn, a, imm) ({ uint32_t _r; __asm__ volatile(insn " %1, " #imm ", 1f\n movi %0, 0\n j 2f\n 1: movi %0, 1\n 2:" \
    : "=&a"(_r) : "a"((uint32_t)(a))); _r; })
#define BRZ(insn, a) ({ uint32_t _r; __asm__ volatile(insn " %1, 1f\n movi %0, 0\n j 2f\n 1: movi %0, 1\n 2:" \
    : "=&a"(_r) : "a"((uint32_t)(a))); _r; })
#define BRB(insn, br, b) ({ uint32_t _r; __asm__ volatile("wsr %1, br\n " insn " b" #b ", 1f\n movi %0, 0\n j 2f\n 1: movi %0, 1\n 2:" \
    : "=&a"(_r) : "a"((uint32_t)(br))); _r; })

static uint32_t rbr(void) { return RSR(br); }

int main(void) {
    CHECK_EQ(BR2("beq", 5, 5), 1); CHECK_EQ(BR2("beq", 5, 6), 0);
    CHECK_EQ(BR2("bne", 5, 6), 1); CHECK_EQ(BR2("bne", 5, 5), 0);
    CHECK_EQ(BR2("blt", -1, 0), 1); CHECK_EQ(BR2("blt", 0, -1), 0); CHECK_EQ(BR2("blt", 3, 3), 0);
    CHECK_EQ(BR2("bge", 0, -1), 1); CHECK_EQ(BR2("bge", 3, 3), 1); CHECK_EQ(BR2("bge", -1, 0), 0);
    CHECK_EQ(BR2("bltu", 0, -1), 1); CHECK_EQ(BR2("bltu", -1, 0), 0);
    CHECK_EQ(BR2("bgeu", -1, 0), 1); CHECK_EQ(BR2("bgeu", 0, -1), 0); CHECK_EQ(BR2("bgeu", 7, 7), 1);
    CHECK_EQ(BR2("bany", 0x10, 0x30), 1); CHECK_EQ(BR2("bany", 0x10, 0x20), 0);
    CHECK_EQ(BR2("bnone", 0x10, 0x20), 1); CHECK_EQ(BR2("bnone", 0x10, 0x30), 0);
    CHECK_EQ(BR2("ball", 0x31, 0x30), 1); CHECK_EQ(BR2("ball", 0x11, 0x30), 0);
    CHECK_EQ(BR2("bnall", 0x11, 0x30), 1); CHECK_EQ(BR2("bnall", 0x31, 0x30), 0);
    CHECK_EQ(BR2("bbc", 0xfffffffe, 0), 1); CHECK_EQ(BR2("bbc", 1, 0), 0);
    CHECK_EQ(BR2("bbc", 0x7fffffff, 31), 1); CHECK_EQ(BR2("bbc", 0x7fffffff, 63), 1); /* bit = at & 31 */
    CHECK_EQ(BR2("bbs", 0x80000000, 31), 1); CHECK_EQ(BR2("bbs", 0x80000000, 30), 0);
    CHECK_EQ(BRI("bbci", 0xfffeffff, 16), 1); CHECK_EQ(BRI("bbci", 0x10000, 16), 0);
    CHECK_EQ(BRI("bbsi", 0x80000000, 31), 1); CHECK_EQ(BRI("bbsi", 0x80000000, 0), 0);
    CHECK_EQ(BRI("bbsi", 1, 0), 1);
    CHECK_EQ(BRI("beqi", -1, -1), 1); CHECK_EQ(BRI("beqi", 1, -1), 0);
    CHECK_EQ(BRI("beqi", 256, 256), 1); CHECK_EQ(BRI("beqi", 12, 12), 1);
    CHECK_EQ(BRI("bnei", 5, 4), 1); CHECK_EQ(BRI("bnei", 4, 4), 0);
    CHECK_EQ(BRI("blti", -2, -1), 1); CHECK_EQ(BRI("blti", -1, -1), 0); CHECK_EQ(BRI("blti", 7, 8), 1);
    CHECK_EQ(BRI("bgei", -1, -1), 1); CHECK_EQ(BRI("bgei", -2, -1), 0); CHECK_EQ(BRI("bgei", 128, 64), 1);
    CHECK_EQ(BRI("bltui", 32767, 32768), 1); CHECK_EQ(BRI("bltui", 32768, 32768), 0);
    CHECK_EQ(BRI("bltui", -1, 65536), 0); CHECK_EQ(BRI("bltui", 1, 2), 1);
    CHECK_EQ(BRI("bgeui", 65536, 65536), 1); CHECK_EQ(BRI("bgeui", 65535, 65536), 0);
    CHECK_EQ(BRI("bgeui", -1, 16), 1);
    CHECK_EQ(BRZ("beqz", 0), 1); CHECK_EQ(BRZ("beqz", 1), 0);
    CHECK_EQ(BRZ("bnez", 1), 1); CHECK_EQ(BRZ("bnez", 0), 0);
    CHECK_EQ(BRZ("bltz", -1), 1); CHECK_EQ(BRZ("bltz", 0), 0);
    CHECK_EQ(BRZ("bgez", 0), 1); CHECK_EQ(BRZ("bgez", -1), 0);
    CHECK_EQ(BRZ("beqz.n", 0), 1); CHECK_EQ(BRZ("beqz.n", 9), 0);
    CHECK_EQ(BRZ("bnez.n", 9), 1); CHECK_EQ(BRZ("bnez.n", 0), 0);
    CHECK_EQ(BRB("bt", 1 << 3, 3), 1); CHECK_EQ(BRB("bt", ~(1 << 3), 3), 0);
    CHECK_EQ(BRB("bf", ~(1 << 15), 15), 1); CHECK_EQ(BRB("bf", 1 << 15, 15), 0);
    /* boolean ops */
    WSR(br, 0x0003); /* b0 = b1 = 1 */
    __asm__ volatile("andb b2, b0, b1\n andbc b3, b0, b1\n orb b4, b0, b5\n orbc b6, b5, b0\n xorb b7, b0, b1");
    CHECK_EQ(rbr(), 0x0017);
    WSR(br, 0x00f0);
    __asm__ volatile("any4 b8, b4\n all4 b9, b4\n any4 b10, b0\n all4 b11, b0");
    CHECK_EQ(rbr() & 0x0f00, 0x0300);
    WSR(br, 0x00ff);
    __asm__ volatile("all8 b8, b0\n any8 b9, b8");
    CHECK_EQ(rbr() >> 8 & 3, 3);
    WSR(br, 0x007f);
    __asm__ volatile("all8 b8, b0\n");
    CHECK_EQ(rbr() >> 8 & 1, 0);
    /* movt/movf */
    WSR(br, 1 << 2);
    CHECK_EQ(({ uint32_t r = 1; __asm__("movt %0, %1, b2" : "+a"(r) : "a"(5)); r; }), 5);
    CHECK_EQ(({ uint32_t r = 1; __asm__("movf %0, %1, b2" : "+a"(r) : "a"(5)); r; }), 1);
    /* jx / call0 / callx0 / ret */
    uint32_t r;
    __asm__ volatile("movi %0, 1f\n jx %0\n movi %0, 0\n 1: movi %0, 77" : "=&a"(r));
    CHECK_EQ(r, 77);
    return 0;
}
