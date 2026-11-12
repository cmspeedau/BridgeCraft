# RFC: Scripting / Automation API Engine Choice (issue #18)

## Summary

Propose **embedded Lua (`mlua`)** as the first scripting engine for BridgeCraft
automation. WASM remains a future option for packaged plugins, but Lua ships
first because it matches the "quick scripts in a folder" workflow and keeps the
host API surface small and auditable.

## Options considered

### Option A — embedded Lua (`mlua`, MIT/Apache-2.0)

- Tiny startup cost; scripts are plain text files in `<config>/scripts/`.
- Trivial sandbox: deny `os`/`io`/`debug`/`package`, expose only capability
  APIs. Sandboxing is a policy decision, not a runtime boundary, which keeps
  it understandable.
- Copy-paste friendly for power users; no toolchain required.
- Weakness: no hard memory cap by default; mitigated with instruction-count
  hooks and wall-clock timeouts.

### Option B — WASM (`wasmtime`/Extism, MIT/Apache-2.0)

- Hard resource limits and capability-based host functions by construction.
- Any source language.
- Weakness: users need a toolchain to author scripts; heavier first-run cost;
  larger dependency surface to vet.

### Option C — both

Likely end state, but not the start: doubling the engine doubles the
sandboxing and host-API maintenance burden before we have a single user.

## Decision

**Lua first.** Revisit WASM once the plugin system (roadmap Phase 5) needs a
packaged, distributable boundary; the host API sketched below is engine-neutral
so a later WASM host can reuse it.

## Host API surface (sketched)

All access goes through capability functions; no ambient filesystem:

- `bc.selection()` — list of selected paths.
- `bc.current_folder()` — browsed root.
- `bc.metadata(path)` / `bc.set_metadata(path, table)` — via `bc-metadata`.
- `bc.rename_plan(pattern, paths)` / `bc.rename_apply(plan)` — via `bc-core`
  rename engine, journaled for undo.
- `bc.export_job(request)` — submit to the bounded export queue.
- `bc.smart_collection_predicate(fn)` — register a predicate over indexed
  metadata (issue #13).
- `bc.log(...)` / `bc.notify(...)` — surface results in the script results
  list view.

## Sandbox & limits

- No globals beyond the `bc` table; `os`, `io`, `debug`, `package`,
  `require` (except a vetted `bc` module) are removed.
- Wall-clock timeout (default 5 s) and instruction-count cap per invocation.
- Scripts live in `<config>/scripts/`; no path outside capability APIs.
- Errors are collected per script and shown in a list view with per-script
  results; a failing script never aborts the host.

## Follow-up issues

- Implementation: script host on a background thread, script discovery,
  results view.
- Later: WASM host reusing the same capability API for packaged plugins.
