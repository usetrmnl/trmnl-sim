    .text
/* uint32_t double_exc(uint32_t fault_addr): load fault while PS.EXCM=1 -> double
   exception vector (runtime skips the load and returns with RFDE). Returns the PC of
   the faulting load. */
    .global double_exc
    .align 4
double_exc:
    entry   a1, 32
    rsr     a3, ps
    movi    a4, 0x40030
    wsr     a4, ps
    rsync
    movi    a5, 1f
1:  _l32i   a6, a2, 0
    wsr     a3, ps
    rsync
    mov     a2, a5
    retw

/* uint32_t bad_retw(void): RETW with a0[31:30] == 0 is illegal (handler skips it) */
    .global bad_retw
    .align 4
bad_retw:
    entry   a1, 32
    mov     a3, a0
    movi    a0, 0x1234
    _retw                        /* not narrowed: the handler skips 3 bytes */
    mov     a0, a3
    movi    a2, 42
    retw

/* uint32_t bad_sr(void): RSR of a special register the S3 does not have */
    .global bad_sr
    .align 4
bad_sr:
    entry   a1, 32
    movi    a2, 7
    .byte   0x20, 0x36, 0x03     /* rsr a2, 54 */
    retw

/* void ill_n(void) */
    .global ill_n
    .align 4
ill_n:
    entry   a1, 32
    ill.n
    retw

    .global do_syscall
    .align 4
do_syscall:
    entry   a1, 32
    syscall
    retw

/* uint32_t fp_touch(uint32_t x): first FP instruction with CPENABLE clear */
    .global fp_touch
    .align 4
fp_touch:
    entry   a1, 32
    wfr     f0, a2
    add.s   f0, f0, f0
    rfr     a2, f0
    retw

/* void qr_copy(void *dst, const void *src): LD.QR / ST.QR through q5 */
    .global qr_copy
    .align 4
qr_copy:
    entry   a1, 32
    ld.qr   q5, a3, 16
    st.qr   q5, a2, -16
    retw

/* uint32_t cp3_regs(void): WUR/RUR of cop_ai user registers, returns a checksum */
    .global cp3_regs
    .align 4
cp3_regs:
    entry   a1, 32
    movi    a3, -1
    wur.accx_1 a3
    wur.sar_byte a3
    wur.fft_bit_width a3
    movi    a4, 0x12345678
    wur.qacc_l_4 a4
    wur.ua_state_2 a4
    rur.accx_1 a5            /* 0xff */
    rur.sar_byte a6          /* 0xf */
    add     a2, a5, a6
    rur.fft_bit_width a6     /* 0xf */
    add     a2, a2, a6
    rur.qacc_l_4 a6
    add     a2, a2, a6
    rur.ua_state_2 a6
    add     a2, a2, a6
    retw
