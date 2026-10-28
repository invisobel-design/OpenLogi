//! `openlogi diag g600` — read a Logitech G600's onboard profiles.
//!
//! The G600 is not a HID++ device: its button table lives in vendor feature
//! reports. This command only reads them and never writes to the mouse.

use anyhow::{Context, Result, anyhow};
use clap::Args;
use openlogi_hid::g600::{ButtonAssignment, GButton, Layer, Profile, Snapshot, is_g600_route};

#[derive(Debug, Args)]
pub struct G600Args {
    /// Also print every report as raw hex, report ID first.
    #[arg(long)]
    pub raw: bool,
}

pub async fn run(args: G600Args) -> Result<()> {
    let devices = openlogi_hid::enumerate_standalone()
        .await
        .context("failed to enumerate standalone HID devices")?;
    let mut routes = devices
        .iter()
        .map(openlogi_core::device::StandaloneDevice::route)
        .filter(is_g600_route);
    let route = routes
        .next()
        .ok_or_else(|| anyhow!("no G600 found — is it plugged in?"))?;
    if routes.next().is_some() {
        return Err(anyhow!("more than one G600 found; unplug all but one"));
    }
    let snapshot = openlogi_hid::read_g600_snapshot(&route)
        .await
        .context("failed to read the G600")?;
    print!("{}", render(&snapshot, args.raw));
    Ok(())
}

/// The text `run` prints for `snapshot`.
fn render(snapshot: &Snapshot, raw: bool) -> String {
    use std::fmt::Write as _;

    let mut out = String::new();
    let active = hex(&snapshot.active_profile_report);
    let _ = writeln!(out, "active-profile report (not decoded): {active}");
    for profile in &snapshot.profiles {
        let _ = writeln!(out);
        let _ = write!(out, "{}", render_profile(profile));
        if raw {
            let _ = writeln!(out, "  raw: {}", hex(profile.as_bytes()));
        }
    }
    out
}

fn render_profile(profile: &Profile) -> String {
    use std::fmt::Write as _;

    let mut out = String::new();
    let _ = writeln!(out, "profile {:?}", profile.slot());
    let [r, g, b] = profile.led_color();
    let _ = writeln!(out, "  led: #{r:02x}{g:02x}{b:02x}");
    let _ = writeln!(
        out,
        "  report-rate code: {:#04x}",
        profile.report_rate_code()
    );
    let _ = writeln!(out, "  dpi shift: {}", dpi_text(profile.dpi_shift()));
    let slots = profile.dpi_slots().map(dpi_text).join(", ");
    let _ = writeln!(
        out,
        "  dpi slots: {slots} (default slot {})",
        profile.default_dpi_slot()
    );
    for layer in [Layer::Normal, Layer::Shifted] {
        let _ = writeln!(out, "  {layer:?} layer:");
        for button in GButton::all() {
            let _ = writeln!(
                out,
                "    G{:<2} {}",
                button.number(),
                assignment_text(profile.button(layer, button))
            );
        }
    }
    out
}

fn dpi_text(dpi: Option<u16>) -> String {
    dpi.map_or_else(|| "unset".to_owned(), |dpi| dpi.to_string())
}

fn assignment_text(assignment: ButtonAssignment) -> String {
    match assignment {
        ButtonAssignment::Unassigned => "unassigned".to_owned(),
        ButtonAssignment::Keyboard { modifiers, usage } => {
            format!(
                "key usage {usage:#04x}, modifiers {:#04x}",
                modifiers.bits()
            )
        }
        ButtonAssignment::Mouse(button) => format!("{button:?}"),
        ButtonAssignment::Function(function) => format!("{function:?}"),
        ButtonAssignment::Unrecognized {
            code,
            modifiers,
            usage,
        } => format!("unrecognized {code:02x} {modifiers:02x} {usage:02x}"),
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::{assignment_text, dpi_text, hex};
    use openlogi_hid::g600::{ButtonAssignment, Function, ModifierMask};

    #[test]
    fn unknown_entries_print_their_bytes_so_nothing_is_hidden() {
        assert_eq!(
            assignment_text(ButtonAssignment::Unrecognized {
                code: 0x16,
                modifiers: 0,
                usage: 0
            }),
            "unrecognized 16 00 00"
        );
    }

    #[test]
    fn entries_render_by_kind() {
        assert_eq!(assignment_text(ButtonAssignment::Unassigned), "unassigned");
        assert_eq!(
            assignment_text(ButtonAssignment::Function(Function::DpiUp)),
            "DpiUp"
        );
        assert_eq!(
            assignment_text(ButtonAssignment::Keyboard {
                modifiers: ModifierMask::new(0x02),
                usage: 0x05
            }),
            "key usage 0x05, modifiers 0x02"
        );
    }

    #[test]
    fn unset_dpi_and_hex_render() {
        assert_eq!(dpi_text(None), "unset");
        assert_eq!(dpi_text(Some(800)), "800");
        assert_eq!(hex(&[0xf0, 0x09, 0x00]), "f0 09 00");
    }
}
