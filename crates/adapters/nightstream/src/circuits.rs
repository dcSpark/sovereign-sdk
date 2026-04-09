//! Pre-compiled Nightstream RISC-V ROM bytes for guest circuits.
//!
//! Each circuit is a compiled RISC-V program whose ROM bytes are checked in
//! as a generated `.rs` file under `circuits/`. The code commitment (method ID)
//! for each circuit is `SHA-256(ROM_BYTES)`.

/// Value validator circuit ROM bytes.
#[allow(missing_docs)]
pub mod value_validator_rom {
    include!("../circuits/value_validator_rom.rs");
}

/// Note spend circuit ROM bytes (midnight-privacy transfer/withdraw).
#[allow(missing_docs)]
pub mod note_spend_rom {
    include!("../circuits/note_spend_rom.rs");
}

/// Note deposit circuit ROM bytes (midnight-privacy deposit).
#[allow(missing_docs)]
pub mod note_deposit_rom {
    include!("../circuits/note_deposit_rom.rs");
}
