/* Nightstream guest linker script for riscv32im-unknown-none-elf.
 *
 * Vendored from: https://github.com/dcSpark/Nightstream
 *   crates/nightstream-sdk/guest/riscv32im-unknown-none-elf.ld
 *   commit: a6b22554a69106a1119f3d326a42bd9c234c741f
 */

ENTRY(_start);

SECTIONS
{
  . = 0x0;

  /* Provide a default fixed stack pointer (top of our tiny test RAM).
     Can be overridden by the linker via --defsym=_STACK_PTR=... */
  PROVIDE(_STACK_PTR = 0x7f0);

  /* Neo guest ABI addresses (RAM). */
  PROVIDE(_NEO_OUTPUT_ADDR = 0x100);
  PROVIDE(_NEO_INPUT_ADDR  = 0x104);

  /* Heap region for optional bump allocator (RAM). */
  PROVIDE(_NEO_HEAP_START = 0x200);
  PROVIDE(_NEO_HEAP_END   = 0x700);

  .neo_start : ALIGN(4)
  {
    KEEP(*(.neo_start))
    *(.text*)
  }

  /* Data sections are placed after code in the linker address space.
   * The Rv32B1 runner validates all ROM words as 32-bit instructions,
   * so data MUST NOT be in .neo_start.  The host pre-loads these
   * bytes into RAM via ram_init at the addresses assigned here. */
  .rodata : ALIGN(4)
  {
    *(.rodata*)
  }

  .data : ALIGN(4)
  {
    *(.data*)
    *(.sdata*)
  }

  .bss : ALIGN(4)
  {
    *(.bss*)
    *(.sbss*)
  }

  /DISCARD/ :
  {
    *(.eh_frame*)
    *(.comment*)
  }
}
