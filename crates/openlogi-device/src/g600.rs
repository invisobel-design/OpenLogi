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

mod spec;

pub use spec::ParseAssignmentError;

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
    /// The button's number, `1` through `5`.
    #[must_use]
    pub const fn number(self) -> u8 {
        self.code()
    }

    const fn code(self) -> u8 {
        match self {
            Self::Button1 => 0x01,
            Self::Button2 => 0x02,
            Self::Button3 => 0x03,
            Self::Button4 => 0x04,
            Self::Button5 => 0x05,
        }
    }

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
    const fn code(self) -> u8 {
        match self {
            Self::DpiUp => 0x11,
            Self::DpiDown => 0x12,
            Self::DpiCycle => 0x13,
            Self::ProfileCycle => 0x14,
            Self::DpiShift => 0x15,
            Self::GShift => 0x17,
        }
    }

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
    /// The three bytes this assignment is stored as: the inverse of decoding.
    #[must_use]
    pub const fn encode(self) -> [u8; BUTTON_ENTRY_LEN] {
        match self {
            Self::Unassigned => [0, 0, 0],
            Self::Keyboard { modifiers, usage } => [0x00, modifiers.bits(), usage],
            Self::Mouse(button) => [button.code(), 0, 0],
            Self::Function(function) => [function.code(), 0, 0],
            Self::Unrecognized {
                code,
                modifiers,
                usage,
            } => [code, modifiers, usage],
        }
    }

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
        let start = button_offset(layer, button);
        ButtonAssignment::decode([self.raw[start], self.raw[start + 1], self.raw[start + 2]])
    }

    /// This profile with `button` in `layer` set to `assignment`.
    ///
    /// Every other byte is carried over unchanged, including the regions this
    /// module does not decode.
    #[must_use]
    pub fn with_button(&self, layer: Layer, button: GButton, assignment: ButtonAssignment) -> Self {
        let start = button_offset(layer, button);
        let mut raw = self.raw;
        raw[start..start + BUTTON_ENTRY_LEN].copy_from_slice(&assignment.encode());
        Self {
            slot: self.slot,
            raw,
        }
    }

    fn rgb(&self, start: usize) -> [u8; 3] {
        [self.raw[start], self.raw[start + 1], self.raw[start + 2]]
    }
}

/// Offset of `button`'s entry in `layer`'s table within a profile report.
fn button_offset(layer: Layer, button: GButton) -> usize {
    let table = match layer {
        Layer::Normal => BUTTONS,
        Layer::Shifted => SHIFT_BUTTONS,
    };
    table + BUTTON_ENTRY_LEN * button.index()
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

/// Whether an edit may take away the mouse's primary clicks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrimaryButtons {
    /// Refuse to change G1 and G2 of the normal layer — the left and right
    /// click — to anything but what they are.
    Protect,
    /// Allow it. A mouse whose clicks are gone can still be repaired from the
    /// keyboard, but only by someone who knows to.
    Allow,
}

/// One button edit: which entry to change, and to what.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ButtonEdit {
    /// The profile to change.
    pub slot: ProfileSlot,
    /// The button table to change.
    pub layer: Layer,
    /// The button to change.
    pub button: GButton,
    /// What the button should do afterwards.
    pub assignment: ButtonAssignment,
}

/// What an edit did, so a caller can report it or undo it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EditOutcome {
    /// The profile as it was before the edit.
    pub before: Profile,
    /// The profile as read back from the mouse after the edit.
    pub after: Profile,
    /// Whether anything was written. `false` when the button already held the
    /// requested assignment.
    pub written: bool,
}

/// Why a button edit did not happen, or could not be confirmed.
#[derive(Debug, Error)]
pub enum EditError {
    /// Reading the profile, before or after the write, failed.
    #[error(transparent)]
    Read(#[from] ReadError),
    /// Writing the report failed.
    #[error("writing the profile failed: {0}")]
    Write(BackendError),
    /// The edit would change the left or right click and the caller did not
    /// allow that.
    #[error(
        "G{} in the normal layer is a primary click; changing it needs PrimaryButtons::Allow",
        button.number()
    )]
    PrimaryButton {
        /// The protected button.
        button: GButton,
    },
    /// The profile read back after the write differs from what was written.
    ///
    /// The mouse may now hold something other than either profile; `before` is
    /// what to restore.
    #[error("the mouse did not keep the profile that was written")]
    NotKept {
        /// The profile as it was before the write.
        before: Box<Profile>,
        /// The profile read back.
        read_back: Box<Profile>,
    },
}

/// Whether `edit` would change what a primary click (G1 or G2, normal layer) does.
fn touches_primary(edit: ButtonEdit, current: &Profile) -> bool {
    edit.layer == Layer::Normal
        && matches!(edit.button.number(), 1 | 2)
        && current.button(edit.layer, edit.button) != edit.assignment
}

/// Set one button of one onboard profile, then confirm by reading it back.
///
/// Reads the profile, changes only the requested entry, writes the whole
/// report, reads it again and compares. Nothing is written when the button
/// already holds the assignment.
///
/// # Errors
///
/// [`EditError::PrimaryButton`] before anything is written when `guard`
/// protects a primary click that the edit would change; the read and write
/// errors of the backend; and [`EditError::NotKept`] when the read-back
/// differs from what was written.
pub async fn edit_button(
    backend: &dyn HidBackend,
    route: &DeviceRoute,
    edit: ButtonEdit,
    guard: PrimaryButtons,
) -> Result<EditOutcome, EditError> {
    if !is_g600_route(route) {
        return Err(ReadError::NotAG600.into());
    }
    let node = find_raw_route_node(backend, route)
        .await
        .map_err(ReadError::from)?
        .ok_or(ReadError::DeviceNotFound)?;
    let mut reader = backend
        .open_feature_reader(&node)
        .await
        .map_err(ReadError::from)?;
    let before = read_profile(reader.as_mut(), edit.slot).await?;
    if guard == PrimaryButtons::Protect && touches_primary(edit, &before) {
        return Err(EditError::PrimaryButton {
            button: edit.button,
        });
    }
    let wanted = before.with_button(edit.layer, edit.button, edit.assignment);
    if wanted == before {
        return Ok(EditOutcome {
            after: before.clone(),
            before,
            written: false,
        });
    }
    let mut writer = backend
        .open_feature_writer(&node)
        .await
        .map_err(EditError::Write)?;
    writer
        .write_feature_report(wanted.as_bytes())
        .await
        .map_err(EditError::Write)?;
    let after = read_profile(reader.as_mut(), edit.slot).await?;
    if after != wanted {
        return Err(EditError::NotKept {
            before: Box::new(before),
            read_back: Box::new(after),
        });
    }
    Ok(EditOutcome {
        before,
        after,
        written: true,
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

    type Store = std::sync::Arc<std::sync::Mutex<std::collections::HashMap<u8, Vec<u8>>>>;

    /// What a fake mouse does with a written report.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Writes {
        /// Keep it, so a read returns what was written.
        Keep,
        /// Accept it and forget it.
        Ignore,
        /// Fail the write.
        Fail,
    }

    /// A backend whose only feature-report node is `nodes`, with a mouse behind
    /// it that remembers its reports and logs every report written.
    struct FakeBackend {
        nodes: Vec<NodeInfo>,
        reports: Store,
        writes: Writes,
        written: std::sync::Arc<std::sync::Mutex<Vec<Vec<u8>>>>,
    }

    impl FakeBackend {
        fn new(nodes: Vec<NodeInfo>, reports: &[&str]) -> Self {
            Self {
                nodes,
                reports: std::sync::Arc::new(std::sync::Mutex::new(
                    reports
                        .iter()
                        .map(|report| bytes(report))
                        .map(|report| (report[0], report))
                        .collect(),
                )),
                writes: Writes::Keep,
                written: std::sync::Arc::default(),
            }
        }

        fn with_g600() -> Self {
            Self::new(vec![g600_node()], &[ACTIVE, FIRST, SECOND, THIRD])
        }

        fn with_writes(mut self, writes: Writes) -> Self {
            self.writes = writes;
            self
        }

        fn stored(&self, report_id: u8) -> Vec<u8> {
            self.reports.lock().expect("store")[&report_id].clone()
        }

        fn writes_seen(&self) -> Vec<Vec<u8>> {
            self.written.lock().expect("log").clone()
        }
    }

    /// The fake mouse's feature-report port.
    struct FakePort {
        reports: Store,
        writes: Writes,
        written: std::sync::Arc<std::sync::Mutex<Vec<Vec<u8>>>>,
    }

    #[hidpp::async_trait]
    impl FeatureReader for FakePort {
        async fn read_feature_report(
            &mut self,
            report_id: u8,
            _max_len: usize,
        ) -> Result<Vec<u8>, BackendError> {
            self.reports
                .lock()
                .expect("store")
                .get(&report_id)
                .cloned()
                .ok_or(BackendError::Disconnected)
        }
    }

    #[hidpp::async_trait]
    impl crate::backend::FeatureWriter for FakePort {
        async fn write_feature_report(&mut self, report: &[u8]) -> Result<(), BackendError> {
            self.written.lock().expect("log").push(report.to_vec());
            match self.writes {
                Writes::Keep => {
                    self.reports
                        .lock()
                        .expect("store")
                        .insert(report[0], report.to_vec());
                    Ok(())
                }
                Writes::Ignore => Ok(()),
                Writes::Fail => Err(BackendError::Backend("write refused".to_owned())),
            }
        }
    }

    impl FakeBackend {
        fn port(&self) -> FakePort {
            FakePort {
                reports: self.reports.clone(),
                writes: self.writes,
                written: self.written.clone(),
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
            Ok(Box::new(self.port()))
        }

        async fn open_feature_writer(
            &self,
            _node: &NodeInfo,
        ) -> Result<Box<dyn crate::backend::FeatureWriter>, BackendError> {
            Ok(Box::new(self.port()))
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
        let backend = FakeBackend::new(Vec::new(), &[]);
        let error = futures_lite::future::block_on(read_snapshot(&backend, &g600_route()))
            .expect_err("nothing is connected");
        assert!(matches!(error, ReadError::DeviceNotFound));
    }

    #[test]
    fn snapshot_will_not_choose_between_identical_nodes() {
        let backend = FakeBackend::new(vec![g600_node(), g600_node()], &[]);
        let error = futures_lite::future::block_on(read_snapshot(&backend, &g600_route()))
            .expect_err("two nodes answer to one route");
        assert!(matches!(
            error,
            ReadError::Route(WriteError::AmbiguousRawDevice)
        ));
    }

    fn edit(slot: ProfileSlot, layer: Layer, number: u8, text: &str) -> ButtonEdit {
        ButtonEdit {
            slot,
            layer,
            button: g(number),
            assignment: text.parse().expect("valid assignment text"),
        }
    }

    fn run_edit(
        backend: &FakeBackend,
        edit: ButtonEdit,
        guard: PrimaryButtons,
    ) -> Result<EditOutcome, EditError> {
        futures_lite::future::block_on(edit_button(backend, &g600_route(), edit, guard))
    }

    /// Positions at which two reports differ.
    fn differing(a: &[u8], b: &[u8]) -> Vec<usize> {
        (0..a.len().max(b.len()))
            .filter(|&i| a.get(i) != b.get(i))
            .collect()
    }

    #[test]
    fn an_edit_changes_exactly_one_entry_and_nothing_else() {
        for (slot, layer, number, text, offset) in [
            (
                ProfileSlot::First,
                Layer::Normal,
                9,
                "key:ctrl+b",
                31 + 8 * 3,
            ),
            (
                ProfileSlot::First,
                Layer::Shifted,
                12,
                "mouse:3",
                94 + 11 * 3,
            ),
            // Profile 3 holds a code this module cannot name; an edit elsewhere
            // must leave it alone.
            (
                ProfileSlot::Third,
                Layer::Normal,
                6,
                "fn:dpi-up",
                31 + 5 * 3,
            ),
        ] {
            let backend = FakeBackend::with_g600();
            let before = backend.stored(slot.report_id());
            let outcome = run_edit(
                &backend,
                edit(slot, layer, number, text),
                PrimaryButtons::Protect,
            )
            .expect("the edit is kept");
            let after = backend.stored(slot.report_id());
            assert!(outcome.written);
            assert_eq!(outcome.after.as_bytes().as_slice(), after.as_slice());
            assert_eq!(
                outcome.after.button(layer, g(number)),
                text.parse::<ButtonAssignment>().expect("valid text")
            );
            assert!(
                differing(&before, &after)
                    .iter()
                    .all(|i| (offset..offset + 3).contains(i)),
                "{slot:?} {layer:?} G{number} changed bytes outside its own entry"
            );
            for other in ProfileSlot::ALL.into_iter().filter(|other| *other != slot) {
                let untouched = match other {
                    ProfileSlot::First => bytes(FIRST),
                    ProfileSlot::Second => bytes(SECOND),
                    ProfileSlot::Third => bytes(THIRD),
                };
                assert_eq!(backend.stored(other.report_id()), untouched);
            }
            assert_eq!(backend.writes_seen(), vec![after]);
        }
    }

    #[test]
    fn an_unrecognized_entry_survives_an_edit_of_its_neighbour() {
        let backend = FakeBackend::with_g600();
        run_edit(
            &backend,
            edit(ProfileSlot::Third, Layer::Normal, 15, "key:f13"),
            PrimaryButtons::Protect,
        )
        .expect("the edit is kept");
        let third = Profile::parse(&backend.stored(0xf5)).expect("a profile");
        assert_eq!(
            third.button(Layer::Normal, g(16)),
            ButtonAssignment::Unrecognized {
                code: 0x16,
                modifiers: 0,
                usage: 0
            }
        );
    }

    #[test]
    fn setting_what_is_already_set_writes_nothing() {
        let backend = FakeBackend::with_g600();
        let outcome = run_edit(
            &backend,
            edit(ProfileSlot::First, Layer::Normal, 8, "fn:profile-cycle"),
            PrimaryButtons::Protect,
        )
        .expect("a no-op edit succeeds");
        assert!(!outcome.written);
        assert_eq!(outcome.before, outcome.after);
        assert!(backend.writes_seen().is_empty());
    }

    #[test]
    fn the_primary_clicks_are_protected_unless_allowed() {
        for number in [1, 2] {
            let backend = FakeBackend::with_g600();
            let error = run_edit(
                &backend,
                edit(ProfileSlot::First, Layer::Normal, number, "key:a"),
                PrimaryButtons::Protect,
            )
            .expect_err("a primary click is protected");
            assert!(
                matches!(error, EditError::PrimaryButton { button } if button == g(number)),
                "{error}"
            );
            assert!(backend.writes_seen().is_empty(), "nothing may be written");

            run_edit(
                &backend,
                edit(ProfileSlot::First, Layer::Normal, number, "key:a"),
                PrimaryButtons::Allow,
            )
            .expect("allowed on request");
            assert_eq!(backend.writes_seen().len(), 1);
        }
    }

    #[test]
    fn protection_covers_only_the_normal_layers_two_clicks() {
        let backend = FakeBackend::with_g600();
        for (layer, number) in [(Layer::Shifted, 1), (Layer::Normal, 3), (Layer::Normal, 20)] {
            run_edit(
                &backend,
                edit(ProfileSlot::First, layer, number, "key:a"),
                PrimaryButtons::Protect,
            )
            .expect("not a primary click");
        }
        // Keeping a primary click as it is is not changing it.
        run_edit(
            &backend,
            edit(ProfileSlot::First, Layer::Normal, 1, "mouse:1"),
            PrimaryButtons::Protect,
        )
        .expect("unchanged");
    }

    #[test]
    fn a_mouse_that_does_not_keep_the_write_is_reported_with_the_original() {
        let backend = FakeBackend::with_g600().with_writes(Writes::Ignore);
        let error = run_edit(
            &backend,
            edit(ProfileSlot::First, Layer::Normal, 9, "key:b"),
            PrimaryButtons::Protect,
        )
        .expect_err("the read-back differs");
        let EditError::NotKept { before, read_back } = error else {
            panic!("expected NotKept, got {error}");
        };
        assert_eq!(before.as_bytes().as_slice(), bytes(FIRST).as_slice());
        assert_eq!(read_back.as_bytes().as_slice(), bytes(FIRST).as_slice());
    }

    #[test]
    fn a_failed_write_is_an_error_not_a_silent_success() {
        let backend = FakeBackend::with_g600().with_writes(Writes::Fail);
        let error = run_edit(
            &backend,
            edit(ProfileSlot::First, Layer::Normal, 9, "key:b"),
            PrimaryButtons::Protect,
        )
        .expect_err("the write fails");
        assert!(matches!(error, EditError::Write(_)), "{error}");
    }

    #[test]
    fn an_edit_refuses_a_route_that_is_not_a_g600_and_writes_nothing() {
        let backend = FakeBackend::with_g600();
        let error = futures_lite::future::block_on(edit_button(
            &backend,
            &raw_route(0xc900, 0xff43, 0x0202, "id:g600-node"),
            edit(ProfileSlot::First, Layer::Normal, 9, "key:b"),
            PrimaryButtons::Protect,
        ))
        .expect_err("not a G600");
        assert!(matches!(error, EditError::Read(ReadError::NotAG600)));
        assert!(backend.writes_seen().is_empty());
    }

    #[test]
    fn re_setting_every_entry_to_what_it_decodes_as_is_lossless() {
        for report in [FIRST, SECOND, THIRD] {
            let profile = profile(report);
            for layer in [Layer::Normal, Layer::Shifted] {
                for button in GButton::all() {
                    assert_eq!(
                        profile.with_button(layer, button, profile.button(layer, button)),
                        profile,
                        "{layer:?} G{}",
                        button.number()
                    );
                }
            }
        }
    }

    #[test]
    fn every_entry_encodes_back_to_the_bytes_it_decoded_from() {
        for code in 0..=255_u8 {
            for modifiers in [0x00, 0x01, 0x82] {
                for usage in [0x00, 0x05, 0xff] {
                    let entry = [code, modifiers, usage];
                    assert_eq!(ButtonAssignment::decode(entry).encode(), entry);
                }
            }
        }
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
