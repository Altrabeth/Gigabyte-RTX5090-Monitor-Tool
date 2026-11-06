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

    /// Text colour as six hex digits. The default grey is the reference's, which it
    /// describes as the luminance mask of the panel's rainbow. In one run on firmware
    /// 1.3 the text showed in a rainbow, not in the colour given here
    #[arg(long, value_name = "RRGGBB", default_value = "8b8d8b", value_parser = parse_hex_color)]
    pub color: [u8; 3],

    /// Background colour as six hex digits
    #[arg(long, value_name = "RRGGBB", default_value = "000000", value_parser = parse_hex_color)]
    pub bg: [u8; 3],

    /// Font size in pixels to the em
    #[arg(long, value_name = "PIXELS", default_value_t = 28)]
    pub size: u32,

    /// Skip AA, the frame that saves the panel configuration to NVRAM, so that this
    /// run writes no NVRAM. Do not expect it to stop the rainbow: in one run on
    /// firmware 1.3 with this flag the text still showed in a rainbow, with one AA
    /// sent earlier in mode 0 and none in that run. For a fixed colour, render the
    /// text to a picture and use the image command, which avoids text mode
    #[arg(long)]
    pub no_effect: bool,

    #[arg(long, help = upload::NO_OVERLAY_HELP)]
    pub no_overlay: bool,
}

/// Renders the text, then uploads it. Rendering comes first because connecting
/// already puts the probe on the bus, or in a dry run prints it.
///
/// `AA` is the references' rainbow trigger (`cli_text` in `aorus_lcd.py`,
/// `PanelController.cs`) and also the save frame, so it takes the upload's save
/// path, with its notice. The panel here showed the rainbow in a run without `AA`,
/// so `--no-effect` only keeps that frame off the bus: `docs/PROTOCOL.md` section 2.
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

    // Rendered in-process so the test never has to run the binary. Whitespace is
    // collapsed because the phrases may wrap.
    #[test]
    fn help_says_what_no_effect_does_and_what_it_does_not() {
        let help = Harness::command().render_long_help().to_string();
        let short = Harness::command().render_help().to_string();

        for help in [help, short] {
            let help = help.split_whitespace().collect::<Vec<_>>().join(" ");

            assert!(help.contains("--no-effect"), "{help}");
            assert!(help.contains("writes no NVRAM"), "{help}");
            assert!(help.contains("expect it to stop the rainbow"), "{help}");
            assert!(help.contains("use the image command"), "{help}");
            assert!(help.contains("not in the colour given here"), "{help}");
            assert!(!help.contains("Do not start the rainbow"), "{help}");
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
