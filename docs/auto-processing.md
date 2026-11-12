# Watched-folder auto-processing

BridgeCraft can watch an ingest folder and apply a persisted rule after a file
has stopped changing. Rules live in the versioned JSON file
`<user-config>/BridgeCraft/auto-rules.json`; BridgeCraft writes updates
atomically. The versioned JSON config stores a user-adjustable settle delay (defaults to two seconds). It checks both
file size and modification time, then rechecks immediately before dispatch.
Temporary files with names ending in `.tmp`, `.part`, `.partial`, or containing
`.bridgecraft-tmp` are ignored.

Each rule has a root, optional case-insensitive extension allow-list, minimum
size, optional relative subfolder glob, and an ordered action list. `*` matches
within one path component and `**` matches zero or more components. Supported
actions are thumbnail generation, token-pattern rename, export, and moving the
file under a `YYYY/MM-DD/` directory based on its modification date.

Actions run on a bounded four-worker pool, not the UI thread. At most 4,096
actions and 16,384 unsettled paths are retained at once; additional bursts are
left for a later filesystem event or manual processing rather than growing
memory without bound. Individual outcomes are persisted in a bounded 200-entry history. A failure increments the rule's consecutive-failure count; a success
resets it. The tenth consecutive failure disables the rule and persists a
reason. Re-enable or edit the rule to reset the count. Outputs are collision
checked and BridgeCraft temporary outputs are ignored by the watcher.

## Example: camera ingest preset

A rule for `/media/camera-drop` can be configured as:

```json
{
  "version": 1,
  "settle_millis": 2000,
  "rules": [
    {
      "id": "camera-ingest",
      "name": "Camera ingest",
      "root": "/media/camera-drop",
      "enabled": true,
      "extensions": ["jpg", "dng", "nef", "arw"],
      "min_size": 1024,
      "subfolder_pattern": "incoming/**",
      "actions": [
        { "kind": "generate_thumbnail", "max_edge": 256 },
        { "kind": "rename", "pattern": "{date:%Y-%m-%d}_{name}_{seq:0>3}" },
        { "kind": "move_to_dated_folder" }
      ],
      "consecutive_failures": 0,
      "disabled_reason": null
    }
  ]
}
```

The watcher is recursive. A file matched under `incoming/` becomes eligible only
after its fingerprint stays stable for the settle interval. The actions then run
in the listed order; rename updates the path used by later actions. For this
example, thumbnail generation runs first, the renamed file is moved to its
modification-date folder, and a failure is recorded for the file/rule without
stopping other queued files.

The **Auto** status-bar window lets users add, pause/re-enable, and remove
rules, adjust the global settle delay, and inspect recent per-file outcomes.
Metadata-index extraction is not available yet because the search index API is
not implemented; it will be added when that API lands.
