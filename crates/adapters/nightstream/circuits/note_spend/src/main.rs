/*
 * RISC-V Guest Program: Note Spend (Placeholder)
 *
 * Lightweight echo circuit used while the Nightstream prover lacks native
 * BN254 field arithmetic.  The host writes a length-prefixed buffer of u32
 * words into the INPUT region; the circuit copies them verbatim to the
 * OUTPUT region.  Output claims on those words bind the proof to the
 * serialised SpendPublic payload.
 *
 * Input (via Neo ABI, starting at INPUT_ADDR = 0x104):
 *   word 0:  len  -- number of payload u32 words to echo
 *   words 1..len: payload words
 *
 * Output (starting at OUTPUT_ADDR = 0x100):
 *   words 0..len: copied payload words
 */

#![no_std]
#![no_main]

/// Input address (Nightstream guest ABI).
const INPUT_ADDR: u32 = 0x104;

/// Output address (Nightstream guest ABI).
const OUTPUT_ADDR: u32 = 0x100;

#[nightstream_sdk::provable]
fn note_spend() -> ! {
    // First input word: number of payload words to echo.
    let len = unsafe { core::ptr::read_volatile(INPUT_ADDR as *const u32) };

    let mut src = INPUT_ADDR + 4;
    let mut dst = OUTPUT_ADDR;

    // Copy each payload word from input to output.
    let mut i: u32 = 0;
    while i < len {
        let word = unsafe { core::ptr::read_volatile(src as *const u32) };
        unsafe { core::ptr::write_volatile(dst as *mut u32, word) };
        src += 4;
        dst += 4;
        i += 1;
    }

    nightstream_sdk::halt();
}
