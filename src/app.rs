// SPDX-License-Identifier: GPL-3.0-or-later
//! Application glue shared between the windowed `vibenes` binary and
//! the headless `test_runner`. Keeps NES wiring in one place so the
//! two don't drift apart.

use anyhow::Result;

use crate::nes::Nes;
use crate::rom::Cartridge;

/// Where completed PPU frames are delivered for presentation.
///
/// `push_frame` receives a borrow of a 256×240 RGBA8 buffer. Implementations
/// must either copy it synchronously or drop it - the PPU owns the buffer
/// and will overwrite it during the next frame.
pub trait FrameSink: Send {
    fn push_frame(&mut self, framebuffer: &[u8]);
}

/// Drops every frame. Used by the headless test runner and in unit tests
/// where presentation is irrelevant.
pub struct NullSink;

impl FrameSink for NullSink {
    fn push_frame(&mut self, _framebuffer: &[u8]) {}
}

/// Central NES construction. Callers hand in a parsed `Cartridge`; we
/// pick the region, build the mapper, reset the CPU, and return a ready-
/// to-step `Nes`. Both binaries route through here so region selection and
/// reset semantics stay consistent.
pub fn build_nes(cart: Cartridge) -> Result<Nes> {
    Nes::from_cartridge(cart)
}
