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
    ///
    /// Disabled: on a real G600 this wrote a shifted copy of the old profile
    /// instead of the change. Use `write-raw` to study that.
    Set(SetArgs),
    /// Write exact bytes to one onboard profile and watch how the mouse stores
    /// them. A diagnostic; it can leave the profile scrambled.
    WriteRaw(WriteRawArgs),
}

#[derive(Debug, Args)]
pub struct WriteRawArgs {
    /// Which onboard profile to write, 1 to 3. Must match the report ID of the
    /// bytes.
    #[arg(long, value_parser = clap::value_parser!(u8).range(1..=3))]
    pub profile: u8,

    /// The whole report as hexadecimal bytes separated by spaces, report ID
    /// first (`f3 00 00 ...`). Exactly what `show --raw` prints after `raw:`.
    #[arg(long, value_parser = parse_hex)]
    pub hex: HexBytes,

    /// Send this many bytes: the report is cut, or padded with zeros, to this
    /// length. The default sends it whole.
    #[arg(long)]
    pub send_len: Option<usize>,
}

/// Bytes given as hexadecimal text. A type of its own so clap reads the whole
/// argument as one value instead of one byte per occurrence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HexBytes(pub Vec<u8>);

/// Parse space-separated hexadecimal bytes.
fn parse_hex(text: &str) -> Result<HexBytes, String> {
    text.split_whitespace()
        .map(|byte| u8::from_str_radix(byte, 16).map_err(|_| format!("`{byte}` is not a hex byte")))
        .collect::<Result<_, _>>()
        .map(HexBytes)
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
            Self::WriteRaw(args) => write_raw(&args).await,
        }
    }
}

/// Whether `set` may run. See [`set`].
const ENABLE_SET: bool = false;

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
    // Disabled after it scrambled a real profile; kept so the arguments and
    // the checked write path stay in place for when writes are understood.
    if !ENABLE_SET {
        return Err(anyhow!(
            "`set` is disabled: on a real G600 the mouse stored a shifted copy of the \
             old profile instead of the change. Run `diag g600 write-raw` to study it."
        ));
    }
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

/// How long after a write to read the profile back, in seconds.
const READ_BACK_AFTER: [u64; 3] = [0, 1, 3];

async fn write_raw(args: &WriteRawArgs) -> Result<()> {
    let wanted = Profile::parse(&args.hex.0).context("--hex is not a whole profile report")?;
    let slot = ProfileSlot::ALL[usize::from(args.profile - 1)];
    if wanted.slot() != slot {
        return Err(anyhow!(
            "--profile {} does not match the report ID {:#04x} in --hex",
            args.profile,
            wanted.as_bytes()[0]
        ));
    }
    let mut report = wanted.as_bytes().to_vec();
    if let Some(len) = args.send_len {
        report.resize(len, 0);
    }
    let route = route().await?;
    println!("writing {} bytes to profile {}", report.len(), args.profile);
    openlogi_hid::write_g600_profile_report(&route, &report)
        .await
        .context("the write failed")?;
    let mut elapsed = 0;
    for after in READ_BACK_AFTER {
        tokio::time::sleep(std::time::Duration::from_secs(after - elapsed)).await;
        elapsed = after;
        let snapshot = openlogi_hid::read_g600_snapshot(&route)
            .await
            .context("failed to read the G600 back")?;
        let stored = &snapshot.profiles[usize::from(args.profile - 1)];
        println!(
            "after {after}s: {}",
            compare(wanted.as_bytes(), stored.as_bytes())
        );
        if stored != &wanted {
            println!("  stored: {}", hex(stored.as_bytes()));
        }
    }
    Ok(())
}

/// How a stored report relates to the one that was written.
fn compare(written: &[u8], stored: &[u8]) -> String {
    if written == stored {
        return "identical to what was written".to_owned();
    }
    let differing = written
        .iter()
        .zip(stored)
        .filter(|(written, stored)| written != stored)
        .count();
    match shift_between(written, stored) {
        Some(shift) => format!(
            "differs in {differing} bytes; the stored bytes look like what was written moved {shift:+} position(s)"
        ),
        None => format!("differs in {differing} bytes, and not by a simple shift"),
    }
}

/// The shift that turns `written` into `stored`, if one does: the stored byte
/// at `i` is the written byte at `i - shift` across the middle of the report,
/// where the ends cannot confuse it.
fn shift_between(written: &[u8], stored: &[u8]) -> Option<isize> {
    let (start, end) = (8_usize, written.len().min(stored.len()).checked_sub(8)?);
    (-4..=4_isize).filter(|shift| *shift != 0).find(|&shift| {
        (start..end).all(|i| {
            i.checked_add_signed(-shift)
                .and_then(|from| written.get(from))
                .is_some_and(|byte| *byte == stored[i])
        })
    })
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
    use super::{G600Cmd, compare, dpi_text, hex, parse_hex, shift_between};
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
    fn set_is_disabled_until_writes_are_understood() {
        let G600Cmd::Set(args) = parse(&[
            "set",
            "--profile",
            "2",
            "--button",
            "7",
            "--assign",
            "key:b",
        ])
        .expect("valid") else {
            panic!("expected set");
        };
        let error = futures::executor::block_on(super::set(&args))
            .expect_err("set must refuse before touching a device");
        assert!(error.to_string().contains("disabled"), "{error}");
    }

    #[test]
    fn write_raw_takes_hex_bytes_and_refuses_bad_ones() {
        let G600Cmd::WriteRaw(args) = parse(&[
            "write-raw",
            "--profile",
            "1",
            "--hex",
            "f3 00 ff",
            "--send-len",
            "153",
        ])
        .expect("valid") else {
            panic!("expected write-raw");
        };
        assert_eq!(args.hex.0, [0xf3, 0x00, 0xff]);
        assert_eq!(args.send_len, Some(153));
        assert_eq!(parse_hex("zz"), Err("`zz` is not a hex byte".to_owned()));
        assert_eq!(parse_hex("f3 0a"), Ok(super::HexBytes(vec![0xf3, 0x0a])));
        parse(&["write-raw", "--profile", "1", "--hex", "f3 xx"])
            .expect_err("a bad byte is refused before any device is touched");
    }

    #[test]
    fn a_shifted_copy_is_recognised_and_other_differences_are_not() {
        let written: Vec<u8> = (0..154).map(|i| (i * 7 % 251) as u8).collect();
        let mut shifted = vec![0, 0];
        shifted.extend_from_slice(&written[1..153]);
        assert_eq!(shift_between(&written, &shifted), Some(1));
        assert!(compare(&written, &shifted).contains("+1"));
        assert_eq!(compare(&written, &written), "identical to what was written");

        let mut edited = written.clone();
        edited[60] ^= 0xff;
        assert!(compare(&written, &edited).contains("not by a simple shift"));
        assert_eq!(shift_between(&written, &[]), None);
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
