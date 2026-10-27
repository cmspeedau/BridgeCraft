# AGENTS.md — Guidance for AI Coding Agents

This document gives AI coding agents (and human contributors) the conventions and constraints for working on **BridgeCraft**. Follow it alongside [ROADMAP.md](ROADMAP.md), [README.md](README.md), [CONTRIBUTING.md](CONTRIBUTING.md), and [CLEANROOM.md](CLEANROOM.md).

## Project overview

BridgeCraft is a clean-room, cross-platform digital asset browser and file manager written in Rust. It is intentionally **not** a port or translation of any proprietary codebase.

### Clean-room policy (non-negotiable)

- **Never copy or translate Adobe Bridge (or any proprietary) source code, decompiled output, or assets.** No reading its binaries, resources, or SDKs to reproduce internals.
- Behavior may be informed by **publicly documented formats, openly licensed specifications, and observable UI behavior described in writing**.
- All contributors must sign the attestation in CONTRIBUTING.md before their first contribution. Agents must not silently include third-party code without a compatible, documented license.
- Third-party dependencies are tracked in `THIRD_PARTY_LICENSES.md`.

## Workspace layout

| Crate | Purpose |
|---|---|
| `bc-core` | Filesystem scanning, watchers, selection model, app state |
| `bc-metadata` | EXIF / IPTC / XMP / sidecar read & write |
| `bc-thumbnails` | Thumbnail cache (SQLite), decoders (JPEG/PNG/RAW/PSD/PDF/video) |
| `bc-ui` | Frontend shell (GUI framework TBD — see Phase 0 spike in ROADMAP) |

Keep crate boundaries clean: `bc-ui` depends on the others; the others never depend on `bc-ui`.

## Build, test, and quality gates

Run these before declaring any task complete:

```sh
cargo build --workspace
cargo test --workspace
cargo clippy --workspace -- -D warnings
cargo fmt --all -- --check
```

- Never claim a test, build, or lint run succeeded unless you actually ran it and saw the output.
- Distinguish pre-existing failures from regressions introduced by your changes.
- Add or update tests for new features, bug fixes, and important edge cases.

## Coding conventions

- Rust edition and toolchain: follow `rust-toolchain.toml` / workspace `Cargo.toml` if present; otherwise the latest stable channel.
- Follow existing style. Use `rustfmt` defaults; keep `clippy` clean with no suppressions unless justified in a comment.
- Prefer the smallest change that fully solves the task. No opportunistic refactoring or unrelated formatting changes.
- No placeholder implementations, no silently swallowed errors, no `unwrap()` in non-test code paths that can receive untrusted input.
- Validate external inputs (file paths, metadata, sidecars) at boundaries; file parsing is untrusted input.
- Never hardcode secrets, API keys, or credentials.

## Scope and safety rules

- **Do not modify or delete** anything not required by the task. Do not overwrite unrelated in-progress changes.
- Preserve backward compatibility of internal APIs unless the task requires otherwise.
- Never run destructive commands (`cargo clean` beyond need, `git reset --hard`, force pushes, deleting branches/issues) without explicit user confirmation.
- The user reviews and merges; agents push only the specific files they were asked to change, with clear commit messages.

## Roadmap discipline

- Implement work according to [ROADMAP.md](ROADMAP.md). If a requested change conflicts with the roadmap, flag it and ask before diverging.
- Update the roadmap when adding user-visible behavior, changing packaging, or completing milestones.

## Packaging & releases

Final-stage targets (Phase 6): macOS universal 2 build, Windows portable ZIP + MSI, Linux AppImage with `.zsync` delta updates. Artifacts are **unsigned** by user decision.

- Do not add code-signing, notarization, or signing-certificate steps. This is a documented, accepted trade-off (see README "Releases & packaging" and the roadmap risks section).
- Keep packaging configuration reproducible and driven by CI, not manual local steps.

## Documentation

- Update README/ROADMAP/docs for any user-visible behavior, new API, or configuration change.
- Keep documentation consistent with what is actually implemented — never document unverified behavior.
