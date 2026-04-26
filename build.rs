//! Cargo build script - compiles the vendored emu2413 OPLL core
//! (`vendor/emu2413/`) into a static library that the VRC7 mapper FFI
//! wrapper (`src/mapper/vrc7_opll.rs`) links against. Pure C, no
//! configuration knobs; the `cc` crate picks up `$CC` / MSVC / clang
//! transparently.

fn main() {
    let src = "vendor/emu2413/emu2413.c";
    println!("cargo:rerun-if-changed={src}");
    println!("cargo:rerun-if-changed=vendor/emu2413/emu2413.h");

    let mut build = cc::Build::new();
    build
        .file(src)
        .include("vendor/emu2413")
        .warnings(false)
        .extra_warnings(false);

    // emu2413 is third-party C - silence the noisier diagnostics that
    // would otherwise pollute our build output. These flags are GCC /
    // Clang only; MSVC ignores unknown `-W` flags by default but the
    // `flag_if_supported` gate keeps us safe across all toolchains.
    for flag in [
        "-Wno-unused-parameter",
        "-Wno-unused-variable",
        "-Wno-sign-compare",
        "-Wno-implicit-fallthrough",
    ] {
        build.flag_if_supported(flag);
    }

    build.compile("emu2413");
}
