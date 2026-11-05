//! `aorus-lcd image`: shows a picture on the panel.

use std::path::PathBuf;

use aorus_lcd::image::load_file;
use aorus_lcd::protocol::{Framebuffer, UploadParams, DESCRIPTOR};
use aorus_lcd::Result;

use super::upload;

/// Arguments of `aorus-lcd image`.
#[derive(Debug, clap::Args)]
pub struct Args {
    /// Image file; it is resized to 320x170 without keeping its aspect ratio
    #[arg(value_name = "FILE")]
    pub file: PathBuf,

    /// After a successful upload, also save the panel configuration: this writes
    /// to the panel's NVRAM
    #[arg(long)]
    pub save: bool,

    #[arg(long, help = upload::NO_OVERLAY_HELP)]
    pub no_overlay: bool,
}

/// Decodes the file, then uploads it. The decode comes first because connecting
/// already puts the probe on the bus, or in a dry run prints it.
pub fn run(args: Args, globals: &crate::Globals) -> Result<()> {
    let pixels = load_file(&args.file)?;
    let payload = [&DESCRIPTOR[..], &pixels].concat();
    upload::send(
        &payload,
        &UploadParams::still(Framebuffer::Static),
        upload::Finish {
            clear_overlay: args.no_overlay,
            persist: args.save,
        },
        globals,
    )
}

#[cfg(test)]
mod tests {
    use clap::{CommandFactory, Parser};

    use super::*;

    #[derive(Parser)]
    struct Harness {
        #[command(flatten)]
        args: Args,
    }

    #[test]
    fn saving_has_to_be_asked_for() {
        let args = Harness::try_parse_from(["image", "a.png"]).unwrap().args;
        assert!(!args.save);

        let args = Harness::try_parse_from(["image", "a.png", "--save"])
            .unwrap()
            .args;
        assert!(args.save);
    }

    #[test]
    fn clearing_the_overlay_has_to_be_asked_for() {
        let args = Harness::try_parse_from(["image", "a.png"]).unwrap().args;
        assert!(!args.no_overlay);

        let args = Harness::try_parse_from(["image", "a.png", "--no-overlay"])
            .unwrap()
            .args;
        assert!(args.no_overlay);
    }

    // Rendered in-process so the test never has to run the binary.
    #[test]
    fn help_says_that_no_overlay_sends_e1() {
        let help = Harness::command().render_help().to_string();

        assert!(help.contains("--no-overlay"), "{help}");
        assert!(help.contains("E1"), "{help}");
    }

    // Rendered in-process so the test never has to run the binary.
    #[test]
    fn help_says_that_save_writes_nvram_after_the_upload() {
        let help = Harness::command().render_help().to_string();

        assert!(help.contains("After a successful upload"), "{help}");
        assert!(help.contains("NVRAM"), "{help}");
    }

    #[test]
    fn a_file_is_required() {
        assert!(Harness::try_parse_from(["image"]).is_err());
    }
}
