/* Flat 512 KiB RAM at the reset VECBASE; vectors first, reset code at +0x400. */
MEMORY { ram (rwx) : ORIGIN = 0x40000000, LENGTH = 0x80000 }
ENTRY(_start)
SECTIONS
{
  .vectors 0x40000000 : { KEEP(*(.vectors)) } > ram
  .text 0x40000400 : {
    KEEP(*(.reset))
    *(.literal .text .literal.* .text.*)
  } > ram
  .rodata : { *(.rodata .rodata.* .srodata*) } > ram
  .data : { *(.data .data.* .sdata*) } > ram
  .bss (NOLOAD) : { _bss_start = .; *(.bss .bss.* .sbss* COMMON) _bss_end = .; } > ram
  _stack_top = ORIGIN(ram) + LENGTH(ram) - 64;
  /DISCARD/ : { *(.comment) *(.xtensa.info) *(.xt.*) }
}
