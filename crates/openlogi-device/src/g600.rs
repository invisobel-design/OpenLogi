//! Logitech G600 onboard-profile reports (read side).
//!
//! The G600 predates HID++ 2.0 on gaming mice. Its configuration lives in
//! vendor feature reports on the `0xff80/0x0080` collection (see
//! [`openlogi_device_registry::g600`]): three 154-byte onboard profiles, one
//! per report ID `0xf3`–`0xf5`.
//!
//! [`Profile`] keeps the report's bytes verbatim and decodes fields on demand,
//! so a later read-modify-write can only change the bytes it means to — the
//! regions this module does not understand (and button codes it does not
//! recognise) survive untouched.
//!
//! The layout below was checked against three reports read from a real G600:
//! the field offsets add up to exactly [`PROFILE_REPORT_LEN`], the DPI steps
//! decode to sensible values, and the button tables land on the factory
//! defaults. Names given to button function codes follow a third-party
//! write-up; they have not been confirmed by pressing each button.

use openlogi_device_registry::g600::matches_g600;
use thiserror::Error;

use crate::backend::{BackendError, FeatureReader, HidBackend};
use crate::channel::route::{DeviceRoute, find_raw_route_node};
use crate::write::WriteError;

/// Feature report ID that carries the active profile and DPI slot.
///
/// Its two data bytes are not decoded yet.
pub const ACTIVE_PROFILE_REPORT_ID: u8 = 0xf0;

/// Length in bytes of one onboard-profile feature report, report ID included.
pub const PROFILE_REPORT_LEN: usize = 154;

/// How many numbered buttons (G1–G20) each button table describes.
pub const BUTTON_COUNT: u8 = 20;

/// Bytes per button entry: a code, a modifier mask, and a key usage.
const BUTTON_ENTRY_LEN: usize = 3;
/// DPI values are stored in units of this many DPI.
const DPI_STEP: u16 = 50;

// Offsets within a profile report; offset 0 is the report ID.
const LED_COLOR: usize = 1;
const REPORT_RATE: usize = 11;
const DPI_SHIFT: usize = 12;
const DEFAULT_DPI_SLOT: usize = 13;
const DPI_SLOTS: usize = 14;
const DPI_SLOT_COUNT: usize = 4;
const BUTTONS: usize = 31;
const SHIFT_LED_COLOR: usize = 91;
const SHIFT_BUTTONS: usize = 94;

const _: () = assert!(
    SHIFT_BUTTONS + BUTTON_ENTRY_LEN * BUTTON_COUNT as usize == PROFILE_REPORT_LEN,
    "the G-shift table must end exactly at the end of the report"
);
const _: () = assert!(
    BUTTONS + BUTTON_ENTRY_LEN * BUTTON_COUNT as usize == SHIFT_LED_COLOR,
    "the button table must run straight into the G-shift LED color"
);

/// One of the mouse's three onboard profiles.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ProfileSlot {
    /// Report `0xf3`.
    First,
    /// Report `0xf4`.
    Second,
    /// Report `0xf5`.
    Third,
}

impl ProfileSlot {
    /// Every profile slot, in report-ID order.
    pub const ALL: [Self; 3] = [Self::First, Self::Second, Self::Third];

    /// The feature report ID this profile is read from and written to.
    #[must_use]
    pub const fn report_id(self) -> u8 {
        match self {
            Self::First => 0xf3,
            Self::Second => 0xf4,
            Self::Third => 0xf5,
        }
    }

    /// The profile a feature report ID names, if it names one.
    #[must_use]
    pub const fn from_report_id(id: u8) -> Option<Self> {
        match id {
            0xf3 => Some(Self::First),
            0xf4 => Some(Self::Second),
            0xf5 => Some(Self::Third),
            _ => None,
        }
    }
}

/// One of the G600's numbered buttons, G1 through G20.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct GButton(u8);

impl GButton {
    /// The button with this number, or `None` outside `1..=20`.
    #[must_use]
    pub const fn new(number: u8) -> Option<Self> {
        if number >= 1 && number <= BUTTON_COUNT {
            Some(Self(number))
        } else {
            None
        }
    }

    /// The button's number as printed in the profile (1-based).
    #[must_use]
    pub const fn number(self) -> u8 {
        self.0
    }

    /// Every button, G1 first.
    pub fn all() -> impl Iterator<Item = Self> {
        (1..=BUTTON_COUNT).map(Self)
    }

    /// Zero-based position in a button table.
    fn index(self) -> usize {
        usize::from(self.0 - 1)
    }
}

/// Which button table of a profile: the normal one, or the one used while the
/// G-shift button is held.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Layer {
    /// The table in effect normally.
    Normal,
    /// The table in effect while G-shift is held.
    Shifted,
}

/// A USB HID keyboard modifier bitmask, as stored in a button entry.
///
/// Bit 0 is left Ctrl, bit 1 left Shift, bit 2 left Alt, bit 3 left GUI, and
/// bits 4–7 are the right-hand counterparts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ModifierMask(u8);

impl ModifierMask {
    /// Wrap a raw modifier bitmask.
    #[must_use]
    pub const fn new(bits: u8) -> Self {
        Self(bits)
    }

    /// The raw bitmask.
    #[must_use]
    pub const fn bits(self) -> u8 {
        self.0
    }
}

/// One of the five standard mouse buttons a G-button can be set to emit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MouseButton {
    /// USB HID button 1 (code `0x01`).
    Button1,
    /// USB HID button 2 (code `0x02`).
    Button2,
    /// USB HID button 3 (code `0x03`).
    Button3,
    /// USB HID button 4 (code `0x04`).
    Button4,
    /// USB HID button 5 (code `0x05`).
    Button5,
}

impl MouseButton {
    const fn from_code(code: u8) -> Option<Self> {
        match code {
            0x01 => Some(Self::Button1),
            0x02 => Some(Self::Button2),
            0x03 => Some(Self::Button3),
            0x04 => Some(Self::Button4),
            0x05 => Some(Self::Button5),
            _ => None,
        }
    }
}

/// A built-in mouse function a G-button can trigger.
///
/// The names follow a third-party write-up of the profile format and have not
/// been confirmed against the mouse's behaviour.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Function {
    /// Code `0x11`.
    DpiUp,
    /// Code `0x12`.
    DpiDown,
    /// Code `0x13`.
    DpiCycle,
    /// Code `0x14`.
    ProfileCycle,
    /// Code `0x15`: temporarily switch to the profile's DPI-shift value.
    DpiShift,
    /// Code `0x17`: select the G-shift button table while held.
    GShift,
}

impl Function {
    const fn from_code(code: u8) -> Option<Self> {
        match code {
            0x11 => Some(Self::DpiUp),
            0x12 => Some(Self::DpiDown),
            0x13 => Some(Self::DpiCycle),
            0x14 => Some(Self::ProfileCycle),
            0x15 => Some(Self::DpiShift),
            0x17 => Some(Self::GShift),
            _ => None,
        }
    }
}

/// What one G-button does, decoded from its three-byte table entry.
///
/// [`ButtonAssignment::Unrecognized`] carries the entry's bytes verbatim. It
/// exists so a profile can be read and rewritten without losing codes this
/// module has not met — it is not a fallback for malformed data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ButtonAssignment {
    /// The all-zero entry: nothing is assigned.
    Unassigned,
    /// A keyboard key, by USB HID keyboard usage, with modifiers held.
    Keyboard {
        /// Modifier keys held with the key.
        modifiers: ModifierMask,
        /// USB HID keyboard/keypad usage (page `0x07`).
        usage: u8,
    },
    /// A standard mouse button.
    Mouse(MouseButton),
    /// A built-in function.
    Function(Function),
    /// A code this module does not recognise, with its two argument bytes.
    Unrecognized {
        /// The entry's first byte.
        code: u8,
        /// The entry's second byte (the modifier mask for keyboard entries).
        modifiers: u8,
        /// The entry's third byte (the key usage for keyboard entries).
        usage: u8,
    },
}

impl ButtonAssignment {
    fn decode([code, modifiers, usage]: [u8; BUTTON_ENTRY_LEN]) -> Self {
        match (code, modifiers, usage) {
            (0x00, 0x00, 0x00) => Self::Unassigned,
            (0x00, modifiers, usage) => Self::Keyboard {
                modifiers: ModifierMask::new(modifiers),
                usage,
            },
            // Mouse-button and function entries carry no arguments; a
            // non-zero argument byte means this is not the entry it looks
            // like, so it stays verbatim rather than being misread.
            (code, 0x00, 0x00) => MouseButton::from_code(code)
                .map(Self::Mouse)
                .or_else(|| Function::from_code(code).map(Self::Function))
                .unwrap_or(Self::Unrecognized {
                    code,
                    modifiers: 0,
                    usage: 0,
                }),
            (code, modifiers, usage) => Self::Unrecognized {
                code,
                modifiers,
                usage,
            },
        }
    }
}

/// Why bytes could not be read as a G600 profile report.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ProfileError {
    /// The report is not exactly [`PROFILE_REPORT_LEN`] bytes.
    #[error(
        "profile report is {actual} bytes; a G600 profile is {}",
        PROFILE_REPORT_LEN
    )]
    WrongLength {
        /// The length that was supplied.
        actual: usize,
    },
    /// The first byte is not one of the three profile report IDs.
    #[error("report ID {0:#04x} is not a G600 profile report")]
    NotAProfile(u8),
}

/// One onboard profile, as read from the mouse.
///
/// Holds the report verbatim; every accessor decodes from it on demand.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Profile {
    slot: ProfileSlot,
    raw: [u8; PROFILE_REPORT_LEN],
}

impl Profile {
    /// Read a profile from a feature report, report ID first.
    ///
    /// # Errors
    ///
    /// [`ProfileError::WrongLength`] unless the report is exactly
    /// [`PROFILE_REPORT_LEN`] bytes, and [`ProfileError::NotAProfile`] unless
    /// its first byte is `0xf3`, `0xf4` or `0xf5`.
    pub fn parse(report: &[u8]) -> Result<Self, ProfileError> {
        let raw: [u8; PROFILE_REPORT_LEN] =
            report.try_into().map_err(|_| ProfileError::WrongLength {
                actual: report.len(),
            })?;
        let slot = ProfileSlot::from_report_id(raw[0]).ok_or(ProfileError::NotAProfile(raw[0]))?;
        Ok(Self { slot, raw })
    }

    /// Which onboard profile this is.
    #[must_use]
    pub const fn slot(&self) -> ProfileSlot {
        self.slot
    }

    /// The report exactly as it was read, report ID first.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; PROFILE_REPORT_LEN] {
        &self.raw
    }

    /// The profile's LED color as `[red, green, blue]`.
    #[must_use]
    pub fn led_color(&self) -> [u8; 3] {
        self.rgb(LED_COLOR)
    }

    /// The LED color shown while G-shift is held, as `[red, green, blue]`.
    #[must_use]
    pub fn shift_led_color(&self) -> [u8; 3] {
        self.rgb(SHIFT_LED_COLOR)
    }

    /// The raw report-rate code. Which rate each value means is not known yet.
    #[must_use]
    pub fn report_rate_code(&self) -> u8 {
        self.raw[REPORT_RATE]
    }

    /// The DPI used while a DPI-shift button is held, or `None` when unset.
    #[must_use]
    pub fn dpi_shift(&self) -> Option<u16> {
        dpi(self.raw[DPI_SHIFT])
    }

    /// The four DPI slots; `None` marks a slot that is unset.
    #[must_use]
    pub fn dpi_slots(&self) -> [Option<u16>; DPI_SLOT_COUNT] {
        std::array::from_fn(|i| dpi(self.raw[DPI_SLOTS + i]))
    }

    /// The raw default-DPI-slot value.
    ///
    /// It looks 1-based — a profile with only its first slot set stores `1` —
    /// but that is an inference from one profile shape, not a confirmed fact.
    #[must_use]
    pub fn default_dpi_slot(&self) -> u8 {
        self.raw[DEFAULT_DPI_SLOT]
    }

    /// What `button` does in `layer`.
    #[must_use]
    pub fn button(&self, layer: Layer, button: GButton) -> ButtonAssignment {
        let table = match layer {
            Layer::Normal => BUTTONS,
            Layer::Shifted => SHIFT_BUTTONS,
        };
        let start = table + BUTTON_ENTRY_LEN * button.index();
        ButtonAssignment::decode([self.raw[start], self.raw[start + 1], self.raw[start + 2]])
    }

    fn rgb(&self, start: usize) -> [u8; 3] {
        [self.raw[start], self.raw[start + 1], self.raw[start + 2]]
    }
}

/// Why a profile could not be read from the mouse.
#[derive(Debug, Error)]
pub enum ReadError {
    /// The HID backend failed to deliver the report.
    #[error(transparent)]
    Backend(#[from] BackendError),
    /// The bytes that came back are not a profile report.
    #[error("{0}")]
    Profile(#[from] ProfileError),
    /// The mouse answered with a different profile than the one asked for.
    #[error("asked for the {requested:?} profile but the mouse answered with {got:?}")]
    Mismatch {
        /// The profile that was requested.
        requested: ProfileSlot,
        /// The profile the returned report names.
        got: ProfileSlot,
    },
    /// The route does not address a G600's vendor feature-report collection.
    #[error("the route does not address a G600")]
    NotAG600,
    /// No G600 is connected at the route.
    #[error("no G600 is connected at that route")]
    DeviceNotFound,
    /// The route could not be resolved to a single device node.
    #[error(transparent)]
    Route(#[from] WriteError),
}

/// Read one onboard profile through `reader`.
///
/// # Errors
///
/// [`ReadError::Backend`] when the read itself fails, [`ReadError::Profile`]
/// when what came back is not a whole profile report, and
/// [`ReadError::Mismatch`] when it names a different profile than requested.
pub async fn read_profile(
    reader: &mut dyn FeatureReader,
    slot: ProfileSlot,
) -> Result<Profile, ReadError> {
    let report = reader
        .read_feature_report(slot.report_id(), PROFILE_REPORT_LEN)
        .await?;
    let profile = Profile::parse(&report)?;
    if profile.slot() == slot {
        Ok(profile)
    } else {
        Err(ReadError::Mismatch {
            requested: slot,
            got: profile.slot(),
        })
    }
}

/// Read all three onboard profiles through `reader`, in slot order.
///
/// # Errors
///
/// The first [`ReadError`] any of the three reads produces.
pub async fn read_profiles(reader: &mut dyn FeatureReader) -> Result<[Profile; 3], ReadError> {
    let first = read_profile(reader, ProfileSlot::First).await?;
    let second = read_profile(reader, ProfileSlot::Second).await?;
    let third = read_profile(reader, ProfileSlot::Third).await?;
    Ok([first, second, third])
}

/// Everything a read-only diagnostic learns from one G600.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    /// The active-profile report exactly as the mouse returned it, report ID
    /// first. Left undecoded: what its data bytes mean is not known yet.
    pub active_profile_report: Vec<u8>,
    /// The three onboard profiles, in slot order.
    pub profiles: [Profile; 3],
}

/// Whether `route` names a G600's vendor feature-report collection.
#[must_use]
pub fn is_g600_route(route: &DeviceRoute) -> bool {
    matches!(
        route,
        DeviceRoute::RawHid {
            vendor_id,
            product_id,
            usage_page,
            usage_id,
            ..
        } if matches_g600(*vendor_id, *product_id, *usage_page, *usage_id)
    )
}

/// Read the active-profile report and all three onboard profiles of the G600
/// that `route` addresses. Nothing is written to the mouse.
///
/// # Errors
///
/// [`ReadError::NotAG600`] when `route` is not a G600's vendor collection — a
/// feature-report read is only meaningful on the device it was written for —
/// [`ReadError::DeviceNotFound`] when no such node is connected,
/// [`ReadError::Route`] when several nodes answer to the route, and the errors
/// of [`read_profiles`] otherwise.
pub async fn read_snapshot(
    backend: &dyn HidBackend,
    route: &DeviceRoute,
) -> Result<Snapshot, ReadError> {
    if !is_g600_route(route) {
        return Err(ReadError::NotAG600);
    }
    let node = find_raw_route_node(backend, route)
        .await?
        .ok_or(ReadError::DeviceNotFound)?;
    let mut reader = backend.open_feature_reader(&node).await?;
    let active_profile_report = reader
        .read_feature_report(ACTIVE_PROFILE_REPORT_ID, PROFILE_REPORT_LEN)
        .await?;
    let profiles = read_profiles(reader.as_mut()).await?;
    Ok(Snapshot {
        active_profile_report,
        profiles,
    })
}

/// A stored DPI step count as DPI; zero means the slot is unset.
fn dpi(steps: u8) -> Option<u16> {
    (steps != 0).then(|| u16::from(steps) * DPI_STEP)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{NodeId, NodeInfo};

    // Three profile reports read from a real G600. They hold only the
    // mouse's settings — no serial number, name or host path.
    const FIRST: &str = "\
        f3 00 00 00 00 04 00 00 00 00 00 00 00 01 18 00 \
        00 00 00 00 00 00 00 00 02 00 00 00 00 00 00 01 \
        00 00 02 00 00 03 00 00 04 00 00 05 00 00 17 00 \
        00 00 02 05 14 00 00 00 00 1e 00 00 1f 00 00 20 \
        00 00 21 00 00 22 00 00 23 00 00 24 00 00 25 00 \
        00 26 00 00 27 00 00 2d 00 00 2e 00 00 00 01 00 \
        00 02 00 00 03 00 00 04 00 00 05 00 00 17 00 00 \
        00 02 05 14 00 00 00 01 1e 00 01 1f 00 01 20 00 \
        01 21 00 01 22 00 01 23 00 01 24 00 01 25 00 01 \
        26 00 01 27 00 01 2d 00 01 2e";
    const SECOND: &str = "\
        f4 ff ff ff 00 00 00 00 00 00 00 00 00 01 18 00 \
        00 00 00 00 00 00 00 00 02 00 00 00 00 00 00 01 \
        00 00 02 00 00 03 00 00 04 00 00 05 00 00 17 00 \
        00 00 02 05 14 00 00 00 00 59 00 00 5a 00 00 5b \
        00 00 5c 00 00 5d 00 00 5e 00 00 5f 00 00 60 00 \
        00 61 00 00 62 00 00 56 00 00 57 ff ff ff 01 00 \
        00 02 00 00 03 00 00 04 00 00 05 00 00 17 00 00 \
        00 02 05 14 00 00 00 01 59 00 01 5a 00 01 5b 00 \
        01 5c 00 01 5d 00 01 5e 00 01 5f 00 01 60 00 01 \
        61 00 01 62 00 01 56 00 01 57";
    const THIRD: &str = "\
        f5 00 ff 00 01 04 00 00 00 00 00 00 08 02 08 18 \
        28 40 00 00 00 00 00 00 02 00 00 00 00 00 00 01 \
        00 00 02 00 00 03 00 00 04 00 00 05 00 00 15 00 \
        00 13 00 00 14 00 00 00 00 1e 00 00 1f 00 00 20 \
        00 00 21 00 00 22 00 00 23 11 00 00 16 00 00 05 \
        00 00 12 00 00 00 00 08 04 00 00 ff ff ff 00 00 \
        00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 \
        00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 \
        00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 \
        00 00 00 00 00 00 00 00 00 00";

    fn bytes(hex: &str) -> Vec<u8> {
        hex.split_whitespace()
            .map(|byte| u8::from_str_radix(byte, 16).expect("test vector is hex"))
            .collect()
    }

    fn profile(hex: &str) -> Profile {
        Profile::parse(&bytes(hex)).expect("test vector is a profile report")
    }

    fn g(number: u8) -> GButton {
        GButton::new(number).expect("test button number is in range")
    }

    fn key(modifiers: u8, usage: u8) -> ButtonAssignment {
        ButtonAssignment::Keyboard {
            modifiers: ModifierMask::new(modifiers),
            usage,
        }
    }

    #[test]
    fn test_vectors_are_whole_reports() {
        for vector in [FIRST, SECOND, THIRD] {
            assert_eq!(bytes(vector).len(), PROFILE_REPORT_LEN);
        }
    }

    #[test]
    fn each_report_id_selects_its_slot() {
        assert_eq!(profile(FIRST).slot(), ProfileSlot::First);
        assert_eq!(profile(SECOND).slot(), ProfileSlot::Second);
        assert_eq!(profile(THIRD).slot(), ProfileSlot::Third);
        for slot in ProfileSlot::ALL {
            assert_eq!(ProfileSlot::from_report_id(slot.report_id()), Some(slot));
        }
    }

    #[test]
    fn first_profile_buttons_match_the_dump() {
        let p = profile(FIRST);
        let normal = |n| p.button(Layer::Normal, g(n));
        assert_eq!(normal(1), ButtonAssignment::Mouse(MouseButton::Button1));
        assert_eq!(normal(5), ButtonAssignment::Mouse(MouseButton::Button5));
        assert_eq!(normal(6), ButtonAssignment::Function(Function::GShift));
        assert_eq!(normal(7), key(0x02, 0x05));
        assert_eq!(
            normal(8),
            ButtonAssignment::Function(Function::ProfileCycle)
        );
        assert_eq!(normal(9), key(0x00, 0x1e));
        assert_eq!(normal(20), key(0x00, 0x2e));
    }

    #[test]
    fn shifted_layer_is_a_separate_table() {
        let p = profile(FIRST);
        assert_eq!(p.button(Layer::Normal, g(9)), key(0x00, 0x1e));
        assert_eq!(p.button(Layer::Shifted, g(9)), key(0x01, 0x1e));
        assert_eq!(p.button(Layer::Shifted, g(20)), key(0x01, 0x2e));
    }

    #[test]
    fn second_profile_uses_keypad_usages() {
        let p = profile(SECOND);
        assert_eq!(p.button(Layer::Normal, g(9)), key(0x00, 0x59));
        assert_eq!(p.button(Layer::Normal, g(19)), key(0x00, 0x56));
        assert_eq!(p.button(Layer::Normal, g(20)), key(0x00, 0x57));
    }

    #[test]
    fn third_profile_functions_and_unassigned_shift_table() {
        let p = profile(THIRD);
        let normal = |n| p.button(Layer::Normal, g(n));
        assert_eq!(normal(6), ButtonAssignment::Function(Function::DpiShift));
        assert_eq!(normal(7), ButtonAssignment::Function(Function::DpiCycle));
        assert_eq!(normal(15), ButtonAssignment::Function(Function::DpiUp));
        assert_eq!(normal(17), ButtonAssignment::Mouse(MouseButton::Button5));
        assert_eq!(normal(18), ButtonAssignment::Function(Function::DpiDown));
        assert_eq!(normal(19), key(0x00, 0x08));
        assert_eq!(normal(20), ButtonAssignment::Mouse(MouseButton::Button4));
        for button in GButton::all() {
            assert_eq!(
                p.button(Layer::Shifted, button),
                ButtonAssignment::Unassigned,
                "G{} should be unassigned while shifted",
                button.number()
            );
        }
    }

    #[test]
    fn unknown_codes_are_kept_not_guessed() {
        // G16 of the third profile uses a code no write-up documents.
        assert_eq!(
            profile(THIRD).button(Layer::Normal, g(16)),
            ButtonAssignment::Unrecognized {
                code: 0x16,
                modifiers: 0,
                usage: 0,
            }
        );
    }

    #[test]
    fn function_entry_with_stray_arguments_is_not_misread() {
        let mut report = bytes(THIRD);
        // G7 is `13 00 00` (DPI cycle); give it an argument byte.
        let start = BUTTONS + BUTTON_ENTRY_LEN * 6;
        report[start + 2] = 0x07;
        let p = Profile::parse(&report).expect("still a profile report");
        assert_eq!(
            p.button(Layer::Normal, g(7)),
            ButtonAssignment::Unrecognized {
                code: 0x13,
                modifiers: 0,
                usage: 0x07,
            }
        );
    }

    #[test]
    fn dpi_values_decode_in_fifty_dpi_steps() {
        let p = profile(THIRD);
        assert_eq!(
            p.dpi_slots(),
            [Some(400), Some(1200), Some(2000), Some(3200)]
        );
        assert_eq!(p.dpi_shift(), Some(400));
        assert_eq!(p.default_dpi_slot(), 2);
    }

    #[test]
    fn unset_dpi_slots_are_none() {
        let p = profile(FIRST);
        assert_eq!(p.dpi_slots(), [Some(1200), None, None, None]);
        assert_eq!(p.dpi_shift(), None);
        assert_eq!(p.default_dpi_slot(), 1);
    }

    #[test]
    fn led_colors_come_from_their_own_fields() {
        assert_eq!(profile(FIRST).led_color(), [0x00, 0x00, 0x00]);
        assert_eq!(profile(SECOND).led_color(), [0xff, 0xff, 0xff]);
        assert_eq!(profile(THIRD).led_color(), [0x00, 0xff, 0x00]);
        assert_eq!(profile(FIRST).shift_led_color(), [0x00, 0x00, 0x00]);
        assert_eq!(profile(SECOND).shift_led_color(), [0xff, 0xff, 0xff]);
        assert_eq!(profile(THIRD).shift_led_color(), [0xff, 0xff, 0xff]);
    }

    #[test]
    fn report_rate_code_is_exposed_raw() {
        assert_eq!(profile(FIRST).report_rate_code(), 0x00);
    }

    #[test]
    fn parsing_keeps_every_byte() {
        for vector in [FIRST, SECOND, THIRD] {
            let original = bytes(vector);
            assert_eq!(profile(vector).as_bytes().as_slice(), original.as_slice());
        }
    }

    #[test]
    fn wrong_length_reports_are_rejected() {
        let good = bytes(FIRST);
        for len in [0, 1, PROFILE_REPORT_LEN - 1, PROFILE_REPORT_LEN + 1] {
            let mut report = good.clone();
            report.resize(len, 0);
            assert_eq!(
                Profile::parse(&report),
                Err(ProfileError::WrongLength { actual: len })
            );
        }
    }

    #[test]
    fn other_report_ids_are_rejected() {
        for id in [0x00, ACTIVE_PROFILE_REPORT_ID, 0xf2, 0xf6, 0xff] {
            let mut report = bytes(FIRST);
            report[0] = id;
            assert_eq!(Profile::parse(&report), Err(ProfileError::NotAProfile(id)));
        }
    }

    #[test]
    fn button_numbers_are_one_to_twenty() {
        assert_eq!(GButton::new(0), None);
        assert_eq!(GButton::new(21), None);
        assert_eq!(GButton::new(1).map(GButton::number), Some(1));
        assert_eq!(GButton::new(20).map(GButton::number), Some(20));
        assert_eq!(GButton::all().count(), usize::from(BUTTON_COUNT));
    }

    /// Serves canned reports by ID, and records what it was asked for.
    #[derive(Default)]
    struct FakeReader {
        reports: std::collections::HashMap<u8, Vec<u8>>,
        asked: Vec<(u8, usize)>,
        fail: bool,
    }

    #[hidpp::async_trait]
    impl FeatureReader for FakeReader {
        async fn read_feature_report(
            &mut self,
            report_id: u8,
            max_len: usize,
        ) -> Result<Vec<u8>, BackendError> {
            self.asked.push((report_id, max_len));
            if self.fail {
                return Err(BackendError::Disconnected);
            }
            self.reports
                .get(&report_id)
                .cloned()
                .ok_or_else(|| BackendError::Backend(format!("no report {report_id:#04x}")))
        }
    }

    fn reader_with(vectors: &[&str]) -> FakeReader {
        let mut reader = FakeReader::default();
        for vector in vectors {
            let report = bytes(vector);
            reader.reports.insert(report[0], report);
        }
        reader
    }

    #[test]
    fn read_profiles_reads_every_slot_at_the_profile_length() {
        let mut reader = reader_with(&[FIRST, SECOND, THIRD]);
        let profiles = futures_lite::future::block_on(read_profiles(&mut reader))
            .expect("all three profiles are served");
        assert_eq!(
            profiles.map(|p| p.slot()),
            [ProfileSlot::First, ProfileSlot::Second, ProfileSlot::Third]
        );
        assert_eq!(
            reader.asked,
            vec![
                (0xf3, PROFILE_REPORT_LEN),
                (0xf4, PROFILE_REPORT_LEN),
                (0xf5, PROFILE_REPORT_LEN),
            ]
        );
    }

    #[test]
    fn read_profile_surfaces_backend_failures() {
        let mut reader = FakeReader {
            fail: true,
            ..FakeReader::default()
        };
        let error = futures_lite::future::block_on(read_profile(&mut reader, ProfileSlot::Second))
            .expect_err("the backend is failing");
        assert!(matches!(
            error,
            ReadError::Backend(BackendError::Disconnected)
        ));
    }

    #[test]
    fn read_profile_rejects_a_short_report() {
        let mut short = bytes(FIRST);
        short.truncate(PROFILE_REPORT_LEN - 1);
        let mut reader = FakeReader::default();
        reader.reports.insert(0xf3, short);
        let error = futures_lite::future::block_on(read_profile(&mut reader, ProfileSlot::First))
            .expect_err("a truncated report is not a profile");
        assert!(matches!(
            error,
            ReadError::Profile(ProfileError::WrongLength { actual }) if actual == PROFILE_REPORT_LEN - 1
        ));
    }

    #[test]
    fn read_profile_rejects_the_wrong_profile() {
        // The mouse answers a request for the first profile with the second.
        let mut reader = FakeReader::default();
        reader.reports.insert(0xf3, bytes(SECOND));
        let error = futures_lite::future::block_on(read_profile(&mut reader, ProfileSlot::First))
            .expect_err("a mismatched report must not be accepted");
        assert!(matches!(
            error,
            ReadError::Mismatch {
                requested: ProfileSlot::First,
                got: ProfileSlot::Second,
            }
        ));
    }

    /// An active-profile report as read from the same mouse.
    const ACTIVE: &str = "f0 09 00 00";

    fn g600_node() -> NodeInfo {
        NodeInfo {
            id: NodeId::from("g600-node".to_owned()),
            vendor_id: 0x046d,
            product_id: 0xc24a,
            usage_page: 0xff80,
            usage_id: 0x0080,
            name: "Logitech Gaming Mouse G600".to_owned(),
            manufacturer: None,
            serial_number: None,
        }
    }

    fn raw_route(product_id: u16, usage_page: u16, usage_id: u16, identity: &str) -> DeviceRoute {
        DeviceRoute::RawHid {
            vendor_id: 0x046d,
            product_id,
            usage_page,
            usage_id,
            identity: identity.to_owned(),
        }
    }

    fn g600_route() -> DeviceRoute {
        raw_route(0xc24a, 0xff80, 0x0080, "id:g600-node")
    }

    /// A backend whose only feature-report node is `nodes`.
    struct FakeBackend {
        nodes: Vec<NodeInfo>,
        reports: std::collections::HashMap<u8, Vec<u8>>,
    }

    impl FakeBackend {
        fn with_g600() -> Self {
            Self {
                nodes: vec![g600_node()],
                reports: [ACTIVE, FIRST, SECOND, THIRD]
                    .map(bytes)
                    .into_iter()
                    .map(|report| (report[0], report))
                    .collect(),
            }
        }
    }

    #[hidpp::async_trait]
    impl HidBackend for FakeBackend {
        async fn enumerate(&self) -> Result<Vec<NodeInfo>, BackendError> {
            Ok(self.nodes.clone())
        }

        async fn enumerate_hidpp(&self) -> Result<Vec<NodeInfo>, BackendError> {
            Ok(Vec::new())
        }

        async fn open_hidpp(
            &self,
            _node: &NodeInfo,
        ) -> Result<Option<std::sync::Arc<hidpp::channel::HidppChannel>>, BackendError> {
            Ok(None)
        }

        async fn open_raw_writer(
            &self,
            _node: &NodeInfo,
        ) -> Result<Box<dyn crate::backend::RawWriter>, BackendError> {
            Err(BackendError::Backend("no raw writer".to_owned()))
        }

        async fn open_feature_reader(
            &self,
            _node: &NodeInfo,
        ) -> Result<Box<dyn FeatureReader>, BackendError> {
            Ok(Box::new(FakeReader {
                reports: self.reports.clone(),
                ..FakeReader::default()
            }))
        }

        fn watch(&self) -> Result<crate::backend::HotplugStream, BackendError> {
            Err(BackendError::Backend("no hotplug".to_owned()))
        }
    }

    #[test]
    fn snapshot_reads_the_active_report_and_every_profile() {
        let snapshot =
            futures_lite::future::block_on(read_snapshot(&FakeBackend::with_g600(), &g600_route()))
                .expect("a connected G600 can be read");
        assert_eq!(snapshot.active_profile_report, bytes(ACTIVE));
        assert_eq!(
            snapshot.profiles.map(|p| p.slot()),
            [ProfileSlot::First, ProfileSlot::Second, ProfileSlot::Third]
        );
    }

    #[test]
    fn snapshot_refuses_a_route_that_is_not_a_g600() {
        let backend = FakeBackend::with_g600();
        // A Litra's raw collection, and a route that is not raw HID at all.
        for route in [
            raw_route(0xc900, 0xff43, 0x0202, "id:g600-node"),
            raw_route(0xc24a, 0x0001, 0x0002, "id:g600-node"),
            DeviceRoute::Direct {
                vendor_id: 0x046d,
                product_id: 0xc24a,
            },
        ] {
            let error = futures_lite::future::block_on(read_snapshot(&backend, &route))
                .expect_err("only a G600 vendor collection may be read");
            assert!(matches!(error, ReadError::NotAG600), "{route}: {error}");
        }
    }

    #[test]
    fn snapshot_reports_a_missing_device() {
        let backend = FakeBackend {
            nodes: Vec::new(),
            reports: std::collections::HashMap::new(),
        };
        let error = futures_lite::future::block_on(read_snapshot(&backend, &g600_route()))
            .expect_err("nothing is connected");
        assert!(matches!(error, ReadError::DeviceNotFound));
    }

    #[test]
    fn snapshot_will_not_choose_between_identical_nodes() {
        let backend = FakeBackend {
            nodes: vec![g600_node(), g600_node()],
            reports: std::collections::HashMap::new(),
        };
        let error = futures_lite::future::block_on(read_snapshot(&backend, &g600_route()))
            .expect_err("two nodes answer to one route");
        assert!(matches!(
            error,
            ReadError::Route(WriteError::AmbiguousRawDevice)
        ));
    }

    #[test]
    fn every_function_code_decodes() {
        for (code, function) in [
            (0x11, Function::DpiUp),
            (0x12, Function::DpiDown),
            (0x13, Function::DpiCycle),
            (0x14, Function::ProfileCycle),
            (0x15, Function::DpiShift),
            (0x17, Function::GShift),
        ] {
            assert_eq!(
                ButtonAssignment::decode([code, 0, 0]),
                ButtonAssignment::Function(function)
            );
        }
    }
}
