//! Guest environment for Ligero zkVM

use serde::de::DeserializeOwned;
use sov_rollup_interface::zk::ZkvmGuest;

use crate::LigeroVerifier;

/// Guest environment for Ligero zkVM
#[derive(Default)]
pub struct LigeroGuest;

impl ZkvmGuest for LigeroGuest {
    type Verifier = LigeroVerifier;

    fn read_from_host<T: DeserializeOwned>(&self) -> T {
        unimplemented!("LigeroGuest::read_from_host - Ligero uses WASM programs directly")
    }

    fn commit<T: serde::Serialize>(&self, _item: &T) {
        unimplemented!("LigeroGuest::commit - Ligero handles commits within WASM")
    }
}

