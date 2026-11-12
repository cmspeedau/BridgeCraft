# Scripting / Automation API (issue #18)

## Decision

BridgeCraft uses **WebAssembly (WASM)** for automation scripts and plugins. The
`wasmi` interpreter already used by the plugin host is also the scripting
runtime, avoiding a second execution engine and keeping one auditable sandbox
and host ABI. Native plugins, WASI, ambient filesystem access, and ambient
network access are not part of v1.

Scripts are discovered from `<config>/scripts/` as local plugin-style
subdirectories. Each script directory contains `plugin.json` and its declared
WASM module. The manifest declares requested capabilities and explicit
filesystem roots. User grants are persisted separately, default to denied,
and cannot exceed what the manifest requests.

## Host API

WASM guests import only explicitly implemented functions from the
`bridgecraft` module. All functions enforce granted capabilities and relevant
filesystem roots before calling the native host:

- `selection_count` / `selection_path` — selection access, gated by
  `selection_read`.
- `current_folder` — browsed folder, gated by `current_folder_read`.
- `metadata_read` — serialized metadata, gated by `metadata_read` and an
  explicit filesystem-root grant.
- `thumbnail_read` — thumbnail bytes, gated by `thumbnail_read` and an
  explicit filesystem-root grant.
- `file_rename`, `file_copy`, and `file_move` — file operations, gated by
  `file_ops` and explicit grants for source and destination paths.
- `log` — append bounded UTF-8 output to the invocation result.

Network may be declared in the manifest for forward compatibility, but no
network host function is currently linked. A network grant does not create
network access.

## Sandbox and limits

- `wasmi` executes modules without WASI or ambient imports.
- WASM fuel is the deterministic execution bound; linear memory and module
  sizes are capped. Elapsed-time checks are diagnostic and are not represented
  as an interruptible hard timeout.
- Scripts and plugins have isolated persisted enablement and grants.
- Invalid manifests, modules, and guest traps are surfaced per invocation;
  failures in `run_all` do not stop other scripts.
- Scripts run only after explicit discovery and must resolve to a discovered
  module beneath the scripts directory.

## Current scope and follow-ups

The script host exposes discovery, execution, per-script results, persistent
enablement/grants, and the same permissioned native host interface as plugins.
The UI results view and a script authoring/toolchain workflow remain follow-up
work. Plugin panels are declared but not rendered in v1. API imports and guest
memory buffers are versioned by the plugin API version in `plugin.json`.
