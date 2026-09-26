/* MAC16: MUL/MULA/MULS/UMUL with AA/AD/DA/DD operands, the LDINC/LDDEC forms, ACC. */
#include "rt.h"

static int64_t acc_get(void) {
    uint32_t lo = RSR(acclo), hi = RSR(acchi);
    return (int64_t)((uint64_t)(int8_t)hi << 32 | lo);
}
static void acc_set(int64_t v) { WSR(acclo, (uint32_t)v); WSR(acchi, (uint32_t)(v >> 32)); }
static int64_t wrap40(int64_t v) { return (int64_t)((uint64_t)v << 24) >> 24; }
static int32_t h(uint32_t v, int hi) { return (int16_t)(hi ? v >> 16 : v); }
static uint32_t hu(uint32_t v, int hi) { return (uint16_t)(hi ? v >> 16 : v); }

#define AA(insn, x, y) __asm__ volatile(insn " %0, %1" :: "a"(x), "a"(y))

int main(void) {
    const uint32_t xs[] = {0x7fff8000, 0x8001ffff, 0x12345678, 0xffff0001, 0};
    for (int i = 0; i < 5; i++)
        for (int j = 0; j < 5; j++) {
            uint32_t x = xs[i], y = xs[j];
            acc_set(0x7f00000000ll); AA("umul.aa.ll", x, y);
            CHECK(acc_get() == (int64_t)(hu(x, 0) * hu(y, 0)));
            AA("umul.aa.hh", x, y);
            CHECK(acc_get() == (int64_t)(hu(x, 1) * hu(y, 1)));
            AA("mul.aa.hl", x, y);
            CHECK(acc_get() == (int64_t)h(x, 1) * h(y, 0));
            AA("mul.aa.lh", x, y);
            CHECK(acc_get() == (int64_t)h(x, 0) * h(y, 1));
            int64_t a = 0x7ffffffff0ll;
            acc_set(a); AA("mula.aa.ll", x, y);
            a = wrap40(a + (int64_t)h(x, 0) * h(y, 0));
            CHECK(acc_get() == a);
            AA("muls.aa.hh", x, y);
            a = wrap40(a - (int64_t)h(x, 1) * h(y, 1));
            CHECK(acc_get() == a);
        }
    /* AD / DA / DD via the MR registers */
    WSR(m0, 0x00030004); WSR(m1, 0xfffe0005); WSR(m2, 0x00070008); WSR(m3, 0x0009fff6);
    acc_set(0);
    __asm__ volatile("mul.ad.hl %0, m2" :: "a"(0x00020000));
    CHECK(acc_get() == 2 * 8);
    __asm__ volatile("mula.da.lh m1, %0" :: "a"(0x00100000));
    CHECK(acc_get() == 16 + 5 * 16);
    __asm__ volatile("muls.dd.hh m1, m3");
    CHECK(acc_get() == 96 - (-2 * 9));
    __asm__ volatile("mul.dd.ll m0, m3");
    CHECK(acc_get() == 4 * -10);
    /* LDINC / LDDEC */
    static volatile uint32_t mem[4] = {0x11112222, 0x00050006, 0x00070002, 0x44445555};
    uint32_t p = (uint32_t)&mem[0];
    __asm__ volatile("ldinc m2, %0" : "+a"(p) :: "memory");
    CHECK_EQ(p, (uint32_t)&mem[1]);
    CHECK_EQ(RSR(m2), 0x00050006);
    __asm__ volatile("lddec m3, %0" : "+a"(p) :: "memory");
    CHECK_EQ(p, (uint32_t)&mem[0]);
    CHECK_EQ(RSR(m3), 0x11112222);
    /* mula.dd.ll.ldinc: multiply with the old m0/m2, then load m0 */
    WSR(m0, 0x00000003); WSR(m2, 0x00000004);
    acc_set(100);
    p = (uint32_t)&mem[1];
    __asm__ volatile("mula.dd.ll.ldinc m0, %0, m0, m2" : "+a"(p) :: "memory");
    CHECK(acc_get() == 112);
    CHECK_EQ(RSR(m0), 0x00070002);
    CHECK_EQ(p, (uint32_t)&mem[2]);
    __asm__ volatile("mula.da.hl.lddec m1, %0, m0, %1" : "+a"(p) : "a"(0x0000000a) : "memory");
    CHECK(acc_get() == 112 + 7 * 10);
    CHECK_EQ(RSR(m1), 0x00050006);
    return 0;
}
