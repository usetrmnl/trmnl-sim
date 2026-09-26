/* Zero-overhead loop helpers (called from loops_c.c). */
    .text

/* uint32_t loop_n(uint32_t n): LOOP with n >= 1 iterations, 3-byte last insn */
    .global loop_n
    .align 4
loop_n:
    entry   a1, 32
    movi    a3, 0
    loop    a2, 1f
    addi    a3, a3, 1
1:  mov     a2, a3
    retw

/* narrow last instruction */
    .global loop_narrow
    .align 4
loop_narrow:
    entry   a1, 32
    movi    a3, 0
    loop    a2, 1f
    addi.n  a3, a3, 2
1:  mov     a2, a3
    retw

    .global loopnez_n
    .align 4
loopnez_n:
    entry   a1, 32
    movi    a3, 0
    loopnez a2, 1f
    addi    a3, a3, 1
1:  mov     a2, a3
    retw

    .global loopgtz_n
    .align 4
loopgtz_n:
    entry   a1, 32
    movi    a3, 0
    loopgtz a2, 1f
    addi    a3, a3, 1
1:  mov     a2, a3
    retw

/* last instruction is a conditional branch: not taken -> loops back; taken -> leaves.
   uint32_t loop_break(uint32_t n, uint32_t stop): counts iterations until a3 == stop */
    .global loop_break
    .align 4
loop_break:
    entry   a1, 32
    movi    a4, 0
    loop    a2, 1f
    addi    a4, a4, 1
    beq     a4, a3, 2f
1:  mov     a2, a4
    retw
2:  addi    a2, a4, 1000
    retw

/* a taken backward branch as the last instruction does not decrement LCOUNT:
   inner "manual" loop repeats body 3 times per hardware iteration.
   uint32_t loop_backbranch(uint32_t n) -> n * 3 */
    .global loop_backbranch
    .align 4
loop_backbranch:
    entry   a1, 32
    movi    a4, 0
    loop    a2, 1f
    movi    a5, 3
3:  addi    a4, a4, 1
    addi    a5, a5, -1
    bnez    a5, 3b
1:  mov     a2, a4
    retw

/* nested hardware loops with LBEG/LEND/LCOUNT saved around the inner loop.
   uint32_t loop_nested(uint32_t outer, uint32_t inner) -> outer * inner */
    .global loop_nested
    .align 4
loop_nested:
    entry   a1, 32
    movi    a4, 0
    loop    a2, 1f
    rsr     a6, lbeg
    rsr     a7, lend
    rsr     a8, lcount
    loop    a3, 2f
    addi    a4, a4, 1
2:  wsr     a6, lbeg
    wsr     a7, lend
    wsr     a8, lcount
    isync
    nop
1:  mov     a2, a4
    retw

/* LCOUNT/LBEG/LEND observed: returns LCOUNT at the start of the last iteration (0) plus
   LEND - LBEG. */
    .global loop_regs
    .align 4
loop_regs:
    entry   a1, 32
    movi    a3, 5
    loop    a3, 1f
    rsr     a4, lcount
    nop
1:  rsr     a5, lbeg
    rsr     a6, lend
    sub     a6, a6, a5
    add     a2, a4, a6
    retw
