//! `aorus-lcd probe`: shows whether the panel controller answers.

use std::time::Duration;

use aorus_lcd::bus::discovery::Adapter;
use aorus_lcd::bus::transport::hex_row;
use aorus_lcd::session::{self, Session, Status};
use aorus_lcd::Result;

/// What `--dry-run` prints after the frame, in place of a status.
const NO_STATUS: &str = "dry-run: read-backs are unavailable, so there is no status to show";

/// Arguments of `aorus-lcd probe`.
#[derive(Debug, clap::Args)]
pub struct Args {
    /// Keep retrying for up to SECS while the panel does not answer; it can need
    /// a minute after boot
    #[arg(long, value_name = "SECS", default_value_t = 0)]
    pub wait: u64,
}

/// Probes and prints the result. A failed probe is an error, so `main` exits
/// non-zero.
pub fn run(args: Args, globals: &crate::Globals) -> Result<()> {
    let options = session::Options {
        wait: Duration::from_secs(args.wait),
        ..globals.options()
    };
    match session::connect(&options)? {
        Session::Live(verified) => {
            println!("{}", report(verified.adapter(), &verified.status()));
        }
        Session::DryRun(_) => println!("{NO_STATUS}"),
    }
    Ok(())
}

fn report(adapter: &Adapter, status: &Status) -> String {
    format!(
        "adapter: {}\nbus:     {}\nstatus:  {}",
        adapter.name,
        adapter.bus,
        hex_row(status)
    )
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    #[derive(Parser)]
    struct Harness {
        #[command(flatten)]
        args: Args,
    }

    fn wait_parsed_from(arguments: &[&str]) -> Option<u64> {
        Some(Harness::try_parse_from(arguments).ok()?.args.wait)
    }

    #[test]
    fn the_report_shows_adapter_bus_and_status_in_hex() {
        let adapter = Adapter {
            bus: 4,
            name: "NVIDIA i2c adapter 1 at 1:00.0".into(),
        };

        assert_eq!(
            report(&adapter, &[0xFF, 0x0A, 0, 1, 2, 3, 4, 0xAB]),
            "adapter: NVIDIA i2c adapter 1 at 1:00.0\n\
             bus:     4\n\
             status:  FF 0A 00 01 02 03 04 AB"
        );
    }

    #[test]
    fn there_is_no_wait_by_default() {
        assert_eq!(wait_parsed_from(&["probe"]), Some(0));
    }

    #[test]
    fn wait_takes_whole_seconds() {
        assert_eq!(wait_parsed_from(&["probe", "--wait", "60"]), Some(60));
        assert_eq!(wait_parsed_from(&["probe", "--wait", "soon"]), None);
    }
}
