/*
 * A plausible Cortex-M memory layout for the flash measurement (G2.4). This crate ships no linker
 * script of its own - an integrator's comes from their MCU's `cortex-m-rt` support crate - but
 * `scripts/flash-cost.sh` measures a *linked* image, and linking needs somewhere to put one.
 *
 * 1 MB flash / 128 KB RAM is representative of the STM32-class parts this crate targets. Only the
 * layout matters here, not the sizes: the measurement is the size of the .text/.rodata the image
 * actually occupies, and nothing is ever executed.
 */
MEMORY {
  FLASH : ORIGIN = 0x08000000, LENGTH = 1024K
  RAM   : ORIGIN = 0x20000000, LENGTH = 128K
}

ENTRY(_start)

SECTIONS {
  .text : { *(.text .text.*) } > FLASH
  .rodata : { *(.rodata .rodata.*) } > FLASH
  .data : { *(.data .data.*) } > RAM
  .bss : { *(.bss .bss.* COMMON) } > RAM
  /* No unwinding: the probe builds with `panic = "abort"`, so exception tables are dead weight
     that would otherwise inflate the measured image. */
  /DISCARD/ : { *(.ARM.exidx .ARM.exidx.* .ARM.extab*) }
}
