//! The text form of a [`ButtonAssignment`].
//!
//! One grammar, owned here, for everything that names an assignment as text:
//!
//! | Text | Meaning |
//! |---|---|
//! | `none` | nothing assigned |
//! | `mouse:1` … `mouse:5` | a standard mouse button |
//! | `fn:dpi-up`, `fn:dpi-down`, `fn:dpi-cycle`, `fn:profile-cycle`, `fn:dpi-shift`, `fn:g-shift` | a built-in function |
//! | `key:ctrl+shift+b` | a keyboard key, with modifiers held |
//! | `key:ctrl` | modifiers only |
//! | `key:0x05` | a key by its raw USB HID usage |
//! | `raw:16,00,00` | the entry's three bytes, for codes with no name yet |
//!
//! [`Display`](std::fmt::Display) prints the same grammar, and parsing what it
//! prints gives back the value it printed.

use std::fmt;
use std::str::FromStr;

use thiserror::Error;

use super::{ButtonAssignment, Function, ModifierMask, MouseButton};

/// Modifier names and their bits, in the order they are printed.
const MODIFIERS: [(&str, u8); 8] = [
    ("ctrl", 0x01),
    ("shift", 0x02),
    ("alt", 0x04),
    ("gui", 0x08),
    ("rctrl", 0x10),
    ("rshift", 0x20),
    ("ralt", 0x40),
    ("rgui", 0x80),
];

/// Extra spellings accepted for a modifier; never printed.
const MODIFIER_ALIASES: [(&str, u8); 5] = [
    ("control", 0x01),
    ("win", 0x08),
    ("super", 0x08),
    ("cmd", 0x08),
    ("option", 0x04),
];

/// Built-in function names.
const FUNCTIONS: [(&str, Function); 6] = [
    ("dpi-up", Function::DpiUp),
    ("dpi-down", Function::DpiDown),
    ("dpi-cycle", Function::DpiCycle),
    ("profile-cycle", Function::ProfileCycle),
    ("dpi-shift", Function::DpiShift),
    ("g-shift", Function::GShift),
];

/// Key names and their USB HID keyboard usages (page `0x07`).
const KEYS: [(&str, u8); 102] = [
    ("a", 0x04),
    ("b", 0x05),
    ("c", 0x06),
    ("d", 0x07),
    ("e", 0x08),
    ("f", 0x09),
    ("g", 0x0a),
    ("h", 0x0b),
    ("i", 0x0c),
    ("j", 0x0d),
    ("k", 0x0e),
    ("l", 0x0f),
    ("m", 0x10),
    ("n", 0x11),
    ("o", 0x12),
    ("p", 0x13),
    ("q", 0x14),
    ("r", 0x15),
    ("s", 0x16),
    ("t", 0x17),
    ("u", 0x18),
    ("v", 0x19),
    ("w", 0x1a),
    ("x", 0x1b),
    ("y", 0x1c),
    ("z", 0x1d),
    ("1", 0x1e),
    ("2", 0x1f),
    ("3", 0x20),
    ("4", 0x21),
    ("5", 0x22),
    ("6", 0x23),
    ("7", 0x24),
    ("8", 0x25),
    ("9", 0x26),
    ("0", 0x27),
    ("enter", 0x28),
    ("esc", 0x29),
    ("backspace", 0x2a),
    ("tab", 0x2b),
    ("space", 0x2c),
    ("minus", 0x2d),
    ("equals", 0x2e),
    ("lbracket", 0x2f),
    ("rbracket", 0x30),
    ("backslash", 0x31),
    ("semicolon", 0x33),
    ("quote", 0x34),
    ("grave", 0x35),
    ("comma", 0x36),
    ("period", 0x37),
    ("slash", 0x38),
    ("capslock", 0x39),
    ("f1", 0x3a),
    ("f2", 0x3b),
    ("f3", 0x3c),
    ("f4", 0x3d),
    ("f5", 0x3e),
    ("f6", 0x3f),
    ("f7", 0x40),
    ("f8", 0x41),
    ("f9", 0x42),
    ("f10", 0x43),
    ("f11", 0x44),
    ("f12", 0x45),
    ("printscreen", 0x46),
    ("scrolllock", 0x47),
    ("pause", 0x48),
    ("insert", 0x49),
    ("home", 0x4a),
    ("pageup", 0x4b),
    ("delete", 0x4c),
    ("end", 0x4d),
    ("pagedown", 0x4e),
    ("right", 0x4f),
    ("left", 0x50),
    ("down", 0x51),
    ("up", 0x52),
    ("numlock", 0x53),
    ("kp-divide", 0x54),
    ("kp-multiply", 0x55),
    ("kp-minus", 0x56),
    ("kp-plus", 0x57),
    ("kp-enter", 0x58),
    ("kp-1", 0x59),
    ("kp-2", 0x5a),
    ("kp-3", 0x5b),
    ("kp-4", 0x5c),
    ("kp-5", 0x5d),
    ("kp-6", 0x5e),
    ("kp-7", 0x5f),
    ("kp-8", 0x60),
    ("kp-9", 0x61),
    ("kp-0", 0x62),
    ("kp-period", 0x63),
    ("application", 0x65),
    ("f13", 0x68),
    ("f14", 0x69),
    ("f15", 0x6a),
    ("f16", 0x6b),
    ("f17", 0x6c),
    ("f18", 0x6d),
];

/// Why text is not a button assignment.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ParseAssignmentError {
    /// The text is not `none` and has no `kind:` prefix.
    #[error("`{0}` is not an assignment; use none, mouse:N, fn:NAME, key:KEYS or raw:CC,MM,UU")]
    UnknownForm(String),
    /// The mouse button is not 1 through 5.
    #[error("`{0}` is not a mouse button; the mouse has buttons 1 to 5")]
    MouseButton(String),
    /// The function name is not one of the built-in functions.
    #[error(
        "`{0}` is not a function; use dpi-up, dpi-down, dpi-cycle, profile-cycle, dpi-shift or g-shift"
    )]
    Function(String),
    /// A part of a key chord is neither a modifier nor a key.
    #[error("`{0}` is not a modifier or key name")]
    Key(String),
    /// A key chord names more than one key, or none at all.
    #[error("a key assignment needs one key, optionally with modifiers: `{0}`")]
    Chord(String),
    /// A `raw:` entry is not three hexadecimal bytes.
    #[error("`{0}` is not three hexadecimal bytes such as 16,00,00")]
    Raw(String),
}

fn key_usage(name: &str) -> Option<u8> {
    KEYS.iter()
        .find(|(candidate, _)| *candidate == name)
        .map(|&(_, usage)| usage)
}

fn key_name(usage: u8) -> Option<&'static str> {
    KEYS.iter()
        .find(|&&(_, candidate)| candidate == usage)
        .map(|&(name, _)| name)
}

fn modifier_bit(name: &str) -> Option<u8> {
    MODIFIERS
        .iter()
        .chain(MODIFIER_ALIASES.iter())
        .find(|(candidate, _)| *candidate == name)
        .map(|&(_, bit)| bit)
}

fn hex_byte(text: &str) -> Option<u8> {
    let digits = text.strip_prefix("0x").unwrap_or(text);
    if digits.is_empty() || digits.len() > 2 {
        return None;
    }
    u8::from_str_radix(digits, 16).ok()
}

fn parse_chord(chord: &str) -> Result<ButtonAssignment, ParseAssignmentError> {
    let mut modifiers = 0_u8;
    let mut usage = None;
    for part in chord.split('+') {
        if let Some(bit) = modifier_bit(part) {
            modifiers |= bit;
        } else if let Some(found) =
            key_usage(part).or_else(|| part.starts_with("0x").then(|| hex_byte(part)).flatten())
        {
            if usage.replace(found).is_some() {
                return Err(ParseAssignmentError::Chord(chord.to_owned()));
            }
        } else {
            return Err(ParseAssignmentError::Key(part.to_owned()));
        }
    }
    // A chord of nothing, or of the key usage zero with no modifier, would be
    // stored as the all-zero entry, which means "unassigned".
    if modifiers == 0 && usage.is_none_or(|usage| usage == 0) {
        return Err(ParseAssignmentError::Chord(chord.to_owned()));
    }
    Ok(ButtonAssignment::Keyboard {
        modifiers: ModifierMask::new(modifiers),
        usage: usage.unwrap_or(0),
    })
}

impl FromStr for ButtonAssignment {
    type Err = ParseAssignmentError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let text = text.trim().to_ascii_lowercase();
        if text == "none" {
            return Ok(Self::Unassigned);
        }
        let (kind, rest) = text
            .split_once(':')
            .ok_or_else(|| ParseAssignmentError::UnknownForm(text.clone()))?;
        match kind {
            "mouse" => rest
                .parse::<u8>()
                .ok()
                .and_then(|number| match number {
                    1 => Some(MouseButton::Button1),
                    2 => Some(MouseButton::Button2),
                    3 => Some(MouseButton::Button3),
                    4 => Some(MouseButton::Button4),
                    5 => Some(MouseButton::Button5),
                    _ => None,
                })
                .map(Self::Mouse)
                .ok_or_else(|| ParseAssignmentError::MouseButton(rest.to_owned())),
            "fn" => FUNCTIONS
                .iter()
                .find(|(name, _)| *name == rest)
                .map(|&(_, function)| Self::Function(function))
                .ok_or_else(|| ParseAssignmentError::Function(rest.to_owned())),
            "key" => parse_chord(rest),
            "raw" => {
                let bytes: Vec<u8> = rest.split(',').filter_map(hex_byte).collect();
                let entry: [u8; 3] = bytes
                    .as_slice()
                    .try_into()
                    .ok()
                    .filter(|_| rest.split(',').count() == 3)
                    .ok_or_else(|| ParseAssignmentError::Raw(rest.to_owned()))?;
                Ok(Self::decode(entry))
            }
            _ => Err(ParseAssignmentError::UnknownForm(text.clone())),
        }
    }
}

impl fmt::Display for ButtonAssignment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Unassigned => f.write_str("none"),
            Self::Mouse(button) => write!(f, "mouse:{}", button.number()),
            Self::Function(function) => {
                let name = FUNCTIONS
                    .iter()
                    .find(|&&(_, candidate)| candidate == function)
                    .map_or("?", |&(name, _)| name);
                write!(f, "fn:{name}")
            }
            Self::Keyboard { modifiers, usage } => {
                f.write_str("key:")?;
                let mut first = true;
                for (name, bit) in MODIFIERS {
                    if modifiers.bits() & bit != 0 {
                        if !first {
                            f.write_str("+")?;
                        }
                        f.write_str(name)?;
                        first = false;
                    }
                }
                if usage != 0 {
                    if !first {
                        f.write_str("+")?;
                    }
                    match key_name(usage) {
                        Some(name) => f.write_str(name)?,
                        None => write!(f, "{usage:#04x}")?,
                    }
                }
                Ok(())
            }
            Self::Unrecognized {
                code,
                modifiers,
                usage,
            } => write!(f, "raw:{code:02x},{modifiers:02x},{usage:02x}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> ButtonAssignment {
        text.parse().expect("valid assignment text")
    }

    #[test]
    fn each_kind_parses() {
        assert_eq!(parse("none"), ButtonAssignment::Unassigned);
        assert_eq!(
            parse("mouse:4"),
            ButtonAssignment::Mouse(MouseButton::Button4)
        );
        assert_eq!(
            parse("fn:dpi-cycle"),
            ButtonAssignment::Function(Function::DpiCycle)
        );
        assert_eq!(
            parse("key:ctrl+shift+b"),
            ButtonAssignment::Keyboard {
                modifiers: ModifierMask::new(0x03),
                usage: 0x05
            }
        );
        assert_eq!(
            parse("KEY:Ctrl"),
            ButtonAssignment::Keyboard {
                modifiers: ModifierMask::new(0x01),
                usage: 0
            }
        );
        assert_eq!(
            parse("key:0x68"),
            ButtonAssignment::Keyboard {
                modifiers: ModifierMask::new(0),
                usage: 0x68
            }
        );
    }

    #[test]
    fn raw_entries_decode_to_their_named_form() {
        assert_eq!(
            parse("raw:11,00,00"),
            ButtonAssignment::Function(Function::DpiUp)
        );
        assert_eq!(
            parse("raw:16,00,00"),
            ButtonAssignment::Unrecognized {
                code: 0x16,
                modifiers: 0,
                usage: 0
            }
        );
    }

    #[test]
    fn bad_text_is_refused_with_the_part_that_is_wrong() {
        for (text, expected) in [
            ("b", ParseAssignmentError::UnknownForm("b".to_owned())),
            ("mouse:6", ParseAssignmentError::MouseButton("6".to_owned())),
            ("mouse:x", ParseAssignmentError::MouseButton("x".to_owned())),
            (
                "fn:turbo",
                ParseAssignmentError::Function("turbo".to_owned()),
            ),
            (
                "key:ctrl+nope",
                ParseAssignmentError::Key("nope".to_owned()),
            ),
            ("key:a+b", ParseAssignmentError::Chord("a+b".to_owned())),
            ("key:", ParseAssignmentError::Key(String::new())),
            ("key:0x00", ParseAssignmentError::Chord("0x00".to_owned())),
            ("raw:16,00", ParseAssignmentError::Raw("16,00".to_owned())),
            (
                "raw:16,00,00,00",
                ParseAssignmentError::Raw("16,00,00,00".to_owned()),
            ),
            (
                "raw:zz,00,00",
                ParseAssignmentError::Raw("zz,00,00".to_owned()),
            ),
            (
                "pad:1",
                ParseAssignmentError::UnknownForm("pad:1".to_owned()),
            ),
        ] {
            assert_eq!(text.parse::<ButtonAssignment>(), Err(expected), "{text}");
        }
    }

    #[test]
    fn display_round_trips_every_entry_the_mouse_can_hold() {
        for code in 0..=255_u8 {
            for modifiers in [0x00, 0x01, 0x82] {
                for usage in [0x00, 0x05, 0x68, 0xff] {
                    let assignment = ButtonAssignment::decode([code, modifiers, usage]);
                    let text = assignment.to_string();
                    assert_eq!(
                        text.parse::<ButtonAssignment>(),
                        Ok(assignment),
                        "{code:02x} {modifiers:02x} {usage:02x} printed as {text}"
                    );
                }
            }
        }
    }

    #[test]
    fn display_uses_names_where_they_exist() {
        assert_eq!(parse("key:alt+f4").to_string(), "key:alt+f4");
        assert_eq!(parse("key:rshift+kp-5").to_string(), "key:rshift+kp-5");
        assert_eq!(parse("key:0x66").to_string(), "key:0x66");
    }

    #[test]
    fn names_are_unique() {
        for (i, (name, usage)) in KEYS.iter().enumerate() {
            assert!(
                KEYS.iter()
                    .skip(i + 1)
                    .all(|(other, other_usage)| other != name && other_usage != usage),
                "{name} repeats a name or a usage"
            );
        }
    }
}
