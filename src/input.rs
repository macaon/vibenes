// SPDX-License-Identifier: GPL-3.0-or-later
//! Input bindings and runtime for keyboard + gamepad → NES controllers.
//!
//! Replaces the hardcoded keyboard / gilrs polling that used to live
//! in `src/main.rs`. The user-visible model is:
//!
//! - **Player 1** always has the keyboard, plus the first detected
//!   gamepad if any. Defaults match the historical layout (Z=B, X=A,
//!   arrows = D-pad, Enter = Start, RShift = Select).
//! - **Player 2** is gamepad-only; never assigned by default. Hot-plug
//!   rules (see [`InputRuntime::on_gamepad_connected`]) auto-assign a
//!   second controller to P2 *only if* P1 already has a gamepad - so
//!   "first controller plugged in goes to P1" is the predictable rule
//!   no matter what order devices show up.
//! - **Sticky UUID assignments.** Once a slot is bound to a gamepad's
//!   UUID, that binding survives disconnect / reconnect; manual
//!   reassignment is the only way to break it.
//!
//! The data model intentionally separates *what fires the button*
//! ([`PhysicalSource`]) from *which gamepad is yours*
//! ([`PlayerInput::gamepad_uuid`]). A [`PhysicalSource::GamepadButton`]
//! binding doesn't carry a UUID - the runtime only consults bindings
//! for the player's currently-resolved pad. That keeps bindings
//! portable across controllers (rebind once, every controller you
//! ever plug into the same slot uses it).
//!
//! ## On-disk format (`~/.config/vibenes/input.toml`)
//!
//! ```toml
//! turbo_rate_hz = 30
//!
//! [p1]
//! gamepad_uuid = "030000005e040000-..."  # optional
//!
//! [p1.bindings]
//! A      = ["key:KeyX",       "gamepad:South"]
//! B      = ["key:KeyZ",       "gamepad:West"]
//! Select = ["key:ShiftRight", "gamepad:Select"]
//! Start  = ["key:Enter",      "gamepad:Start"]
//! Up     = ["key:ArrowUp",    "gamepad:DPadUp",    "axis:LeftStickY-"]
//! Down   = ["key:ArrowDown",  "gamepad:DPadDown",  "axis:LeftStickY+"]
//! Left   = ["key:ArrowLeft",  "gamepad:DPadLeft",  "axis:LeftStickX-"]
//! Right  = ["key:ArrowRight", "gamepad:DPadRight", "axis:LeftStickX+"]
//!
//! [p2]
//! [p2.bindings]
//! A      = ["gamepad:South"]
//! # ... etc, no keyboard sources by default
//! ```
//!
//! Source-string grammar:
//! - `key:<name>`     - winit `KeyCode` discriminant name
//! - `gamepad:<name>` - gilrs `Button` discriminant name (excluding `Mode`)
//! - `axis:<name><sign>` - gilrs `Axis` name + literal `+` or `-`

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use winit::keyboard::KeyCode;

/// Logical NES controller buttons. The first eight match the live
/// shifter bit order (A=LSB through Right=MSB) - see
/// [`crate::nes::controller::Controller`]. The two turbo variants
/// trigger the same physical NES bit (A or B respectively) but are
/// modulated by the configured turbo rate.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub enum NesButton {
    A,
    B,
    Select,
    Start,
    Up,
    Down,
    Left,
    Right,
    TurboA,
    TurboB,
}

impl NesButton {
    /// Bit position in the controller shifter for the underlying NES
    /// signal. `TurboA` and `TurboB` share bits with `A` and `B`
    /// respectively - the distinction lives only at the input-routing
    /// layer.
    pub fn shifter_bit(self) -> u8 {
        match self {
            Self::A | Self::TurboA => 0x01,
            Self::B | Self::TurboB => 0x02,
            Self::Select => 0x04,
            Self::Start => 0x08,
            Self::Up => 0x10,
            Self::Down => 0x20,
            Self::Left => 0x40,
            Self::Right => 0x80,
        }
    }

    pub fn is_turbo(self) -> bool {
        matches!(self, Self::TurboA | Self::TurboB)
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::A => "A",
            Self::B => "B",
            Self::Select => "Select",
            Self::Start => "Start",
            Self::Up => "Up",
            Self::Down => "Down",
            Self::Left => "Left",
            Self::Right => "Right",
            Self::TurboA => "TurboA",
            Self::TurboB => "TurboB",
        }
    }

    pub fn all() -> &'static [NesButton] {
        &[
            Self::A,
            Self::B,
            Self::Select,
            Self::Start,
            Self::Up,
            Self::Down,
            Self::Left,
            Self::Right,
            Self::TurboA,
            Self::TurboB,
        ]
    }
}

impl FromStr for NesButton {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        Ok(match s {
            "A" => Self::A,
            "B" => Self::B,
            "Select" => Self::Select,
            "Start" => Self::Start,
            "Up" => Self::Up,
            "Down" => Self::Down,
            "Left" => Self::Left,
            "Right" => Self::Right,
            "TurboA" => Self::TurboA,
            "TurboB" => Self::TurboB,
            _ => return Err(anyhow!("unknown NES button {s:?}")),
        })
    }
}

/// Sign-of-axis tag for analog stick bindings. `LeftStickY-` means
/// "fire when LeftStickY < -threshold" (the user has pushed the
/// stick toward the negative end of that axis). gilrs reports
/// LeftStickY values as `-1.0` when fully down on most pads -
/// which is the opposite of what most people expect, so the binding
/// strings encode the *physical* sign, not "up vs down."
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AxisSign {
    Positive,
    Negative,
}

/// Default axis threshold (fraction of full deflection) below which
/// a stick is considered "not pressed." 50% matches the old
/// hardcoded `DEADBAND` and works for every gamepad we know of.
pub const AXIS_THRESHOLD: f32 = 0.5;

/// One physical input that can fire a [`NesButton`]. A binding may
/// list multiple sources; the first one currently held wins
/// (logical OR).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PhysicalSource {
    /// Keyboard key, identified by its winit `KeyCode` discriminant
    /// name (e.g. `"KeyZ"`, `"ArrowUp"`, `"ShiftRight"`).
    Key(KeyName),
    /// Digital gamepad button. UUID-agnostic - resolved against the
    /// player's currently-assigned pad at poll time.
    GamepadButton(GamepadButton),
    /// Analog axis treated as a digital input via a deflection
    /// threshold ([`AXIS_THRESHOLD`]).
    GamepadAxis { axis: GamepadAxis, sign: AxisSign },
}

impl PhysicalSource {
    /// Encode as the compact TOML-friendly string used in
    /// `input.toml`. See module docs for the grammar.
    pub fn encode(&self) -> String {
        match self {
            Self::Key(k) => format!("key:{}", k.name()),
            Self::GamepadButton(b) => format!("gamepad:{}", b.name()),
            Self::GamepadAxis { axis, sign } => {
                let s = match sign {
                    AxisSign::Positive => "+",
                    AxisSign::Negative => "-",
                };
                format!("axis:{}{s}", axis.name())
            }
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        if let Some(rest) = s.strip_prefix("key:") {
            return Ok(Self::Key(KeyName::from_name(rest)?));
        }
        if let Some(rest) = s.strip_prefix("gamepad:") {
            return Ok(Self::GamepadButton(GamepadButton::from_name(rest)?));
        }
        if let Some(rest) = s.strip_prefix("axis:") {
            // Last char is sign.
            let (axis_part, sign_part) = rest.split_at(rest.len().saturating_sub(1));
            let sign = match sign_part {
                "+" => AxisSign::Positive,
                "-" => AxisSign::Negative,
                other => {
                    return Err(anyhow!(
                        "axis source must end in '+' or '-', got {other:?}"
                    ))
                }
            };
            return Ok(Self::GamepadAxis {
                axis: GamepadAxis::from_name(axis_part)?,
                sign,
            });
        }
        Err(anyhow!(
            "unknown source kind in {s:?}; expected key:..., gamepad:..., or axis:..."
        ))
    }
}

// ============================================================
// KeyName - subset of winit::KeyCode that we care about
// ============================================================

/// Wrapper over the subset of [`winit::keyboard::KeyCode`] we let
/// users bind to NES buttons. We don't expose modifier keys
/// (Ctrl / Alt / Meta) because they're reserved for future shortcut
/// chords (load-state, fullscreen, etc.).
///
/// Round-trips via [`KeyName::name`] / [`KeyName::from_name`] using
/// the discriminant name (e.g. `"KeyZ"`, `"ArrowUp"`). That string
/// matches `KeyCode`'s `Debug` format, which lets users edit the
/// TOML by reading their own emulator's key-event logs and pasting
/// the name verbatim.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct KeyName(KeyCode);

impl KeyName {
    pub fn from_winit(code: KeyCode) -> Self {
        Self(code)
    }

    pub fn to_winit(self) -> KeyCode {
        self.0
    }

    pub fn name(self) -> &'static str {
        // Hand-rolled to keep the on-disk string stable across winit
        // version bumps - we don't want winit's Debug format silently
        // changing under us. Only the keys we permit are listed here;
        // everything else round-trips via the catch-all "Unknown".
        keyname_str(self.0)
    }

    pub fn from_name(s: &str) -> Result<Self> {
        keyname_lookup(s)
            .map(Self)
            .ok_or_else(|| anyhow!("unknown key name {s:?}"))
    }
}

/// Static map of supported keys. Update both halves together when
/// adding a new key (the `from_name` reverse lookup walks the same
/// list). Listed in roughly QWERTY-row order so additions cluster.
const KEY_TABLE: &[(&str, KeyCode)] = &[
    // Letters
    ("KeyA", KeyCode::KeyA),
    ("KeyB", KeyCode::KeyB),
    ("KeyC", KeyCode::KeyC),
    ("KeyD", KeyCode::KeyD),
    ("KeyE", KeyCode::KeyE),
    ("KeyF", KeyCode::KeyF),
    ("KeyG", KeyCode::KeyG),
    ("KeyH", KeyCode::KeyH),
    ("KeyI", KeyCode::KeyI),
    ("KeyJ", KeyCode::KeyJ),
    ("KeyK", KeyCode::KeyK),
    ("KeyL", KeyCode::KeyL),
    ("KeyM", KeyCode::KeyM),
    ("KeyN", KeyCode::KeyN),
    ("KeyO", KeyCode::KeyO),
    ("KeyP", KeyCode::KeyP),
    ("KeyQ", KeyCode::KeyQ),
    ("KeyR", KeyCode::KeyR),
    ("KeyS", KeyCode::KeyS),
    ("KeyT", KeyCode::KeyT),
    ("KeyU", KeyCode::KeyU),
    ("KeyV", KeyCode::KeyV),
    ("KeyW", KeyCode::KeyW),
    ("KeyX", KeyCode::KeyX),
    ("KeyY", KeyCode::KeyY),
    ("KeyZ", KeyCode::KeyZ),
    // Digits
    ("Digit0", KeyCode::Digit0),
    ("Digit1", KeyCode::Digit1),
    ("Digit2", KeyCode::Digit2),
    ("Digit3", KeyCode::Digit3),
    ("Digit4", KeyCode::Digit4),
    ("Digit5", KeyCode::Digit5),
    ("Digit6", KeyCode::Digit6),
    ("Digit7", KeyCode::Digit7),
    ("Digit8", KeyCode::Digit8),
    ("Digit9", KeyCode::Digit9),
    // Arrows
    ("ArrowUp", KeyCode::ArrowUp),
    ("ArrowDown", KeyCode::ArrowDown),
    ("ArrowLeft", KeyCode::ArrowLeft),
    ("ArrowRight", KeyCode::ArrowRight),
    // Misc useful keys for binding (not counting modifiers we reserve
    // for future shortcuts).
    ("Enter", KeyCode::Enter),
    ("Space", KeyCode::Space),
    ("Tab", KeyCode::Tab),
    ("Backspace", KeyCode::Backspace),
    ("Escape", KeyCode::Escape),
    ("ShiftLeft", KeyCode::ShiftLeft),
    ("ShiftRight", KeyCode::ShiftRight),
    // Numpad - some users prefer it for P2-style layouts in the future.
    ("Numpad0", KeyCode::Numpad0),
    ("Numpad1", KeyCode::Numpad1),
    ("Numpad2", KeyCode::Numpad2),
    ("Numpad3", KeyCode::Numpad3),
    ("Numpad4", KeyCode::Numpad4),
    ("Numpad5", KeyCode::Numpad5),
    ("Numpad6", KeyCode::Numpad6),
    ("Numpad7", KeyCode::Numpad7),
    ("Numpad8", KeyCode::Numpad8),
    ("Numpad9", KeyCode::Numpad9),
    ("NumpadEnter", KeyCode::NumpadEnter),
];

fn keyname_str(code: KeyCode) -> &'static str {
    for (name, c) in KEY_TABLE {
        if *c == code {
            return name;
        }
    }
    "Unknown"
}

fn keyname_lookup(name: &str) -> Option<KeyCode> {
    KEY_TABLE.iter().find(|(n, _)| *n == name).map(|(_, c)| *c)
}

// ============================================================
// GamepadButton - mirror of gilrs::Button (excluding Mode)
// ============================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GamepadButton {
    South,       // Xbox A, PS Cross, Switch B (physical-position naming)
    East,        // Xbox B, PS Circle, Switch A
    North,       // Xbox Y, PS Triangle, Switch X
    West,        // Xbox X, PS Square, Switch Y
    LeftTrigger,
    RightTrigger,
    LeftTrigger2,  // analog L2 / LT
    RightTrigger2, // analog R2 / RT
    Select,        // Xbox Back/View, PS Share, Switch Minus
    Start,         // Xbox Menu, PS Options, Switch Plus
    LeftThumb,     // L3 / left-stick click
    RightThumb,    // R3 / right-stick click
    DPadUp,
    DPadDown,
    DPadLeft,
    DPadRight,
    // We deliberately omit `Mode` - it's reserved for menu-toggle
    // and isn't a NES-binding-eligible source.
}

impl GamepadButton {
    pub fn name(self) -> &'static str {
        match self {
            Self::South => "South",
            Self::East => "East",
            Self::North => "North",
            Self::West => "West",
            Self::LeftTrigger => "LeftTrigger",
            Self::RightTrigger => "RightTrigger",
            Self::LeftTrigger2 => "LeftTrigger2",
            Self::RightTrigger2 => "RightTrigger2",
            Self::Select => "Select",
            Self::Start => "Start",
            Self::LeftThumb => "LeftThumb",
            Self::RightThumb => "RightThumb",
            Self::DPadUp => "DPadUp",
            Self::DPadDown => "DPadDown",
            Self::DPadLeft => "DPadLeft",
            Self::DPadRight => "DPadRight",
        }
    }

    pub fn from_name(s: &str) -> Result<Self> {
        Ok(match s {
            "South" => Self::South,
            "East" => Self::East,
            "North" => Self::North,
            "West" => Self::West,
            "LeftTrigger" => Self::LeftTrigger,
            "RightTrigger" => Self::RightTrigger,
            "LeftTrigger2" => Self::LeftTrigger2,
            "RightTrigger2" => Self::RightTrigger2,
            "Select" => Self::Select,
            "Start" => Self::Start,
            "LeftThumb" => Self::LeftThumb,
            "RightThumb" => Self::RightThumb,
            "DPadUp" => Self::DPadUp,
            "DPadDown" => Self::DPadDown,
            "DPadLeft" => Self::DPadLeft,
            "DPadRight" => Self::DPadRight,
            _ => return Err(anyhow!("unknown gamepad button {s:?}")),
        })
    }

    pub fn to_gilrs(self) -> gilrs::Button {
        match self {
            Self::South => gilrs::Button::South,
            Self::East => gilrs::Button::East,
            Self::North => gilrs::Button::North,
            Self::West => gilrs::Button::West,
            Self::LeftTrigger => gilrs::Button::LeftTrigger,
            Self::RightTrigger => gilrs::Button::RightTrigger,
            Self::LeftTrigger2 => gilrs::Button::LeftTrigger2,
            Self::RightTrigger2 => gilrs::Button::RightTrigger2,
            Self::Select => gilrs::Button::Select,
            Self::Start => gilrs::Button::Start,
            Self::LeftThumb => gilrs::Button::LeftThumb,
            Self::RightThumb => gilrs::Button::RightThumb,
            Self::DPadUp => gilrs::Button::DPadUp,
            Self::DPadDown => gilrs::Button::DPadDown,
            Self::DPadLeft => gilrs::Button::DPadLeft,
            Self::DPadRight => gilrs::Button::DPadRight,
        }
    }

    pub fn from_gilrs(b: gilrs::Button) -> Option<Self> {
        Some(match b {
            gilrs::Button::South => Self::South,
            gilrs::Button::East => Self::East,
            gilrs::Button::North => Self::North,
            gilrs::Button::West => Self::West,
            gilrs::Button::LeftTrigger => Self::LeftTrigger,
            gilrs::Button::RightTrigger => Self::RightTrigger,
            gilrs::Button::LeftTrigger2 => Self::LeftTrigger2,
            gilrs::Button::RightTrigger2 => Self::RightTrigger2,
            gilrs::Button::Select => Self::Select,
            gilrs::Button::Start => Self::Start,
            gilrs::Button::LeftThumb => Self::LeftThumb,
            gilrs::Button::RightThumb => Self::RightThumb,
            gilrs::Button::DPadUp => Self::DPadUp,
            gilrs::Button::DPadDown => Self::DPadDown,
            gilrs::Button::DPadLeft => Self::DPadLeft,
            gilrs::Button::DPadRight => Self::DPadRight,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GamepadAxis {
    LeftStickX,
    LeftStickY,
    RightStickX,
    RightStickY,
    LeftZ,
    RightZ,
    DPadX,
    DPadY,
}

impl GamepadAxis {
    pub fn name(self) -> &'static str {
        match self {
            Self::LeftStickX => "LeftStickX",
            Self::LeftStickY => "LeftStickY",
            Self::RightStickX => "RightStickX",
            Self::RightStickY => "RightStickY",
            Self::LeftZ => "LeftZ",
            Self::RightZ => "RightZ",
            Self::DPadX => "DPadX",
            Self::DPadY => "DPadY",
        }
    }

    pub fn from_name(s: &str) -> Result<Self> {
        Ok(match s {
            "LeftStickX" => Self::LeftStickX,
            "LeftStickY" => Self::LeftStickY,
            "RightStickX" => Self::RightStickX,
            "RightStickY" => Self::RightStickY,
            "LeftZ" => Self::LeftZ,
            "RightZ" => Self::RightZ,
            "DPadX" => Self::DPadX,
            "DPadY" => Self::DPadY,
            _ => return Err(anyhow!("unknown gamepad axis {s:?}")),
        })
    }

    pub fn to_gilrs(self) -> gilrs::Axis {
        match self {
            Self::LeftStickX => gilrs::Axis::LeftStickX,
            Self::LeftStickY => gilrs::Axis::LeftStickY,
            Self::RightStickX => gilrs::Axis::RightStickX,
            Self::RightStickY => gilrs::Axis::RightStickY,
            Self::LeftZ => gilrs::Axis::LeftZ,
            Self::RightZ => gilrs::Axis::RightZ,
            Self::DPadX => gilrs::Axis::DPadX,
            Self::DPadY => gilrs::Axis::DPadY,
        }
    }
}

// ============================================================
// GamepadUuid - 16-byte wrapper, hex-string serialization
// ============================================================

/// Stable identifier for a controller across reconnects. Wraps the
/// 16-byte UUID gilrs reports (`Gamepad::uuid()`); we render as the
/// standard 8-4-4-4-12 hex form for the TOML.
///
/// Some Bluetooth controllers ship literal-zero UUIDs; we treat
/// those as "non-sticky" (the runtime won't restore them across
/// reconnects, only honor them within a session). Real hardware
/// from any brand we've tested ships a non-zero UUID.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GamepadUuid([u8; 16]);

impl GamepadUuid {
    pub fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    pub fn bytes(&self) -> [u8; 16] {
        self.0
    }

    pub fn is_zero(&self) -> bool {
        self.0 == [0u8; 16]
    }

    pub fn to_hex(&self) -> String {
        let b = &self.0;
        format!(
            "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
            b[0], b[1], b[2], b[3],
            b[4], b[5],
            b[6], b[7],
            b[8], b[9],
            b[10], b[11], b[12], b[13], b[14], b[15],
        )
    }

    pub fn from_hex(s: &str) -> Result<Self> {
        let trimmed: String = s.chars().filter(|c| *c != '-').collect();
        if trimmed.len() != 32 {
            return Err(anyhow!(
                "gamepad UUID must be 32 hex chars (with optional dashes); got {}",
                trimmed.len()
            ));
        }
        let mut bytes = [0u8; 16];
        for i in 0..16 {
            let hex = &trimmed[i * 2..i * 2 + 2];
            bytes[i] = u8::from_str_radix(hex, 16)
                .with_context(|| format!("invalid hex in UUID at byte {i}: {hex:?}"))?;
        }
        Ok(Self(bytes))
    }
}

impl Serialize for GamepadUuid {
    fn serialize<S: serde::Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        ser.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for GamepadUuid {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let s = String::deserialize(de)?;
        Self::from_hex(&s).map_err(serde::de::Error::custom)
    }
}

// ============================================================
// PlayerInput / InputConfig - serializable config
// ============================================================

#[derive(Debug, Clone, Default)]
pub struct PlayerInput {
    pub bindings: BTreeMap<NesButton, Vec<PhysicalSource>>,
    pub gamepad_uuid: Option<GamepadUuid>,
}

#[derive(Debug, Clone)]
pub struct InputConfig {
    pub p1: PlayerInput,
    pub p2: PlayerInput,
    pub turbo_rate_hz: u8,
}

impl Default for InputConfig {
    fn default() -> Self {
        Self {
            p1: default_p1(),
            p2: default_p2(),
            turbo_rate_hz: 30,
        }
    }
}

fn default_p1() -> PlayerInput {
    let mut b: BTreeMap<NesButton, Vec<PhysicalSource>> = BTreeMap::new();
    // Keyboard - matches the historical hardcoded layout.
    b.insert(
        NesButton::A,
        vec![
            PhysicalSource::Key(KeyName(KeyCode::KeyX)),
            PhysicalSource::GamepadButton(GamepadButton::South),
        ],
    );
    b.insert(
        NesButton::B,
        vec![
            PhysicalSource::Key(KeyName(KeyCode::KeyZ)),
            PhysicalSource::GamepadButton(GamepadButton::West),
        ],
    );
    b.insert(
        NesButton::Select,
        vec![
            PhysicalSource::Key(KeyName(KeyCode::ShiftRight)),
            PhysicalSource::GamepadButton(GamepadButton::Select),
        ],
    );
    b.insert(
        NesButton::Start,
        vec![
            PhysicalSource::Key(KeyName(KeyCode::Enter)),
            PhysicalSource::GamepadButton(GamepadButton::Start),
        ],
    );
    b.insert(
        NesButton::Up,
        vec![
            PhysicalSource::Key(KeyName(KeyCode::ArrowUp)),
            PhysicalSource::GamepadButton(GamepadButton::DPadUp),
            PhysicalSource::GamepadAxis {
                axis: GamepadAxis::LeftStickY,
                sign: AxisSign::Positive,
            },
        ],
    );
    b.insert(
        NesButton::Down,
        vec![
            PhysicalSource::Key(KeyName(KeyCode::ArrowDown)),
            PhysicalSource::GamepadButton(GamepadButton::DPadDown),
            PhysicalSource::GamepadAxis {
                axis: GamepadAxis::LeftStickY,
                sign: AxisSign::Negative,
            },
        ],
    );
    b.insert(
        NesButton::Left,
        vec![
            PhysicalSource::Key(KeyName(KeyCode::ArrowLeft)),
            PhysicalSource::GamepadButton(GamepadButton::DPadLeft),
            PhysicalSource::GamepadAxis {
                axis: GamepadAxis::LeftStickX,
                sign: AxisSign::Negative,
            },
        ],
    );
    b.insert(
        NesButton::Right,
        vec![
            PhysicalSource::Key(KeyName(KeyCode::ArrowRight)),
            PhysicalSource::GamepadButton(GamepadButton::DPadRight),
            PhysicalSource::GamepadAxis {
                axis: GamepadAxis::LeftStickX,
                sign: AxisSign::Positive,
            },
        ],
    );
    // Turbo: defined as logical buttons but unbound by default.
    b.insert(NesButton::TurboA, vec![]);
    b.insert(NesButton::TurboB, vec![]);
    PlayerInput {
        bindings: b,
        gamepad_uuid: None,
    }
}

fn default_p2() -> PlayerInput {
    // Same gamepad layout as P1, no keyboard. Slot stays "no
    // gamepad assigned" until the hot-plug rule fires.
    let mut b: BTreeMap<NesButton, Vec<PhysicalSource>> = BTreeMap::new();
    b.insert(NesButton::A, vec![PhysicalSource::GamepadButton(GamepadButton::South)]);
    b.insert(NesButton::B, vec![PhysicalSource::GamepadButton(GamepadButton::West)]);
    b.insert(NesButton::Select, vec![PhysicalSource::GamepadButton(GamepadButton::Select)]);
    b.insert(NesButton::Start, vec![PhysicalSource::GamepadButton(GamepadButton::Start)]);
    b.insert(
        NesButton::Up,
        vec![
            PhysicalSource::GamepadButton(GamepadButton::DPadUp),
            PhysicalSource::GamepadAxis {
                axis: GamepadAxis::LeftStickY,
                sign: AxisSign::Positive,
            },
        ],
    );
    b.insert(
        NesButton::Down,
        vec![
            PhysicalSource::GamepadButton(GamepadButton::DPadDown),
            PhysicalSource::GamepadAxis {
                axis: GamepadAxis::LeftStickY,
                sign: AxisSign::Negative,
            },
        ],
    );
    b.insert(
        NesButton::Left,
        vec![
            PhysicalSource::GamepadButton(GamepadButton::DPadLeft),
            PhysicalSource::GamepadAxis {
                axis: GamepadAxis::LeftStickX,
                sign: AxisSign::Negative,
            },
        ],
    );
    b.insert(
        NesButton::Right,
        vec![
            PhysicalSource::GamepadButton(GamepadButton::DPadRight),
            PhysicalSource::GamepadAxis {
                axis: GamepadAxis::LeftStickX,
                sign: AxisSign::Positive,
            },
        ],
    );
    b.insert(NesButton::TurboA, vec![]);
    b.insert(NesButton::TurboB, vec![]);
    PlayerInput {
        bindings: b,
        gamepad_uuid: None,
    }
}

// ============================================================
// TOML serialization - hand-rolled to keep the file readable
// ============================================================

#[derive(Debug, Serialize, Deserialize)]
struct TomlConfig {
    turbo_rate_hz: u8,
    p1: TomlPlayer,
    p2: TomlPlayer,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct TomlPlayer {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    gamepad_uuid: Option<GamepadUuid>,
    #[serde(default)]
    bindings: BTreeMap<String, Vec<String>>,
}

impl InputConfig {
    pub fn to_toml(&self) -> String {
        let cfg = TomlConfig {
            turbo_rate_hz: self.turbo_rate_hz,
            p1: player_to_toml(&self.p1),
            p2: player_to_toml(&self.p2),
        };
        toml::to_string_pretty(&cfg).unwrap_or_else(|e| {
            // Should never fail for our shape; surface the bug rather
            // than silently lose the config.
            format!("# vibenes: failed to serialize input config: {e}\n")
        })
    }

    pub fn from_toml(s: &str) -> Result<Self> {
        let cfg: TomlConfig = toml::from_str(s).context("parsing input.toml")?;
        Ok(Self {
            p1: player_from_toml(&cfg.p1)?,
            p2: player_from_toml(&cfg.p2)?,
            turbo_rate_hz: cfg.turbo_rate_hz,
        })
    }

    /// Default path: `$XDG_CONFIG_HOME/vibenes/input.toml` (or the
    /// `$HOME/.config/...` fallback). Same resolution as
    /// [`crate::save::saves_dir`] but with a different filename.
    pub fn default_path() -> Option<PathBuf> {
        crate::save::saves_dir()
            .and_then(|d| d.parent().map(|p| p.join("input.toml")))
    }

    /// Eagerly load from disk, or write defaults if no file exists.
    /// Malformed files log a warn and fall back to defaults *without*
    /// overwriting the user's file (so a typo doesn't silently destroy
    /// their bindings).
    pub fn load_or_init(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(s) => match Self::from_toml(&s) {
                Ok(cfg) => cfg,
                Err(e) => {
                    log::warn!(
                        "input.toml at {} is malformed ({e}); using defaults without overwriting",
                        path.display()
                    );
                    Self::default()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let cfg = Self::default();
                if let Err(e) = cfg.save(path) {
                    log::warn!(
                        "could not write default input.toml to {}: {e}",
                        path.display()
                    );
                }
                cfg
            }
            Err(e) => {
                log::warn!(
                    "could not read input.toml at {}: {e}; using defaults",
                    path.display()
                );
                Self::default()
            }
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).with_context(|| {
                    format!("creating input config dir {}", parent.display())
                })?;
            }
        }
        std::fs::write(path, self.to_toml())
            .with_context(|| format!("writing input.toml at {}", path.display()))
    }
}

fn player_to_toml(p: &PlayerInput) -> TomlPlayer {
    let mut bindings: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (btn, sources) in &p.bindings {
        let v: Vec<String> = sources.iter().map(|s| s.encode()).collect();
        bindings.insert(btn.name().to_string(), v);
    }
    TomlPlayer {
        gamepad_uuid: p.gamepad_uuid,
        bindings,
    }
}

fn player_from_toml(p: &TomlPlayer) -> Result<PlayerInput> {
    let mut bindings = BTreeMap::new();
    for (k, v) in &p.bindings {
        let nb = NesButton::from_str(k)?;
        let sources: Result<Vec<PhysicalSource>> =
            v.iter().map(|s| PhysicalSource::parse(s)).collect();
        bindings.insert(nb, sources?);
    }
    Ok(PlayerInput {
        bindings,
        gamepad_uuid: p.gamepad_uuid,
    })
}

// ============================================================
// InputRuntime - holds gilrs + applied state
// ============================================================

/// Output of a hot-plug event: what the host should toast (or log).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HotplugNotice {
    /// A controller landed in the named slot (1 or 2). The string
    /// is the gamepad's friendly name (`Gamepad::name()`).
    Assigned { player: u8, name: String },
    /// A controller plugged in but neither slot was free; surfaced
    /// to log only so we can debug "why isn't my third gamepad
    /// doing anything." No toast.
    Ignored { name: String },
    /// A controller was disconnected. Log-only.
    Disconnected { player: Option<u8>, name: String },
}

pub struct InputRuntime {
    cfg: InputConfig,
    cfg_path: Option<PathBuf>,
    gilrs: Option<gilrs::Gilrs>,
    /// Keyboard state. Updated on every winit key event.
    keys_held: HashMap<KeyCode, bool>,
    /// Cached resolution of P1's UUID -> live gilrs id (or None
    /// when the assigned pad is currently disconnected).
    p1_pad_id: Option<gilrs::GamepadId>,
    p2_pad_id: Option<gilrs::GamepadId>,
    /// Rolling frame counter, used to drive turbo oscillators.
    /// Wraps freely - we only care about the period modulus.
    frame_counter: u32,
}

impl InputRuntime {
    pub fn new(cfg: InputConfig, cfg_path: Option<PathBuf>) -> Self {
        let gilrs = match gilrs::Gilrs::new() {
            Ok(g) => Some(g),
            Err(e) => {
                log::warn!("gilrs init failed: {e}; gamepad input disabled");
                None
            }
        };
        let mut rt = Self {
            cfg,
            cfg_path,
            gilrs,
            keys_held: HashMap::new(),
            p1_pad_id: None,
            p2_pad_id: None,
            frame_counter: 0,
        };
        // Apply the boot-time auto-assignment rule to whatever
        // gilrs already enumerated at startup.
        rt.resolve_initial_pads();
        rt
    }

    /// Walk gilrs's startup list and apply the assignment rules in
    /// detection order. Sticky UUIDs win over detection order, so
    /// "the Xbox controller is always P1" survives even if it
    /// happens to enumerate second this session.
    fn resolve_initial_pads(&mut self) {
        let Some(g) = self.gilrs.as_ref() else { return };
        // First pass: sticky UUIDs. Each device is claimed by at
        // most one slot - the `continue` after a P1 match prevents
        // duplicate-UUID controllers (two 8BitDos of the same
        // model both ship UUID X) from collapsing onto a single
        // slot. The slot-empty guards mirror the runtime
        // `on_connect` rule 1.
        let connected: Vec<(gilrs::GamepadId, GamepadUuid, String)> = g
            .gamepads()
            .filter_map(|(id, pad)| {
                let uuid = GamepadUuid::from_bytes(pad.uuid());
                if uuid.is_zero() {
                    return None;
                }
                Some((id, uuid, pad.name().to_string()))
            })
            .collect();
        for (id, uuid, _name) in &connected {
            if Some(*uuid) == self.cfg.p1.gamepad_uuid && self.p1_pad_id.is_none() {
                self.p1_pad_id = Some(*id);
                continue;
            }
            if Some(*uuid) == self.cfg.p2.gamepad_uuid && self.p2_pad_id.is_none() {
                self.p2_pad_id = Some(*id);
            }
        }
        // Second pass: auto-assign the unassigned slots, in detection
        // order. Skip pads we just resolved via sticky UUID, and skip
        // devices that are almost certainly not real controllers (see
        // `name_blocks_auto_assign`).
        for (id, uuid, name) in &connected {
            if Some(*id) == self.p1_pad_id || Some(*id) == self.p2_pad_id {
                continue;
            }
            if name_blocks_auto_assign(name) {
                continue;
            }
            if self.cfg.p1.gamepad_uuid.is_none() && self.p1_pad_id.is_none() {
                self.cfg.p1.gamepad_uuid = Some(*uuid);
                self.p1_pad_id = Some(*id);
                self.persist_quietly();
                continue;
            }
            // Rule: P2 only fills if P1 has a pad.
            let p1_has_pad = self.p1_pad_id.is_some();
            if p1_has_pad
                && self.cfg.p2.gamepad_uuid.is_none()
                && self.p2_pad_id.is_none()
            {
                self.cfg.p2.gamepad_uuid = Some(*uuid);
                self.p2_pad_id = Some(*id);
                self.persist_quietly();
            }
        }
    }

    fn persist_quietly(&self) {
        if let Some(path) = self.cfg_path.as_ref() {
            if let Err(e) = self.cfg.save(path) {
                log::warn!("could not persist input.toml: {e}");
            }
        }
    }

    /// Drain any pending gilrs events. Returns the list of
    /// hot-plug notices the host should toast / log. State-change
    /// events (button presses etc.) are absorbed silently - the
    /// poll path reads device state directly via
    /// [`gilrs::Gilrs::gamepad`].
    pub fn drain_events(&mut self) -> Vec<HotplugNotice> {
        let mut notices = Vec::new();
        let mut needs_persist = false;
        // Inner block borrows `self.gilrs` mutably; we defer persist
        // I/O until the borrow drops.
        if let Some(g) = self.gilrs.as_mut() {
            while let Some(ev) = g.next_event() {
                match ev.event {
                    gilrs::EventType::Connected => {
                        let pad = g.gamepad(ev.id);
                        let uuid = GamepadUuid::from_bytes(pad.uuid());
                        let name = pad.name().to_string();
                        if let Some(notice) = on_connect(
                            ev.id,
                            uuid,
                            &name,
                            &mut self.cfg,
                            &mut self.p1_pad_id,
                            &mut self.p2_pad_id,
                        ) {
                            if matches!(notice, HotplugNotice::Assigned { .. }) {
                                needs_persist = true;
                            }
                            notices.push(notice);
                        }
                    }
                    gilrs::EventType::Disconnected => {
                        // `connected_gamepad` returns None
                        // post-disconnect; `gamepad` keeps the
                        // cached entry so we can still log the
                        // friendly name.
                        let name = g.gamepad(ev.id).name().to_string();
                        let player = if Some(ev.id) == self.p1_pad_id {
                            self.p1_pad_id = None;
                            Some(1)
                        } else if Some(ev.id) == self.p2_pad_id {
                            self.p2_pad_id = None;
                            Some(2)
                        } else {
                            None
                        };
                        notices.push(HotplugNotice::Disconnected { player, name });
                    }
                    _ => {}
                }
            }
        }
        if needs_persist {
            self.persist_quietly();
        }
        notices
    }

    /// Update keyboard state for one winit key event. Call before
    /// [`InputRuntime::compute_player_bits`] so the most recent edge
    /// is reflected.
    pub fn note_key(&mut self, code: KeyCode, pressed: bool) {
        self.keys_held.insert(code, pressed);
    }

    /// Apply a previously-drained Connect/Disconnect event to the
    /// runtime's slot routing. Used by callers that want to peek at
    /// gilrs events for non-binding purposes (menu nav, Mode-button
    /// overlay toggle) before handing the event back to the runtime
    /// - keeps a single drain consumer without losing hot-plug
    /// updates. Returns the same notice [`InputRuntime::drain_events`]
    /// would have produced.
    pub fn handle_synthetic_event(
        &mut self,
        id: gilrs::GamepadId,
        ev: &gilrs::EventType,
    ) -> Option<HotplugNotice> {
        let g = self.gilrs.as_ref()?;
        match ev {
            gilrs::EventType::Connected => {
                let pad = g.gamepad(id);
                let uuid = GamepadUuid::from_bytes(pad.uuid());
                let name = pad.name().to_string();
                let notice = on_connect(
                    id,
                    uuid,
                    &name,
                    &mut self.cfg,
                    &mut self.p1_pad_id,
                    &mut self.p2_pad_id,
                );
                if matches!(notice, Some(HotplugNotice::Assigned { .. })) {
                    self.persist_quietly();
                }
                notice
            }
            gilrs::EventType::Disconnected => {
                // gilrs caches the gamepad entry past disconnect
                // so we can still recover the friendly name.
                let name = g.gamepad(id).name().to_string();
                let player = if Some(id) == self.p1_pad_id {
                    self.p1_pad_id = None;
                    Some(1)
                } else if Some(id) == self.p2_pad_id {
                    self.p2_pad_id = None;
                    Some(2)
                } else {
                    None
                };
                Some(HotplugNotice::Disconnected { player, name })
            }
            _ => None,
        }
    }

    /// Compute the controller-shifter byte for the requested player.
    /// `player` is 1 or 2; anything else returns 0. Caller writes
    /// the result into `nes.bus.controllers[n].buttons`.
    pub fn compute_player_bits(&self, player: u8) -> u8 {
        let p = match player {
            1 => &self.cfg.p1,
            2 => &self.cfg.p2,
            _ => return 0,
        };
        let pad_id = match player {
            1 => self.p1_pad_id,
            2 => self.p2_pad_id,
            _ => None,
        };
        let pad = pad_id.and_then(|id| self.gilrs.as_ref()?.connected_gamepad(id));
        let mut bits = 0u8;
        for (nes_button, sources) in &p.bindings {
            // Turbo bits oscillate when held; non-turbo bits set on
            // any pressed source.
            let any_held = sources
                .iter()
                .any(|src| source_is_active(src, &self.keys_held, pad.as_ref()));
            if !any_held {
                continue;
            }
            if nes_button.is_turbo() {
                if turbo_phase(self.frame_counter, self.cfg.turbo_rate_hz) {
                    bits |= nes_button.shifter_bit();
                }
            } else {
                bits |= nes_button.shifter_bit();
            }
        }
        bits
    }

    /// Advance the per-frame turbo oscillator. Call once per
    /// rendered frame, after computing controller bits.
    pub fn end_frame(&mut self) {
        self.frame_counter = self.frame_counter.wrapping_add(1);
    }

    /// Read-only access for the (future) settings UI to enumerate
    /// connected pads + their names.
    pub fn gilrs(&self) -> Option<&gilrs::Gilrs> {
        self.gilrs.as_ref()
    }

    /// Mutable access for the (future) settings UI's rebinding
    /// flow, which needs `Gilrs::next_event` directly to capture a
    /// "press a button" prompt.
    pub fn gilrs_mut(&mut self) -> Option<&mut gilrs::Gilrs> {
        self.gilrs.as_mut()
    }

    pub fn config(&self) -> &InputConfig {
        &self.cfg
    }

    /// Returns the resolved gilrs id for the given player (1 or 2).
    /// `None` means the slot is unassigned or the assigned pad is
    /// currently disconnected.
    pub fn player_pad_id(&self, player: u8) -> Option<gilrs::GamepadId> {
        match player {
            1 => self.p1_pad_id,
            2 => self.p2_pad_id,
            _ => None,
        }
    }
}

/// True for devices we *never* auto-assign even when both player
/// slots are empty. Linux's evdev backend exposes some keyboards
/// (notably Keychron docks and many "gaming" keyboards) as
/// generic HID gamepads alongside their normal keyboard interface.
/// gilrs has no way to tell these apart from a real controller, so
/// without this filter the first-run auto-assign rule grabs the
/// keyboard's HID gamepad and the user's actual controller can't
/// land in P1. Sticky-UUID restore (rule 1) still honors any
/// device the user has *explicitly* assigned via the settings UI -
/// the filter only gates the auto-assign path.
fn name_blocks_auto_assign(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n.contains("keyboard") || n.contains("keychron")
}

/// Pure function over connect-event state. Generic over the
/// device-id type so unit tests can drive the rules without
/// having to mint a real `gilrs::GamepadId` (gilrs doesn't expose
/// a constructor for those - they only flow out of the Gilrs
/// runtime).
fn on_connect<Id: Copy>(
    id: Id,
    uuid: GamepadUuid,
    name: &str,
    cfg: &mut InputConfig,
    p1_pad_id: &mut Option<Id>,
    p2_pad_id: &mut Option<Id>,
) -> Option<HotplugNotice> {
    // Rule 1: sticky UUID match restores silently, but only when
    // the slot is currently *unassigned* (no live pad id). The
    // empty-slot guard handles the duplicate-UUID case: two
    // controllers of the same model (e.g. two 8BitDo Ultimate 2Cs)
    // ship identical HID UUIDs, so without the guard the second
    // pad would silently overwrite the first slot's live id and
    // never reach the rule 3 auto-assign-to-P2 path. The
    // auto-assign filter (`name_blocks_auto_assign`) does NOT
    // apply here - if the user explicitly bound a Keychron-as-
    // gamepad device, we honor it.
    if Some(uuid) == cfg.p1.gamepad_uuid && p1_pad_id.is_none() {
        *p1_pad_id = Some(id);
        return None;
    }
    if Some(uuid) == cfg.p2.gamepad_uuid && p2_pad_id.is_none() {
        *p2_pad_id = Some(id);
        return None;
    }
    // Filter for the auto-assign rules (2 + 3): skip devices that
    // are almost certainly not real gamepads.
    if name_blocks_auto_assign(name) {
        return Some(HotplugNotice::Ignored { name: name.to_string() });
    }
    // Rule 2: P1 unassigned (and currently empty) -> assign here.
    if cfg.p1.gamepad_uuid.is_none() && p1_pad_id.is_none() {
        cfg.p1.gamepad_uuid = Some(uuid);
        *p1_pad_id = Some(id);
        return Some(HotplugNotice::Assigned { player: 1, name: name.to_string() });
    }
    // Rule 3: P1 has a pad AND P2 unassigned -> assign here.
    let p1_has_pad = p1_pad_id.is_some() || cfg.p1.gamepad_uuid.is_some();
    if p1_has_pad
        && cfg.p2.gamepad_uuid.is_none()
        && p2_pad_id.is_none()
    {
        cfg.p2.gamepad_uuid = Some(uuid);
        *p2_pad_id = Some(id);
        return Some(HotplugNotice::Assigned { player: 2, name: name.to_string() });
    }
    Some(HotplugNotice::Ignored { name: name.to_string() })
}

fn source_is_active(
    src: &PhysicalSource,
    keys_held: &HashMap<KeyCode, bool>,
    pad: Option<&gilrs::Gamepad<'_>>,
) -> bool {
    match src {
        PhysicalSource::Key(k) => *keys_held.get(&k.to_winit()).unwrap_or(&false),
        PhysicalSource::GamepadButton(b) => {
            pad.map(|p| p.is_pressed(b.to_gilrs())).unwrap_or(false)
        }
        PhysicalSource::GamepadAxis { axis, sign } => {
            let Some(p) = pad else { return false };
            let v = p.value(axis.to_gilrs());
            match sign {
                AxisSign::Positive => v > AXIS_THRESHOLD,
                AxisSign::Negative => v < -AXIS_THRESHOLD,
            }
        }
    }
}

/// True when the turbo oscillator is in its "fire" half-period for
/// the current frame, given the configured rate.  Picks the period
/// to be `60 / rate` frames (rounded), so 30 Hz fires every other
/// frame, 20 Hz fires every third, etc. Falls back to "always
/// pressed" for rates ≥ 60 Hz to avoid divide-by-zero / no-op.
fn turbo_phase(frame_counter: u32, rate_hz: u8) -> bool {
    if rate_hz >= 60 || rate_hz == 0 {
        return true;
    }
    let period = (60 / rate_hz as u32).max(1);
    (frame_counter / period) & 1 == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tests run `on_connect` against `usize` ids since gilrs
    /// doesn't expose a way to mint `GamepadId` values outside
    /// its own runtime. The function is generic over the id type
    /// (only equality is used), so any `Copy + Eq` works.
    fn fake_id(n: usize) -> usize {
        n
    }

    fn uuid(seed: u8) -> GamepadUuid {
        let mut b = [0u8; 16];
        for (i, slot) in b.iter_mut().enumerate() {
            *slot = seed.wrapping_add(i as u8);
        }
        GamepadUuid::from_bytes(b)
    }

    #[test]
    fn nes_button_round_trip_via_string() {
        for &b in NesButton::all() {
            let s = b.name();
            assert_eq!(NesButton::from_str(s).unwrap(), b);
        }
    }

    #[test]
    fn key_table_round_trips() {
        for (name, code) in KEY_TABLE {
            let kn = KeyName::from_name(name).unwrap();
            assert_eq!(kn.to_winit(), *code);
            assert_eq!(kn.name(), *name);
        }
    }

    #[test]
    fn gamepad_button_round_trips_through_gilrs() {
        let buttons = [
            GamepadButton::South,
            GamepadButton::East,
            GamepadButton::North,
            GamepadButton::West,
            GamepadButton::LeftTrigger,
            GamepadButton::RightTrigger,
            GamepadButton::LeftTrigger2,
            GamepadButton::RightTrigger2,
            GamepadButton::Select,
            GamepadButton::Start,
            GamepadButton::LeftThumb,
            GamepadButton::RightThumb,
            GamepadButton::DPadUp,
            GamepadButton::DPadDown,
            GamepadButton::DPadLeft,
            GamepadButton::DPadRight,
        ];
        for b in buttons {
            let gb = b.to_gilrs();
            let back = GamepadButton::from_gilrs(gb).expect("known button");
            assert_eq!(back, b);
            assert_eq!(GamepadButton::from_name(b.name()).unwrap(), b);
        }
    }

    #[test]
    fn physical_source_string_round_trip() {
        let cases = [
            PhysicalSource::Key(KeyName(KeyCode::KeyZ)),
            PhysicalSource::GamepadButton(GamepadButton::West),
            PhysicalSource::GamepadAxis {
                axis: GamepadAxis::LeftStickY,
                sign: AxisSign::Negative,
            },
            PhysicalSource::GamepadAxis {
                axis: GamepadAxis::LeftStickX,
                sign: AxisSign::Positive,
            },
        ];
        for c in cases {
            let s = c.encode();
            let back = PhysicalSource::parse(&s).unwrap();
            assert_eq!(back, c, "round-trip via {s:?}");
        }
    }

    #[test]
    fn gamepad_uuid_hex_round_trip() {
        let u = uuid(0x42);
        let s = u.to_hex();
        assert_eq!(s.len(), 36); // 32 hex + 4 dashes
        let back = GamepadUuid::from_hex(&s).unwrap();
        assert_eq!(back, u);
    }

    #[test]
    fn default_config_binds_all_eight_directional_buttons_on_p1_keyboard_and_gamepad() {
        let c = InputConfig::default();
        for &b in &[
            NesButton::A,
            NesButton::B,
            NesButton::Select,
            NesButton::Start,
            NesButton::Up,
            NesButton::Down,
            NesButton::Left,
            NesButton::Right,
        ] {
            let sources = c.p1.bindings.get(&b).expect("button bound");
            assert!(
                sources.iter().any(|s| matches!(s, PhysicalSource::Key(_))),
                "P1 keyboard should bind {b:?}"
            );
            assert!(
                sources.iter().any(|s| matches!(
                    s,
                    PhysicalSource::GamepadButton(_) | PhysicalSource::GamepadAxis { .. }
                )),
                "P1 gamepad should bind {b:?}"
            );
        }
        // Default A maps to South (per the discussion: physical
        // bottom-face button, never PS-style "X label").
        let a = c.p1.bindings.get(&NesButton::A).unwrap();
        assert!(a.contains(&PhysicalSource::GamepadButton(GamepadButton::South)));
        // Default B maps to West.
        let b = c.p1.bindings.get(&NesButton::B).unwrap();
        assert!(b.contains(&PhysicalSource::GamepadButton(GamepadButton::West)));
    }

    #[test]
    fn default_p2_has_gamepad_only() {
        let c = InputConfig::default();
        for sources in c.p2.bindings.values() {
            for s in sources {
                assert!(
                    !matches!(s, PhysicalSource::Key(_)),
                    "P2 default binding includes a keyboard source: {:?}",
                    s
                );
            }
        }
        assert!(c.p2.gamepad_uuid.is_none());
    }

    #[test]
    fn default_turbo_buttons_are_unbound() {
        let c = InputConfig::default();
        assert!(c.p1.bindings.get(&NesButton::TurboA).unwrap().is_empty());
        assert!(c.p1.bindings.get(&NesButton::TurboB).unwrap().is_empty());
        assert!(c.p2.bindings.get(&NesButton::TurboA).unwrap().is_empty());
        assert!(c.p2.bindings.get(&NesButton::TurboB).unwrap().is_empty());
    }

    #[test]
    fn toml_round_trip_preserves_default_config() {
        let c = InputConfig::default();
        let s = c.to_toml();
        let back = InputConfig::from_toml(&s).expect("re-parse defaults");
        assert_eq!(back.turbo_rate_hz, c.turbo_rate_hz);
        assert_eq!(back.p1.bindings, c.p1.bindings);
        assert_eq!(back.p2.bindings, c.p2.bindings);
        assert_eq!(back.p1.gamepad_uuid, c.p1.gamepad_uuid);
        assert_eq!(back.p2.gamepad_uuid, c.p2.gamepad_uuid);
    }

    #[test]
    fn toml_round_trip_preserves_uuid() {
        let mut c = InputConfig::default();
        c.p1.gamepad_uuid = Some(uuid(0x10));
        c.p2.gamepad_uuid = Some(uuid(0x20));
        let s = c.to_toml();
        let back = InputConfig::from_toml(&s).unwrap();
        assert_eq!(back.p1.gamepad_uuid, Some(uuid(0x10)));
        assert_eq!(back.p2.gamepad_uuid, Some(uuid(0x20)));
    }

    #[test]
    fn turbo_phase_30hz_alternates_each_frame_pair() {
        // 60 / 30 = 2 frames per half-period
        assert_eq!(turbo_phase(0, 30), true);
        assert_eq!(turbo_phase(1, 30), true);
        assert_eq!(turbo_phase(2, 30), false);
        assert_eq!(turbo_phase(3, 30), false);
        assert_eq!(turbo_phase(4, 30), true);
    }

    #[test]
    fn turbo_phase_high_rate_always_active() {
        for rate in [60u8, 120, 200] {
            for f in 0..10 {
                assert!(turbo_phase(f, rate));
            }
        }
    }

    #[test]
    fn turbo_phase_zero_rate_treated_as_always_active() {
        for f in 0..10 {
            assert!(turbo_phase(f, 0));
        }
    }

    // ---- Hot-plug rules (pure on_connect against fake ids) ----

    #[test]
    fn rule2_p1_unassigned_first_pad_lands_on_p1() {
        let mut cfg = InputConfig::default();
        let (mut p1, mut p2) = (None, None);
        let n = on_connect(fake_id(1), uuid(0xAA), "Pad A", &mut cfg, &mut p1, &mut p2);
        assert_eq!(p1, Some(fake_id(1)));
        assert_eq!(p2, None);
        assert_eq!(cfg.p1.gamepad_uuid, Some(uuid(0xAA)));
        assert!(matches!(n, Some(HotplugNotice::Assigned { player: 1, .. })));
    }

    #[test]
    fn rule3_p1_assigned_second_pad_lands_on_p2() {
        let mut cfg = InputConfig::default();
        let (mut p1, mut p2) = (None, None);
        on_connect(fake_id(1), uuid(0xAA), "Pad A", &mut cfg, &mut p1, &mut p2);
        let n = on_connect(fake_id(2), uuid(0xBB), "Pad B", &mut cfg, &mut p1, &mut p2);
        assert_eq!(p1, Some(fake_id(1)));
        assert_eq!(p2, Some(fake_id(2)));
        assert_eq!(cfg.p1.gamepad_uuid, Some(uuid(0xAA)));
        assert_eq!(cfg.p2.gamepad_uuid, Some(uuid(0xBB)));
        assert!(matches!(n, Some(HotplugNotice::Assigned { player: 2, .. })));
    }

    #[test]
    fn rule4_third_pad_is_ignored_when_both_slots_filled() {
        let mut cfg = InputConfig::default();
        let (mut p1, mut p2) = (None, None);
        on_connect(fake_id(1), uuid(0xAA), "Pad A", &mut cfg, &mut p1, &mut p2);
        on_connect(fake_id(2), uuid(0xBB), "Pad B", &mut cfg, &mut p1, &mut p2);
        let n = on_connect(fake_id(3), uuid(0xCC), "Pad C", &mut cfg, &mut p1, &mut p2);
        assert!(matches!(n, Some(HotplugNotice::Ignored { .. })));
        assert_eq!(p1, Some(fake_id(1)));
        assert_eq!(p2, Some(fake_id(2)));
    }

    #[test]
    fn rule1_sticky_uuid_match_restores_silently() {
        let mut cfg = InputConfig::default();
        cfg.p1.gamepad_uuid = Some(uuid(0xAA));
        let (mut p1, mut p2) = (None, None);
        let n = on_connect(fake_id(7), uuid(0xAA), "Pad A", &mut cfg, &mut p1, &mut p2);
        assert!(n.is_none(), "sticky restore should be silent");
        assert_eq!(p1, Some(fake_id(7)));
    }

    #[test]
    fn auto_assign_skips_keychron_keyboard_dock() {
        // Linux exposes some Keychron keyboards as a generic HID
        // gamepad. Auto-assign must skip them so the user's actual
        // controller can land in P1.
        let mut cfg = InputConfig::default();
        let (mut p1, mut p2) = (None, None);
        let n = on_connect(
            fake_id(1),
            uuid(0xAA),
            "Keychron Keychron Link",
            &mut cfg,
            &mut p1,
            &mut p2,
        );
        assert_eq!(p1, None);
        assert!(cfg.p1.gamepad_uuid.is_none());
        assert!(matches!(n, Some(HotplugNotice::Ignored { .. })));
        // A real controller plugged in afterward should still
        // grab P1.
        let n = on_connect(
            fake_id(2),
            uuid(0xBB),
            "Microsoft Xbox Controller",
            &mut cfg,
            &mut p1,
            &mut p2,
        );
        assert_eq!(p1, Some(fake_id(2)));
        assert!(matches!(n, Some(HotplugNotice::Assigned { player: 1, .. })));
    }

    #[test]
    fn two_controllers_with_identical_uuids_split_across_p1_and_p2() {
        // Two same-model 8BitDos ship the same HID UUID. The
        // second controller must not silently overwrite P1; it
        // should fall through to rule 3 and land on P2.
        let mut cfg = InputConfig::default();
        let (mut p1, mut p2) = (None, None);
        let same_uuid = uuid(0xAA);

        let n = on_connect(
            fake_id(1),
            same_uuid,
            "8BitDo Ultimate 2C Wireless Controller",
            &mut cfg,
            &mut p1,
            &mut p2,
        );
        assert_eq!(p1, Some(fake_id(1)));
        assert!(matches!(n, Some(HotplugNotice::Assigned { player: 1, .. })));

        let n = on_connect(
            fake_id(2),
            same_uuid,
            "8BitDo Ultimate 2C Wireless Controller",
            &mut cfg,
            &mut p1,
            &mut p2,
        );
        assert_eq!(p1, Some(fake_id(1)), "P1 must not be overwritten");
        assert_eq!(p2, Some(fake_id(2)), "P2 takes the duplicate-UUID pad");
        assert!(matches!(n, Some(HotplugNotice::Assigned { player: 2, .. })));
    }

    #[test]
    fn auto_assign_filter_does_not_block_sticky_restore() {
        // If a user has explicitly bound a Keychron-as-gamepad
        // device (against our advice), reconnecting it must
        // restore the slot - the filter only gates auto-assign.
        let mut cfg = InputConfig::default();
        cfg.p1.gamepad_uuid = Some(uuid(0xCC));
        let (mut p1, mut p2) = (None, None);
        let n = on_connect(
            fake_id(5),
            uuid(0xCC),
            "Keychron Keychron Link",
            &mut cfg,
            &mut p1,
            &mut p2,
        );
        assert!(n.is_none()); // sticky restore is silent
        assert_eq!(p1, Some(fake_id(5)));
    }

    #[test]
    fn p2_does_not_auto_fill_when_p1_is_unassigned() {
        // Edge case: p1 has no UUID and no live id, but a NEW pad
        // shows up. Rule 2 fires (P1 takes it); rule 3 should not.
        let mut cfg = InputConfig::default();
        let (mut p1, mut p2) = (None, None);
        on_connect(fake_id(1), uuid(0xAA), "A", &mut cfg, &mut p1, &mut p2);
        // Now p1 is taken; a second pad lands in p2.
        on_connect(fake_id(2), uuid(0xBB), "B", &mut cfg, &mut p1, &mut p2);
        assert_eq!(p1, Some(fake_id(1)));
        assert_eq!(p2, Some(fake_id(2)));
    }
}
