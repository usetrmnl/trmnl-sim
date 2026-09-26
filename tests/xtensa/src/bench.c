/* Micro-benchmark: CRC32 (bitwise), a byte copy loop and a small recursive function,
   repeated. Only used to measure emulator speed. */
#include "rt.h"

static uint8_t buf[4096], out[4096];

static uint32_t __attribute__((noinline)) crc32(const uint8_t *p, unsigned n) {
    uint32_t c = 0xffffffff;
    while (n--) {
        c ^= *p++;
        for (int k = 0; k < 8; k++) c = (c >> 1) ^ (0xedb88320 & -(c & 1));
    }
    return ~c;
}

static uint32_t __attribute__((noipa)) fib(uint32_t n) { return n < 2 ? n : fib(n - 1) + fib(n - 2); }

int main(void) {
    for (unsigned i = 0; i < sizeof buf; i++) buf[i] = (uint8_t)(i * 7 + 3);
    uint32_t acc = 0;
    for (int it = 0; it < 200; it++) {
        acc += crc32(buf, sizeof buf);
        for (unsigned i = 0; i < sizeof buf; i++) out[i] = buf[i] ^ (uint8_t)acc;
        acc += fib(18) + out[it];
    }
    MMIO_ACTUAL = acc;
    return 0;
}
