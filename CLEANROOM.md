# Clean-Room Policy

BridgeCraft is built as a **clean-room reimplementation** of a digital asset
browser/organizer, written entirely in **Rust**. This document defines the rules
that every contributor must follow.

## Scope Statement

BridgeCraft replicates *observable behavior* and *documented, open* file
formats. It does **not** replicate Adobe Bridge's source code, assets, or
textual output.

## Hard Rules — Never Do This

1. **No Adobe source code.** Never view, copy, transcribe, or adapt any Adobe
   Bridge source, binaries, decompiled code, or disassembly — under any
   license, trial, or other arrangement.
2. **No decompilation or reverse engineering** of Adobe software to extract
   logic, algorithms, constants, or resource files.
3. **No copying of Adobe assets**, including icons, UI artwork, fonts, splash
   screens, sound effects, tooltips, or marketing copy.
4. **No pixel-perfect trade dress.** BridgeCraft must have its own distinct UI
   identity. Similar *layout concepts* (e.g., a folder tree, a thumbnail grid)
   are fine — cloning the visual look-and-feel is not.
5. **No proprietary SDKs** (Adobe XMP SDK, Adobe DNG SDK, ACR, etc.). Use
   clean-room Rust implementations or permissively licensed libraries for all
   formats (see Allowed References).
6. **No specification laundering.** If a contributor has been exposed to Adobe
   source code (employment, NDA'd material, etc.), they must not contribute to
   the corresponding subsystem.

## Permitted Sources of Knowledge

The following are considered clean and may be consulted freely:

- Publicly **documented, open formats and specifications**:
  - ISO 16684-1 **XMP** (XML-based metadata)
  - **EXIF** / **IPTC-IIM** / **IPTC Core** photo metadata standards
  - **DNG** specification (Adobe-published, royalty-free, freely implementable)
  - TIFF/EP, PDF (ISO 32000), MP4/ISOBMFF (ISO 14496), PNG, JPEG/JFIF
  - INDD/PSD community-documented reverse-engineered format references
    (public, third-party documented only)
- Standard public documentation: Rust std, OS APIs (Win32, Cocoa via bindings,
  FUSE/inotify/FSEvents/ReadDirectoryChangesW), databases, GPU APIs.
- **Behavioral observation**: you may *use* a file browser application as an
  ordinary user and take notes on user-visible behavior ("grid supports drag
  into folders", "F2 renames"). You may not disassemble or debug it.
- Feature checklists written from memory of using software as a user.

## Implementation Rules

- All code is **Rust** (stable toolchain). FFI bindings to C libraries are
  permitted only for permissively licensed, cleanly licensed third-party
  libraries (e.g., libraw, ffmpeg, lcms2) — never for anything shipped by
  Adobe in Adobe products.
- All strings, icons, and UI copy in BridgeCraft must be **authored
  originally** for this project.
- Any vendored dependency must carry a license compatible with the project and
  be recorded in `THIRD_PARTY_LICENSES.md`.

## Contribution Gate

Before contributing, every contributor must confirm in their PR (or signed
commit) that they have **not** had access to Adobe Bridge source code or
internal materials. A `.github/CONTRIBUTING.md` checklist covers this.

## Enforcement

Suspected violations (Adobe-derived code, copied assets, laundered specs) must
be reported to the maintainers. Content in violation is removed immediately and
the subsystem is rewritten from clean sources.

---

*This policy exists to protect the project and its contributors. When in
doubt, ask before you look.*
