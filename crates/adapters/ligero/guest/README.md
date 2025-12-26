# Ligero guest programs

This directory intentionally contains **only the built guest artifacts** used by Sovereign:

- `bins/programs/*.wasm` (and optional `.wat`)

The **guest program sources** were moved to the Ligero-owned repo:

- `ligero-prover/utils/circuits/note-spend-guest`
- `ligero-prover/utils/circuits/rust-guest`

To rebuild the WASMs, build them from the `ligero-prover` repo and then copy the resulting `.wasm` files into `bins/programs/` here.
