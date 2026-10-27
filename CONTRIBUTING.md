# Contributing to BridgeCraft

## Clean-room attestation (required before first contribution)

By opening your first PR, you affirm all of the following:

1. I have never worked for Adobe on Bridge, Camera Raw, or related products, and I am not bound by any agreement restricting such work. *(Or: if I am, I will disclose to the maintainers and relevant subsystem exclusions will be applied.)*
2. I will not view, copy, decompile, or reverse engineer any Adobe software as part of this project.
3. All code I submit is my own original work or permissively licensed third-party code with attribution recorded in `THIRD_PARTY_LICENSES.md`.
4. I will not submit Adobe-owned assets, icons, artwork, or UI copy.
5. If I discover I have been exposed to Adobe internals relevant to my assigned area, I will notify maintainers immediately and stop work on that area.

Violations: offending code will be removed and rewritten from scratch (see CLEANROOM.md).

## Development

```sh
cargo fmt --all
cargo clippy --workspace -- -D warnings
cargo test --workspace
```

PRs must pass CI on all three platforms and include tests for library crates.
