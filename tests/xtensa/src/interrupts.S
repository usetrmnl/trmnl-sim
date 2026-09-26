    .text
/* uint32_t excm_test(void): with PS.EXCM set, level-1 and level-3 (= EXCM_LEVEL)
   interrupts stay pending while a level-5 timer interrupt is still taken. Returns the
   INTERRUPT register sampled while masked. */
    .global excm_test
    .align 4
excm_test:
    entry   a1, 32
    rsr     a3, ps
    movi    a4, 0x40030          /* WOE | UM | EXCM, INTLEVEL 0 */
    wsr     a4, ps
    rsync
    movi    a5, (1 << 7) | (1 << 29)
    wsr     a5, intset
    rsync
    rsr     a6, ccount
    addi    a6, a6, 30
    wsr     a6, ccompare2
    movi    a8, 200
1:  addi    a8, a8, -1
    bnez    a8, 1b
    rsr     a6, interrupt
    wsr     a3, ps               /* pending level 1/3 interrupts are taken after this */
    rsync
    mov     a2, a6
    retw

/* uint32_t hw_loop(uint32_t n): n iterations of a hardware loop (interrupted a lot) */
    .global hw_loop
    .align 4
hw_loop:
    entry   a1, 32
    movi    a3, 0
    movi    a4, 0
    loop    a2, 1f
    addi    a3, a3, 1
    addi    a4, a4, 3
1:  sub     a4, a4, a3
    sub     a4, a4, a3
    sub     a2, a4, a3           /* 3n - 3n = 0 if all iterations ran */
    add     a2, a2, a3           /* -> n */
    retw

/* void do_waiti(void) */
    .global do_waiti
    .align 4
do_waiti:
    entry   a1, 32
    waiti   0
    retw
