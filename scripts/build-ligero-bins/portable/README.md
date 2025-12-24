# Portable binaries packaging

This directory contains helper scripts used by `../build-portable-binaries.sh`.

## Output layout

The main script stages outputs into the repository at:

```
crates/adapters/ligero/bins/
  linux-amd64/{bin,lib}/
  linux-arm64/{bin,lib}/
  macos-arm64/{bin,lib}/
  shader/*.wgsl
```

And a single tarball `ligero-bins.tar.gz` containing the contents of `crates/adapters/ligero/bins/`.

## Notes

- Linux builds run in Docker and include an RPATH of `$ORIGIN/../lib` so binaries prefer the bundled `lib/` directory.
- macOS builds run natively (Apple Silicon) and add `@executable_path/../lib` as an rpath so binaries prefer the bundled `lib/` directory.


