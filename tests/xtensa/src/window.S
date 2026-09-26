/* Hand-written windowed call chains: r4 -> r8 -> r12 -> r4 ... using CALL4, CALL8 and
   CALL12, each keeping values live in the registers its call size preserves. */
    .text

    .global r4
    .align 4
r4:
    entry   a1, 32
    beqz    a2, 1f
    slli    a3, a2, 2
    xor     a3, a3, a2
    addi    a6, a2, -1
    call4   r8
    slli    a2, a6, 1
    add     a2, a2, a6
    add     a2, a2, a3
    retw
1:  movi    a2, 1
    retw

    .global r8
    .align 4
r8:
    entry   a1, 32
    beqz    a2, 1f
    addi    a4, a2, 1
    slli    a5, a2, 3
    xor     a6, a4, a5
    add     a7, a6, a2
    addi    a10, a2, -1
    call8   r12
    slli    a2, a10, 1
    add     a2, a2, a10
    add     a2, a2, a4
    add     a2, a2, a5
    add     a2, a2, a6
    add     a2, a2, a7
    retw
1:  movi    a2, 1
    retw

    .global r12
    .align 4
r12:
    entry   a1, 48
    beqz    a2, 1f
    addi    a3, a2, 3
    addi    a4, a2, 4
    addi    a5, a2, 5
    addi    a6, a2, 6
    addi    a7, a2, 7
    addi    a8, a2, 8
    addi    a9, a2, 9
    addi    a10, a2, 10
    addi    a11, a2, 11
    addi    a14, a2, -1
    call12  r4
    slli    a2, a14, 1
    add     a2, a2, a14
    add     a2, a2, a3
    add     a2, a2, a4
    add     a2, a2, a5
    add     a2, a2, a6
    add     a2, a2, a7
    add     a2, a2, a8
    add     a2, a2, a9
    add     a2, a2, a10
    add     a2, a2, a11
    retw.n
1:  movi    a2, 1
    retw.n

/* uint32_t callx_chain(fnptr f, uint32_t n): call through CALLX4 / CALLX12 */
    .global callx4_fn
    .align 4
callx4_fn:
    entry   a1, 32
    mov     a6, a3
    callx4  a2
    mov     a2, a6
    retw

    .global callx12_fn
    .align 4
callx12_fn:
    entry   a1, 48
    mov     a14, a3
    callx12 a2
    mov     a2, a14
    retw

/* uint32_t rotw_test(void): ROTW moves the window by 4 registers per step. */
    .global rotw_test
    .align 4
rotw_test:
    entry   a1, 32
    movi    a4, 0x44
    rsr     a3, windowbase
    rotw    1
    mov     a2, a0          /* old a4 */
    rotw    -1
    rsr     a5, windowbase
    sub     a5, a5, a3      /* 0 */
    add     a2, a4, a5      /* 0x44 */
    retw
