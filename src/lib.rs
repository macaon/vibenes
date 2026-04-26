// SPDX-License-Identifier: GPL-3.0-or-later
//! vibenes - cycle-accurate retro emulator cores in Rust.
//!
//! Layout: each console core lives under its own top-level module
//! (`nes`, eventually `snes`). Cross-core infrastructure (`audio`,
//! `gfx`, `ui`, `save`, `settings`, `gamedb`, `crc32`, `core`) sits
//! at the crate root and is shared. The `core` module defines the
//! abstract trait every console implements; `app` wires a concrete
//! core instance to the host (window, audio sink, input).
pub mod app;
pub mod audio;
pub mod blargg_2005_scan;
pub mod config;
pub mod core;
pub mod crc32;
pub mod debug_overlay;
pub mod gamedb;
pub mod gfx;
pub mod nes;
pub mod save;
pub mod snes;
pub mod settings;
pub mod ui;
pub mod video;
