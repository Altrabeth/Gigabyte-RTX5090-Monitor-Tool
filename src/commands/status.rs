//! `aorus-lcd status`: shows everything the panel reports about itself.

use aorus_lcd::bus::transport::{hex_row, Transport};
use aorus_lcd::panel::{LoopBankRead, Panel, PanelStatus};
use aorus_lcd::protocol::{
    display_query_frame, firmware_query_frame, loop_query_frame, mode_query_frame, DisplayStatus,
    Element, Frame, LoopEntry, Mode, ModeStatus, LOOP_BANK_FIRST, LOOP_BANK_LAST,
};
use aorus_lcd::session::{self, Session};
use aorus_lcd::Result;

/// What `--dry-run` prints after the frames, in place of a status.
const NO_STATUS: &str = "dry-run: read-backs are unavailable, so there is no status to show";

/// Arguments of `aorus-lcd status`.
#[derive(Debug, clap::Args)]
pub struct Args {}

/// Reads and prints the status.
///
/// The link stays the raw one. `Retrying` repeats a failed read without sending
/// its query again, so it could return stale bytes as if they were an answer.
pub fn run(_args: Args, globals: &crate::Globals) -> Result<()> {
    match session::connect(&globals.options())? {
        Session::Live(verified) => {
            let status = Panel::new(verified).status()?;
            println!("{}", report(&status)?);
        }
        Session::DryRun(mut dry_run) => {
            // `Panel::status` would stop at its first read, which in a dry run
            // can only fail, and show one query out of eight.
            for query in queries()? {
                dry_run.write(&query)?;
            }
            println!("{NO_STATUS}");
        }
    }
    Ok(())
}

/// The queries of [`Panel::status`], in its order.
fn queries() -> Result<Vec<Frame>> {
    let mut frames = vec![
        firmware_query_frame(),
        mode_query_frame(),
        display_query_frame(),
    ];
    for bank in LOOP_BANK_FIRST..=LOOP_BANK_LAST {
        frames.push(loop_query_frame(bank)?);
    }
    Ok(frames)
}

fn report(status: &PanelStatus) -> Result<String> {
    let mut lines = vec![
        format!("firmware:  {}", status.firmware),
        format!("mode:      {}", mode_line(&status.mode)),
        format!("on:        {}", if status.mode.on { "yes" } else { "no" }),
        format!("dashboard: {}", dashboard_line(&status.display)),
        "carousel banks: raw bytes, then the interval and slots".to_owned(),
    ];
    for bank in &status.loop_banks {
        lines.push(bank_line(bank)?);
    }
    Ok(lines.join("\n"))
}

fn mode_line(status: &ModeStatus) -> String {
    let raw = status.raw_mode;
    match status.mode() {
        Some(mode) => format!("{} (raw 0x{raw:02X})", mode_label(mode)),
        None => format!("unknown (raw 0x{raw:02X})"),
    }
}

fn dashboard_line(display: &DisplayStatus) -> String {
    let widgets: Vec<&str> = Element::ALL
        .into_iter()
        .filter(|element| display.elements.contains(*element))
        .map(element_name)
        .collect();
    let widgets = if widgets.is_empty() {
        "no widgets".to_owned()
    } else {
        widgets.join(", ")
    };
    format!("{widgets}, every {} s", display.interval_s)
}

/// The raw bytes come first and always print, because bytes 4 to 7 of an `F4`
/// reply are unexplained, and [`parse_loop_bank`](aorus_lcd::protocol::parse_loop_bank)
/// reads only the bytes the real panel confirmed: the interval and three slots.
fn bank_line(bank: &LoopBankRead) -> Result<String> {
    let parsed = bank.parsed()?;
    let slots: Vec<String> = parsed.entries.into_iter().map(slot_label).collect();
    Ok(format!(
        "  bank {}: {}  interval {}, slots: {}",
        bank.bank,
        hex_row(&bank.raw),
        parsed.interval,
        slots.join(", ")
    ))
}

fn slot_label(entry: LoopEntry) -> String {
    match entry {
        LoopEntry::Empty => "empty".to_owned(),
        LoopEntry::Mode(mode) => mode_label(mode),
        LoopEntry::Unknown(raw) => format!("unknown (raw 0x{raw:02X})"),
    }
}

fn mode_label(mode: Mode) -> String {
    format!("{} {}", mode.number(), mode_name(mode))
}

fn mode_name(mode: Mode) -> &'static str {
    match mode {
        Mode::Faith1 => "stat screen 1",
        Mode::Faith2 => "stat screen 2",
        Mode::Faith3 => "stat screen 3",
        Mode::Image => "image",
        Mode::Text => "text",
        Mode::Gif => "GIF",
        Mode::ChibiClock => "chibi clock",
        Mode::Carousel => "carousel",
        Mode::PresetRotation => "preset rotation",
    }
}

fn element_name(element: Element) -> &'static str {
    match element {
        Element::GpuTemp => "GPU temperature",
        Element::GpuClock => "GPU clock",
        Element::GpuUsage => "GPU usage",
        Element::FanSpeed => "fan speed",
        Element::RamClock => "RAM clock",
        Element::RamUsage => "RAM usage",
        Element::Fps => "FPS",
        Element::Tgp => "power",
    }
}

#[cfg(test)]
mod tests {
    use aorus_lcd::bus::transport::testing::Mock;
    use aorus_lcd::protocol::LOOP_READ_LEN;

    use super::*;

    const EMPTY_BANK: [u8; LOOP_READ_LEN] = [0; LOOP_READ_LEN];

    /// A panel that answers `status` with these `DE` and `DF` replies, and with
    /// `bank` for every bank.
    fn panel_answering(mode: [u8; 4], display: [u8; 4], bank: [u8; LOOP_READ_LEN]) -> Mock {
        let mock = Mock::new();
        mock.script_read([0, 0x1A, 0, 0]);
        mock.script_read(mode);
        mock.script_read(display);
        for _ in LOOP_BANK_FIRST..=LOOP_BANK_LAST {
            mock.script_read(bank);
        }
        mock
    }

    fn report_of(mode: [u8; 4], display: [u8; 4], bank: [u8; LOOP_READ_LEN]) -> String {
        let mut panel = Panel::new(panel_answering(mode, display, bank));
        report(&panel.status().unwrap()).unwrap()
    }

    fn mode_line_of(raw: u8) -> String {
        mode_line(&ModeStatus {
            raw_mode: raw,
            on: true,
        })
    }

    #[test]
    fn the_preview_matches_what_a_real_status_sends() {
        let mock = panel_answering([0; 4], [0; 4], EMPTY_BANK);

        Panel::new(mock.clone()).status().unwrap();

        assert_eq!(mock.writes(), queries().unwrap());
    }

    #[test]
    fn the_report_shows_every_field_and_each_banks_raw_bytes() {
        let text = report_of(
            [0, 0x06, 1, 0],
            [0, 0x85, 3, 0],
            [5, 1, 2, 0xEE, 0xA1, 0xA2, 0xA3, 0xA4],
        );

        let bank = |number| {
            format!(
                "  bank {number}: 05 01 02 EE A1 A2 A3 A4  interval 5, slots: \
                 0 stat screen 1, 1 stat screen 2, unknown (raw 0xEE)"
            )
        };
        let expected = [
            "firmware:  1.10".to_owned(),
            "mode:      5 GIF (raw 0x06)".to_owned(),
            "on:        yes".to_owned(),
            "dashboard: GPU temperature, GPU usage, power, every 3 s".to_owned(),
            "carousel banks: raw bytes, then the interval and slots".to_owned(),
            bank(1),
            bank(2),
            bank(3),
            bank(4),
            bank(5),
        ];
        assert_eq!(text, expected.join("\n"));
    }

    #[test]
    fn an_unknown_mode_shows_its_raw_value() {
        assert_eq!(mode_line_of(0x00), "unknown (raw 0x00)");
        assert_eq!(mode_line_of(0x09), "unknown (raw 0x09)");
        assert_eq!(mode_line_of(0x0C), "unknown (raw 0x0C)");
    }

    #[test]
    fn the_raw_mode_the_panel_rests_in_is_named() {
        assert_eq!(mode_line_of(0x0B), "8 preset rotation (raw 0x0B)");
    }

    #[test]
    fn both_read_backs_of_the_carousel_mode_name_it() {
        assert_eq!(mode_line_of(0x08), "7 carousel (raw 0x08)");
        assert_eq!(mode_line_of(0x0A), "7 carousel (raw 0x0A)");
    }

    #[test]
    fn a_panel_that_is_off_says_so() {
        let text = report_of([0, 0x01, 0, 0], [0; 4], EMPTY_BANK);

        assert!(text.contains("on:        no\n"), "{text}");
    }

    #[test]
    fn a_dashboard_without_widgets_says_so() {
        let text = report_of([0; 4], [0, 0, 7, 0], EMPTY_BANK);

        assert!(
            text.contains("dashboard: no widgets, every 7 s\n"),
            "{text}"
        );
    }

    #[test]
    fn empty_slots_are_named_and_keep_their_raw_bytes() {
        let text = report_of([0; 4], [0; 4], EMPTY_BANK);

        let last = "  bank 5: 00 00 00 00 00 00 00 00  interval 0, slots: empty, empty, empty";
        assert!(text.ends_with(last), "{text}");
    }
}
