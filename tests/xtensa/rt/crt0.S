/*
 * Bare-metal runtime for the Xtensa core tests: vector table at VECBASE (0x40000000),
 * reset at 0x40000400. Level-1 exceptions/interrupts and level-2/3 interrupts save the
 * interrupted window + special registers into a frame and call C (rt.c); level 4/5 and
 * NMI handlers are asm-only counters. The window vectors and the alloca handler are
 * the standard Xtensa ones (as in ESP-IDF components/xtensa/xtensa_vectors.S).
 */

#define PS_UM       0x20
#define PS_EXCM     0x10
#define PS_WOE      0x40000

/* Frame layout (must match struct frame in rt.h) */
#define F_PC        0
#define F_PS        4
#define F_SAR       8
#define F_CAUSE     12
#define F_VADDR     16
#define F_LBEG      20
#define F_LEND      24
#define F_LCOUNT    28
#define F_A(n)      (32 + 4 * (n))
#define F_LEVEL     96
#define F_BR        100
#define F_SIZE      112
#define FRAME_ADJ   160         /* keeps the 16 bytes below the interrupted SP intact */

    .section .vectors, "ax"

    .org 0x0
    .global _WindowOverflow4
_WindowOverflow4:
    s32e    a0, a5, -16
    s32e    a1, a5, -12
    s32e    a2, a5,  -8
    s32e    a3, a5,  -4
    rfwo

    .org 0x40
    .global _WindowUnderflow4
_WindowUnderflow4:
    l32e    a0, a5, -16
    l32e    a1, a5, -12
    l32e    a2, a5,  -8
    l32e    a3, a5,  -4
    rfwu

    .align 4
_xt_alloca_exc:
    rsr     a0, WINDOWBASE
    rotw    -1
    rsr     a2, PS
    extui   a3, a2, 8, 4
    xor     a3, a3, a4
    rsr     a4, excsave1
    slli    a3, a3, 8
    xor     a2, a2, a3
    wsr     a2, PS
    rsync
    bbci.l  a4, 31, _WindowUnderflow4
    rotw    -1
    bbci.l  a8, 30, _WindowUnderflow8
    rotw    -1
    j       _WindowUnderflow12

    .org 0x80
    .global _WindowOverflow8
_WindowOverflow8:
    s32e    a0, a9, -16
    l32e    a0, a1, -12
    s32e    a1, a9, -12
    s32e    a2, a9,  -8
    s32e    a3, a9,  -4
    s32e    a4, a0, -32
    s32e    a5, a0, -28
    s32e    a6, a0, -24
    s32e    a7, a0, -20
    rfwo

    .org 0xC0
    .global _WindowUnderflow8
_WindowUnderflow8:
    l32e    a0, a9, -16
    l32e    a1, a9, -12
    l32e    a2, a9,  -8
    l32e    a7, a1, -12
    l32e    a3, a9,  -4
    l32e    a4, a7, -32
    l32e    a5, a7, -28
    l32e    a6, a7, -24
    l32e    a7, a7, -20
    rfwu

    .org 0x100
    .global _WindowOverflow12
_WindowOverflow12:
    s32e    a0,  a13, -16
    l32e    a0,  a1,  -12
    s32e    a1,  a13, -12
    s32e    a2,  a13,  -8
    s32e    a3,  a13,  -4
    s32e    a4,  a0,  -48
    s32e    a5,  a0,  -44
    s32e    a6,  a0,  -40
    s32e    a7,  a0,  -36
    s32e    a8,  a0,  -32
    s32e    a9,  a0,  -28
    s32e    a10, a0,  -24
    s32e    a11, a0,  -20
    rfwo

    .org 0x140
    .global _WindowUnderflow12
_WindowUnderflow12:
    l32e    a0,  a13, -16
    l32e    a1,  a13, -12
    l32e    a2,  a13,  -8
    l32e    a11, a1,  -12
    l32e    a3,  a13,  -4
    l32e    a4,  a11, -48
    l32e    a5,  a11, -44
    l32e    a6,  a11, -40
    l32e    a7,  a11, -36
    l32e    a8,  a11, -32
    l32e    a9,  a11, -28
    l32e    a10, a11, -24
    l32e    a11, a11, -20
    rfwu

    .org 0x180
    wsr     a0, excsave2
    j       _level2
    .org 0x1C0
    wsr     a0, excsave3
    j       _level3
    .org 0x200
    wsr     a0, excsave4
    j       _level4
    .org 0x240
    wsr     a0, excsave5
    j       _level5
    .org 0x280
    wsr     a0, excsave6
    j       _debug
    .org 0x2C0
    wsr     a0, excsave7
    j       _nmi
    .org 0x300                  /* kernel vector (PS.UM = 0) */
    wsr     a0, excsave1
    j       _level1
    .org 0x340                  /* user vector (PS.UM = 1) */
    wsr     a0, excsave1
    rsr     a0, EXCCAUSE
    bnei    a0, 5, 1f
    j       _xt_alloca_exc
1:  j       _level1
    .org 0x3C0
    j       _double

/* ---------------------------------------------------------------------------------- */

    .section .reset, "ax"
    .global _start
    .align 4
_start:
    j       _start2

    .text
    .align 4
_start2:
    movi    a0, 0
    wsr     a0, WINDOWBASE
    rsync
    movi    a0, 1
    wsr     a0, WINDOWSTART
    rsync
    movi    a0, 0x40000000
    wsr     a0, VECBASE
    movi    a1, _stack_top
    movi    a0, PS_UM | PS_WOE
    wsr     a0, PS
    rsync
    movi    a0, 0
    call4   main
    movi    a2, 0x60000000
    s32i    a6, a2, 0           /* exit code */
2:  j       2b

/* Save the interrupted context. Entry: a0 already saved in EXCSAVE_n. */
    .macro SAVE_CTX excsave, epc, eps, level
    mov     a0, a1
    addi    a1, a1, -FRAME_ADJ
    s32i    a0, a1, F_A(1)
    rsr     a0, \excsave
    s32i    a0, a1, F_A(0)
    s32i    a2, a1, F_A(2)
    s32i    a3, a1, F_A(3)
    s32i    a4, a1, F_A(4)
    s32i    a5, a1, F_A(5)
    s32i    a6, a1, F_A(6)
    s32i    a7, a1, F_A(7)
    s32i    a8, a1, F_A(8)
    s32i    a9, a1, F_A(9)
    s32i    a10, a1, F_A(10)
    s32i    a11, a1, F_A(11)
    s32i    a12, a1, F_A(12)
    s32i    a13, a1, F_A(13)
    s32i    a14, a1, F_A(14)
    s32i    a15, a1, F_A(15)
    rsr     a2, \epc
    s32i    a2, a1, F_PC
    rsr     a2, \eps
    s32i    a2, a1, F_PS
    rsr     a2, SAR
    s32i    a2, a1, F_SAR
    rsr     a2, EXCCAUSE
    s32i    a2, a1, F_CAUSE
    rsr     a2, EXCVADDR
    s32i    a2, a1, F_VADDR
    rsr     a2, LBEG
    s32i    a2, a1, F_LBEG
    rsr     a2, LEND
    s32i    a2, a1, F_LEND
    rsr     a2, LCOUNT
    s32i    a2, a1, F_LCOUNT
    rsr     a2, BR
    s32i    a2, a1, F_BR
    movi    a2, \level
    s32i    a2, a1, F_LEVEL
    .endm

    .macro RESTORE_CTX epc, eps
    l32i    a2, a1, F_PC
    wsr     a2, \epc
    l32i    a2, a1, F_PS
    wsr     a2, \eps
    l32i    a2, a1, F_SAR
    wsr     a2, SAR
    l32i    a2, a1, F_LBEG
    wsr     a2, LBEG
    l32i    a2, a1, F_LEND
    wsr     a2, LEND
    l32i    a2, a1, F_LCOUNT
    wsr     a2, LCOUNT
    l32i    a2, a1, F_BR
    wsr     a2, BR
    l32i    a0, a1, F_A(0)
    l32i    a2, a1, F_A(2)
    l32i    a3, a1, F_A(3)
    l32i    a4, a1, F_A(4)
    l32i    a5, a1, F_A(5)
    l32i    a6, a1, F_A(6)
    l32i    a7, a1, F_A(7)
    l32i    a8, a1, F_A(8)
    l32i    a9, a1, F_A(9)
    l32i    a10, a1, F_A(10)
    l32i    a11, a1, F_A(11)
    l32i    a12, a1, F_A(12)
    l32i    a13, a1, F_A(13)
    l32i    a14, a1, F_A(14)
    l32i    a15, a1, F_A(15)
    l32i    a1, a1, F_A(1)
    .endm

    .text
    .align 4
_level1:
    SAVE_CTX excsave1, epc1, PS, 1
    /* run C with EXCM clear, INTLEVEL 1 (level 2/3 may nest), windows enabled */
    movi    a2, PS_UM | PS_WOE | 1
    wsr     a2, PS
    rsync
    mov     a6, a1
    call4   rt_level1
    movi    a2, PS_UM | PS_WOE | PS_EXCM | 1
    wsr     a2, PS
    rsync
    RESTORE_CTX epc1, PS
    rsync
    rfe

    .align 4
_level2:
    SAVE_CTX excsave2, epc2, eps2, 2
    movi    a2, PS_UM | PS_WOE | 2
    wsr     a2, PS
    rsync
    mov     a6, a1
    call4   rt_interrupt
    movi    a2, PS_UM | PS_WOE | PS_EXCM | 2
    wsr     a2, PS
    rsync
    RESTORE_CTX epc2, eps2
    rfi     2

    .align 4
_level3:
    SAVE_CTX excsave3, epc3, eps3, 3
    movi    a2, PS_UM | PS_WOE | 3
    wsr     a2, PS
    rsync
    mov     a6, a1
    call4   rt_interrupt
    movi    a2, PS_UM | PS_WOE | PS_EXCM | 3
    wsr     a2, PS
    rsync
    RESTORE_CTX epc3, eps3
    rfi     3

/* Level 4: count it and mask the lines at this level (the test re-enables). */
    .align 4
_level4:
    movi    a0, rt_hi_scratch
    s32i    a2, a0, 0
    s32i    a3, a0, 4
    movi    a2, rt_level4_count
    l32i    a3, a2, 0
    addi    a3, a3, 1
    s32i    a3, a2, 0
    rsr     a2, INTENABLE
    movi    a3, ~0x53000000
    and     a2, a2, a3
    wsr     a2, INTENABLE
    l32i    a2, a0, 0
    l32i    a3, a0, 4
    rsr     a0, excsave4
    rfi     4

/* Level 5: CCOMPARE2 timer (int 16) and others: count, clear the timer by rewriting
   CCOMPARE2, mask level-5 lines other than the timer. Records eps5 for the test. */
    .align 4
_level5:
    movi    a0, rt_hi_scratch
    s32i    a2, a0, 8
    s32i    a3, a0, 12
    movi    a2, rt_level5_count
    l32i    a3, a2, 0
    addi    a3, a3, 1
    s32i    a3, a2, 0
    rsr     a3, eps5
    s32i    a3, a2, 4           /* rt_level5_count[1] = eps5 */
    rsr     a3, ccompare2
    wsr     a3, ccompare2      /* clears int 16 */
    rsr     a2, INTENABLE
    movi    a3, ~0x84000000
    and     a2, a2, a3
    wsr     a2, INTENABLE
    l32i    a2, a0, 8
    l32i    a3, a0, 12
    rsr     a0, excsave5
    rfi     5

    .align 4
_nmi:
    movi    a0, rt_hi_scratch
    s32i    a2, a0, 16
    s32i    a3, a0, 20
    movi    a2, rt_nmi_count
    l32i    a3, a2, 0
    addi    a3, a3, 1
    s32i    a3, a2, 0
    rsr     a3, eps7
    s32i    a3, a2, 4
    l32i    a2, a0, 16
    l32i    a3, a0, 20
    rsr     a0, excsave7
    rfi     7

/* Debug exceptions are not expected (the harness reports BREAK as a trap). */
    .align 4
_debug:
    movi    a0, 0x60000000
    movi    a2, 0xdeb6
    s32i    a2, a0, 0
3:  j       3b

/* Double exception: record DEPC/EXCCAUSE, skip the (3-byte) faulting instruction. */
    .align 4
_double:
    wsr     a0, excsave1
    movi    a0, rt_double
    s32i    a2, a0, 12
    rsr     a2, DEPC
    s32i    a2, a0, 0
    addi    a2, a2, 3
    wsr     a2, DEPC
    rsr     a2, EXCCAUSE
    s32i    a2, a0, 4
    l32i    a2, a0, 8
    addi    a2, a2, 1
    s32i    a2, a0, 8
    l32i    a2, a0, 12
    rsr     a0, excsave1
    rfde
