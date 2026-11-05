//! Parses the colours that commands take on the command line. This is a helper
//! of those commands, not a subcommand.

use aorus_lcd::{Error, Result};

/// Parses six hex digits into `[r, g, b]`. Leading `#`s are dropped, as by the
/// reference's `s.lstrip("#")`. Use it as a clap `value_parser`, so that a bad
/// colour is refused before anything connects.
///
/// # Errors
///
/// [`Error::InvalidArgument`] if what is left is not exactly six hex digits.
pub fn parse_hex_color(text: &str) -> Result<[u8; 3]> {
    let digits = text.trim_start_matches('#').as_bytes();
    let pair = |index: usize| {
        let [high, low] = [digits[index], digits[index + 1]].map(hex_digit);
        Some(high? << 4 | low?)
    };

    let parsed = if digits.len() == 6 {
        pair(0).zip(pair(2)).zip(pair(4))
    } else {
        None
    };
    let ((red, green), blue) = parsed.ok_or_else(|| {
        Error::InvalidArgument(format!(
            "invalid colour {text:?}: expected six hex digits, such as 8b8d8b"
        ))
    })?;
    Ok([red, green, blue])
}

fn hex_digit(byte: u8) -> Option<u8> {
    char::from(byte).to_digit(16).map(|digit| digit as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn six_digits_are_red_green_blue() {
        assert_eq!(parse_hex_color("8b8d8b").unwrap(), [0x8B, 0x8D, 0x8B]);
        assert_eq!(parse_hex_color("FF8800").unwrap(), [0xFF, 0x88, 0x00]);
    }

    #[test]
    fn a_hash_prefix_is_accepted() {
        assert_eq!(parse_hex_color("#102030").unwrap(), [0x10, 0x20, 0x30]);
    }

    #[test]
    fn anything_else_is_refused_by_name() {
        for text in [
            "", "#", "123", "12345", "1234567", "12345g", "+12345", " 12345", "12 456", "ééé",
        ] {
            let message = parse_hex_color(text).unwrap_err().to_string();
            assert!(message.contains("six hex digits"), "{text:?}: {message}");
        }
    }
}
