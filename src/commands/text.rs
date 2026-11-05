//! `aorus-lcd text`: shows text on the panel.

use aorus_lcd::protocol::{Framebuffer, UploadParams, DESCRIPTOR};
use aorus_lcd::text::{render, Style};
use aorus_lcd::Result;

use super::color::parse_hex_color;
use super::upload;

/// Arguments of `aorus-lcd text`.
#[derive(Debug, clap::Args)]
pub struct Args {
    /// The text, centred on the panel. A line break or a literal \n starts a new
    /// line, and a line too wide for the panel wraps on its spaces
    #[arg(value_name = "TEXT")]
    pub text: String,

    /// Text colour as six hex digits. The default grey is what the rainbow effect
    /// needs, because it uses the grey as a luminance mask
    #[arg(long, value_name = "RRGGBB", default_value = "8b8d8b", value_parser = parse_hex_color)]
    pub color: [u8; 3],

    /// Background colour as six hex digits
    #[arg(long, value_name = "RRGGBB", default_value = "000000", value_parser = parse_hex_color)]
    pub bg: [u8; 3],

    /// Font size in pixels to the em
    #[arg(long, value_name = "PIXELS", default_value_t = 28)]
    pub size: u32,

    /// Do not start the rainbow effect. Without this flag the effect's frame, AA,
    /// also saves the panel configuration to NVRAM, so this flag avoids that write
    #[arg(long)]
    pub no_effect: bool,

    #[arg(long, help = upload::NO_OVERLAY_HELP)]
    pub no_overlay: bool,
}

/// Renders the text, then uploads it. Rendering comes first because connecting
/// already puts the probe on the bus, or in a dry run prints it.
///
/// `AA` is the reference's rainbow trigger (`cli_text` in `aorus_lcd.py`) and also
/// the save frame, so it takes the upload's save path, with its notice.
pub fn run(args: Args, globals: &crate::Globals) -> Result<()> {
    let style = Style {
        size: args.size,
        color: args.color,
        background: args.bg,
    };
    let pixels = render(&with_line_breaks(&args.text), &style)?;
    let payload = [&DESCRIPTOR[..], &pixels].concat();
    upload::send(
        &payload,
        &UploadParams::still(Framebuffer::Text),
        upload::Finish {
            clear_overlay: args.no_overlay,
            persist: !args.no_effect,
        },
        globals,
    )
}

/// Most shells pass `\n` on as two characters, so it would otherwise show up on
/// the panel as a backslash.
fn with_line_breaks(text: &str) -> String {
    text.replace("\\n", "\n")
}

#[cfg(test)]
mod tests {
    use clap::{CommandFactory, Parser};

    use super::*;

    #[derive(Debug, Parser)]
    struct Harness {
        #[command(flatten)]
        args: Args,
    }

    #[test]
    fn the_defaults_are_those_of_the_reference() {
        let args = Harness::try_parse_from(["text", "hello"]).unwrap().args;

        assert_eq!(args.size, 28);
        assert_eq!(args.color, [0x8B, 0x8D, 0x8B]);
        assert_eq!(args.bg, [0, 0, 0]);
        assert!(!args.no_effect);
        assert!(!args.no_overlay);
    }

    #[test]
    fn colours_take_a_hash() {
        let args = Harness::try_parse_from(["text", "x", "--color", "#ff8800", "--bg", "102030"])
            .unwrap()
            .args;

        assert_eq!(args.color, [0xFF, 0x88, 0x00]);
        assert_eq!(args.bg, [0x10, 0x20, 0x30]);
    }

    #[test]
    fn a_bad_colour_is_refused_while_parsing() {
        for option in ["--color", "--bg"] {
            let error = Harness::try_parse_from(["text", "x", option, "12345"]).unwrap_err();

            assert!(error.to_string().contains("six hex digits"), "{error}");
        }
    }

    #[test]
    fn text_is_required() {
        assert!(Harness::try_parse_from(["text"]).is_err());
    }

    // Rendered in-process so the test never has to run the binary.
    #[test]
    fn help_says_that_the_effect_writes_nvram_and_how_to_avoid_it() {
        let help = Harness::command().render_long_help().to_string();
        let short = Harness::command().render_help().to_string();

        for help in [help, short] {
            assert!(help.contains("--no-effect"), "{help}");
            assert!(help.contains("NVRAM"), "{help}");
        }
    }

    // Rendered in-process so the test never has to run the binary.
    #[test]
    fn help_says_that_no_overlay_sends_e1() {
        let help = Harness::command().render_help().to_string();

        assert!(help.contains("--no-overlay"), "{help}");
        assert!(help.contains("E1"), "{help}");
    }

    #[test]
    fn a_literal_backslash_n_is_a_line_break() {
        assert_eq!(with_line_breaks(r"one\ntwo"), "one\ntwo");
        assert_eq!(with_line_breaks("one\ntwo"), "one\ntwo");
    }
}
