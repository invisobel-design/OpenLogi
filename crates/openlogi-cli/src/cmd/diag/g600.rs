//! `openlogi diag g600` — read and change a Logitech G600's onboard profiles.
//!
//! The G600 is not a HID++ device: its button table lives in vendor feature
//! reports. `show` only reads. `set` changes one button of one onboard profile
//! and reads the profile back to confirm the mouse kept it.

use std::fmt::Write as _;

use anyhow::{Context, Result, anyhow};
use clap::{Args, Subcommand};
use openlogi_core::device::StandaloneDevice;
use openlogi_hid::DeviceRoute;
use openlogi_hid::g600::{
    BUTTON_COUNT, ButtonAssignment, ButtonEdit, EditError, GButton, Layer, PrimaryButtons, Profile,
    ProfileSlot, Snapshot, is_g600_route,
};

#[derive(Debug, Subcommand)]
pub enum G600Cmd {
    /// Print the three onboard profiles. Reads only; nothing is written.
    Show(ShowArgs),
    /// Change one button of one onboard profile, then read it back to confirm.
    Set(SetArgs),
}

#[derive(Debug, Args)]
pub struct ShowArgs {
    /// Also print every report as raw hex, report ID first.
    #[arg(long)]
    pub raw: bool,
}

#[derive(Debug, Args)]
pub struct SetArgs {
    /// Which onboard profile to change, 1 to 3.
    #[arg(long, value_parser = clap::value_parser!(u8).range(1..=3))]
    pub profile: u8,

    /// Which button to change, 1 to 20 (G1 to G20).
    #[arg(long, value_parser = clap::value_parser!(u8).range(1..=i64::from(BUTTON_COUNT)))]
    pub button: u8,

    /// Change the button's G-shift entry (used while G-shift is held) instead
    /// of its normal one.
    #[arg(long)]
    pub shifted: bool,

    /// What the button should do: `none`, `mouse:1`..`mouse:5`, `fn:NAME`
    /// (dpi-up, dpi-down, dpi-cycle, profile-cycle, dpi-shift, g-shift),
    /// `key:ctrl+shift+b`, or `raw:CC,MM,UU`.
    #[arg(long, value_parser = str::parse::<ButtonAssignment>)]
    pub assign: ButtonAssignment,

    /// Allow changing G1 and G2 of the normal layer — the left and right click.
    /// Without this they are protected, so a typo cannot leave the mouse
    /// without clicks.
    #[arg(long)]
    pub allow_primary: bool,
}

impl G600Cmd {
    pub async fn run(self) -> Result<()> {
        match self {
            Self::Show(args) => show(&args).await,
            Self::Set(args) => set(&args).await,
        }
    }
}

/// The route of the one connected G600.
async fn route() -> Result<DeviceRoute> {
    let devices = openlogi_hid::enumerate_standalone()
        .await
        .context("failed to enumerate standalone HID devices")?;
    let mut routes = devices
        .iter()
        .map(StandaloneDevice::route)
        .filter(is_g600_route);
    let route = routes
        .next()
        .ok_or_else(|| anyhow!("no G600 found — is it plugged in?"))?;
    if routes.next().is_some() {
        return Err(anyhow!("more than one G600 found; unplug all but one"));
    }
    Ok(route)
}

async fn show(args: &ShowArgs) -> Result<()> {
    let snapshot = openlogi_hid::read_g600_snapshot(&route().await?)
        .await
        .context("failed to read the G600")?;
    print!("{}", render(&snapshot, args.raw));
    Ok(())
}

async fn set(args: &SetArgs) -> Result<()> {
    let slot = ProfileSlot::ALL[usize::from(args.profile - 1)];
    let button = GButton::new(args.button).ok_or_else(|| anyhow!("no such button"))?;
    let layer = if args.shifted {
        Layer::Shifted
    } else {
        Layer::Normal
    };
    let guard = if args.allow_primary {
        PrimaryButtons::Allow
    } else {
        PrimaryButtons::Protect
    };
    let edit = ButtonEdit {
        slot,
        layer,
        button,
        assignment: args.assign,
    };
    let outcome = openlogi_hid::edit_g600_button(&route().await?, edit, guard)
        .await
        .map_err(explain)?;
    let was = outcome.before.button(layer, button);
    let now = outcome.after.button(layer, button);
    println!(
        "profile {} G{} ({layer:?} layer): {was} -> {now}",
        args.profile, args.button
    );
    if outcome.written {
        println!("written, and the mouse reports the same profile back.");
    } else {
        println!("already set; nothing was written.");
    }
    Ok(())
}

/// An edit error, with what to do about the ones that need action.
fn explain(error: EditError) -> anyhow::Error {
    match error {
        EditError::PrimaryButton { button } => anyhow!(
            "G{} in the normal layer is a primary click; pass --allow-primary to change it",
            button.number()
        ),
        EditError::NotKept { before, read_back } => anyhow!(
            "the mouse did not keep the change. It reports:\n  {}\nIt held this before:\n  {}\n\
             Restore it with `diag g600 set` for each button that differs.",
            hex(read_back.as_bytes()),
            hex(before.as_bytes()),
        ),
        other => anyhow!(other).context("failed to change the G600"),
    }
}

/// The text `show` prints for `snapshot`.
fn render(snapshot: &Snapshot, raw: bool) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "active-profile report (not decoded): {}",
        hex(&snapshot.active_profile_report)
    );
    for profile in &snapshot.profiles {
        let _ = writeln!(out);
        out.push_str(&render_profile(profile));
        if raw {
            let _ = writeln!(out, "  raw: {}", hex(profile.as_bytes()));
        }
    }
    out
}

fn render_profile(profile: &Profile) -> String {
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
                profile.button(layer, button)
            );
        }
    }
    out
}

fn dpi_text(dpi: Option<u16>) -> String {
    dpi.map_or_else(|| "unset".to_owned(), |dpi| dpi.to_string())
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
    use super::{G600Cmd, dpi_text, hex};
    use crate::cmd::diag::DiagCmd;
    use clap::Parser;
    use openlogi_hid::g600::ButtonAssignment;

    #[derive(Debug, Parser)]
    struct Cli {
        #[command(subcommand)]
        cmd: DiagCmd,
    }

    fn parse(args: &[&str]) -> Result<G600Cmd, clap::Error> {
        let args = ["diag", "g600"].into_iter().chain(args.iter().copied());
        match Cli::try_parse_from(args)?.cmd {
            DiagCmd::G600(cmd) => Ok(cmd),
            other => panic!("expected the g600 command, got {other:?}"),
        }
    }

    #[test]
    fn set_takes_a_profile_a_button_and_an_assignment() {
        let G600Cmd::Set(args) = parse(&[
            "set",
            "--profile",
            "2",
            "--button",
            "7",
            "--assign",
            "key:ctrl+b",
        ])
        .expect("valid") else {
            panic!("expected set");
        };
        assert_eq!((args.profile, args.button), (2, 7));
        assert!(!args.shifted && !args.allow_primary);
        assert_eq!(args.assign.to_string(), "key:ctrl+b");
    }

    #[test]
    fn set_refuses_out_of_range_numbers_and_bad_assignments() {
        for args in [
            &["set", "--profile", "0", "--button", "1", "--assign", "none"][..],
            &["set", "--profile", "4", "--button", "1", "--assign", "none"],
            &[
                "set",
                "--profile",
                "1",
                "--button",
                "21",
                "--assign",
                "none",
            ],
            &["set", "--profile", "1", "--button", "0", "--assign", "none"],
            &[
                "set",
                "--profile",
                "1",
                "--button",
                "5",
                "--assign",
                "key:nope",
            ],
            &["set", "--profile", "1", "--button", "5"],
        ] {
            parse(args).expect_err("must be refused before touching a device");
        }
    }

    #[test]
    fn show_is_the_read_only_command() {
        let G600Cmd::Show(args) = parse(&["show", "--raw"]).expect("valid") else {
            panic!("expected show");
        };
        assert!(args.raw);
    }

    #[test]
    fn assignments_print_in_the_form_set_accepts() {
        for text in ["none", "mouse:3", "fn:dpi-up", "key:alt+f4", "raw:16,00,00"] {
            let assignment: ButtonAssignment = text.parse().expect("valid");
            assert_eq!(assignment.to_string(), text);
        }
    }

    #[test]
    fn unset_dpi_and_hex_render() {
        assert_eq!(dpi_text(None), "unset");
        assert_eq!(dpi_text(Some(800)), "800");
        assert_eq!(hex(&[0xf0, 0x09, 0x00]), "f0 09 00");
    }
}
