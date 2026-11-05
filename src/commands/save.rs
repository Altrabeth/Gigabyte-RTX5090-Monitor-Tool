//! `aorus-lcd save`: persists the panel configuration to NVRAM.

use aorus_lcd::panel::Panel;
use aorus_lcd::session::{self, Session};
use aorus_lcd::Result;

/// Printed before a real save.
const SAVING: &str = "writing the panel configuration to NVRAM";
/// Printed in place of [`SAVING`] by `--dry-run`, which writes nothing.
const WOULD_SAVE: &str = "dry-run: a real run writes the panel configuration to NVRAM";

/// Arguments of `aorus-lcd save`.
#[derive(Debug, clap::Args)]
pub struct Args {}

/// Sends `AA`, saying first that it writes NVRAM so that the fact reaches the
/// user even if the write then fails. It asks for no confirmation: running
/// `save` is the explicit request.
pub fn run(_args: Args, globals: &crate::Globals) -> Result<()> {
    let session = session::connect(&globals.options())?;
    println!("{}", notice(&session));
    Panel::new(session).save()
}

fn notice<T, W>(session: &Session<T, W>) -> &'static str {
    match session {
        Session::Live(_) => SAVING,
        Session::DryRun(_) => WOULD_SAVE,
    }
}

#[cfg(test)]
mod tests {
    use std::io::Stdout;

    use aorus_lcd::bus::transport::{testing::Mock, DryRun};

    use super::*;

    #[test]
    fn a_dry_run_does_not_claim_to_write() {
        let session: Session<Mock, Stdout> = Session::DryRun(DryRun::new(false));

        assert_eq!(notice(&session), WOULD_SAVE);
    }

    #[test]
    fn both_notices_name_nvram() {
        assert!(SAVING.contains("NVRAM") && WOULD_SAVE.contains("NVRAM"));
    }
}
