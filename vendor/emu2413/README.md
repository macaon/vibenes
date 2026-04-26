emu2413 - Yamaha YM2413 (OPLL) emulator
=======================================

Vendored library, not part of vibenes' first-party source.

- **Upstream**: https://github.com/digital-sound-antiques/emu2413
- **Author**: Mitsutaka Okazaki
- **Version**: v1.5.9 (2022-09-21)
- **License**: MIT - see `LICENSE`. Used unmodified.

Purpose
-------

Provides the YM2413 / VRC7 FM synth core consumed by mapper 85 (Konami
VRC7) - `src/mapper/vrc7.rs` calls into it through the FFI bindings in
`src/mapper/vrc7_opll.rs`. The same chip is the de facto reference
implementation in Mesen2, mGBA, and several MSX/SMS emulators.

Update procedure
----------------

1. Pull `emu2413.c` and `emu2413.h` from the upstream tag.
2. Refresh `LICENSE` and `CHANGELOG.md` if they changed.
3. Bump the version line above.
4. Rebuild and re-run the APU regression sweep + a VRC7 ROM (Lagrange
   Point) to confirm no audible regression.

The Rust side never modifies these files; if a local fix is needed, add
a `*.patch` next to this README and apply it from `build.rs`.
