//! Guest environment for Nightstream zkVM.
//!
//! This is a stub implementation because Nightstream guest programs run inside
//! the RISC-V simulator, not in native Rust. The actual guest code is written
//! using the `nightstream-sdk` crate with the `#[provable]` macro.

use serde::de::DeserializeOwned;
use sov_rollup_interface::zk::ZkvmGuest;

use crate::NightstreamVerifier;

/// Guest environment for Nightstream zkVM.
#[derive(Default)]
pub struct NightstreamGuest;

impl ZkvmGuest for NightstreamGuest {
    type Verifier = NightstreamVerifier;

    fn read_from_host<T: DeserializeOwned>(&self) -> T {
        unimplemented!(
            "NightstreamGuest::read_from_host - Nightstream guests run inside the RISC-V VM \
             and read inputs via NeoAbi at RAM address 0x104"
        )
    }

    fn commit<T: serde::Serialize>(&self, _item: &T) {
        unimplemented!(
            "NightstreamGuest::commit - Nightstream guests write output via NeoAbi at RAM \
             address 0x100"
        )
    }
}
