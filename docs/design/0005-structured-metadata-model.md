# RFC-0005: Structured Metadata Model + XMP Property Core

**Status:** Proposed (implementation landed in `bc-metadata`; maintainer sign-off
requested on the write-policy open question below)
**Issue:** #5
**Crate:** `bc-metadata`

## Summary

Phase 3 needs one metadata representation that EXIF, IPTC-IIM and XMP readers
and writers can all target. The old flat `BTreeMap<String, String>` loses
structure (sequences, language alternates, GPS blocks, EXIF sub-IFDs) and cannot
round-trip writes safely. This RFC proposes an XMP-data-model-shaped structured
store as the canonical representation, with EXIF and IPTC treated as projections.

## 1. Canonical model: XMP

**Decision: yes — XMP is the internal canonical model.** EXIF and IPTC-IIM are
projections on read (decoded into XMP-shaped properties) and on write (projected
back into the target container's native representation).

Rationale:

- XMP (ISO 16684-1) defines a generic data model that can express everything
  EXIF (CIPA DC-X004) and IPTC IIM4 carry: simple typed values, ordered and
  unordered arrays, alternatives, language alternates, and nested structures.
- A single store means readers/writers share one validation and serialization
  path; per-format code only handles container framing, not property semantics.
- Round-tripping is well-defined: an XMP packet read from a file can be written
  back byte-for-byte (modulo normalisation), and native fields are preserved via
  the native-block mechanism (§2).

Trade-off: EXIF-native fields with no clean XMP mapping must be handled
explicitly (§2); this is acceptable and is the only cost.

## 2. EXIF-native fields with no clean XMP mapping

GPS sub-IFD details, maker notes, and other binary/EXIF-specific payloads that
do not project cleanly into XMP properties are carried in a parallel
`native: Vec<NativeBlock>` on `Metadata`. Each block records its source
(`Exif`/`Iptc`/`Xmp`/`Other`), a human label, the raw bytes, and a `preserve`
flag.

- On read, unmapped EXIF tags (maker notes, raw GPS IFD bytes, thumbnail
  location fields we do not surface as properties) are appended as
  `NativeBlock { preserve: true }`.
- On write, blocks with `preserve == true` are written back verbatim into the
  target container when the container supports them; otherwise they are dropped
  and the writer reports it via `MetadataError::UnsupportedWriteTarget` rather
  than silently losing data.
- Projected fields (the `exif.*`/`gps.*` properties) are always regenerated from
  the structured store on write, so a round trip through the model is lossless
  for projected fields and best-effort for native blocks.

## 3. Read precedence

When a `.xmp` sidecar, an embedded XMP packet, IPTC-IIM and EXIF all disagree,
precedence is **source-by-source in this order** (highest first):

1. `.xmp` sidecar packet
2. Embedded XMP packet
3. IPTC-IIM
4. EXIF

Application is field-by-field in effect: a higher-priority source's property
wins for that property; a property absent from the higher source is filled in
from the next source. This is implemented as an ordered merge where later
(lower-priority) sources only insert properties not already present. A future
option to expose per-source provenance on each `Value` is out of scope for v1.

## 4. Write policy (maintainer sign-off required)

Default policy for v1:

- **In-place preferred** where the container format supports embedded XMP
  (JPEG, TIFF/DNG, PNG): write the XMP packet and projected native fields back
  into the file.
- **Sidecar preferred** for RAW and any format where in-place XMP is not safe
  or not supported: write/update the `.xmp` sidecar next to the asset.
- **BridgeCraft never auto-creates a sidecar** unless the user explicitly
  requests sidecar output or the asset is RAW. Existing sidecars are updated in
  place; no sidecar is created for a JPEG/TIFF/PNG that already has embedded
  metadata.
- Policy is exposed as a `WritePolicy` enum (`InPlace`, `Sidecar`, `Auto`)
  passed to writers, defaulting to `Auto` (which resolves per the rules above).

This default is conservative (no surprise sidecar files) and matches the clean-room
"do not surprise the user's filesystem" principle. Sign-off requested because it
affects user-visible file layout.

## Data model

- `Metadata`: `{ properties: BTreeMap<QualifiedName, Value>, native: Vec<NativeBlock> }`.
- `QualifiedName`: `{ namespace: String, name: String }`. Namespace is a URI in
  XMP mode; projection layers may use short stable aliases (e.g. `"exif"`,
  `"gps"`) documented per reader.
- `Value`: `Text | Boolean | Integer | Real | Date (ISO-8601 string) | Uri |
  Array(ArrayKind, Vec<Value>) | AltText(LangAlt) | Struct(BTreeMap<QualifiedName, Value>) | Binary(Vec<u8>)`.
- `ArrayKind`: `Bag` (unordered) | `Seq` (ordered) | `Alt` (alternatives).
- `LangAlt`: `{ entries: BTreeMap<String/*lang*/, String/*text*/> }` (default
  entry keyed by `"x-default"` per XMP).
- `NativeBlock`: `{ source, label, data, preserve }`.

## Traits

- `Reader`: `fn read(&self, data: &[u8]) -> Result<Metadata, MetadataError>` (existing).
- `Writer`: `fn write(&self, data: &[u8], metadata: &Metadata) -> Result<Vec<u8>, MetadataError>`
  plus a default `update` that delegates to `write`. Container-specific writers
  implement this; sidecar writers take an empty `data` slice.

## Error taxonomy

`MetadataError` gains: `UnsupportedType(String)`, `MalformedPacket(String)`,
`UnsupportedWriteTarget(String)`, `InvalidNamespace(String)`, `NotFound`,
alongside the existing `Unsupported`, `Malformed(String)`, `Io`.

## Compatibility

The flat `Metadata = BTreeMap<String, String>` alias is removed; `Metadata` is
now the structured struct. `exif` projection keeps working via
`Metadata::set_text`/`text` under the `"exif"`/`"gps"` namespaces, so the UI's
existing key lookups are updated to the namespaced accessors. Skeleton tests
(`noop_reader_returns_empty`) remain green.

## Open questions

1. XMP canonical? **Yes** (§1).
2. Unmapped EXIF fields? **Native blocks, preserved when possible** (§2).
3. Read precedence? **Source-by-source priority** (§3).
4. Write policy? **In-place preferred, sidecar for RAW, no auto-sidecar** (§4)
   — pending maintainer sign-off.
