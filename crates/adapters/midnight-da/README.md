# `sov-midnight-da`

Midnight implementation of `DaService`, `DaSpec` and `DaVerifier` traits.

Used for Midnight privacy integration.


sov-midnight-da should be imported with "native" flag if any module is imported with the native flag. 
Modules indirectly import rollup-interface with native,
which means that sov-midnight-da cannot fully implement BlobReader if it also does not have "native".