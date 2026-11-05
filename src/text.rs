//! Renders text to a panel frame, centred on the 320x170 canvas, in an embedded
//! font so that the binary needs no font installed.
//!
//! The reference (`render_text_le565` in `aorus_lcd.py`) draws with Pillow, so
//! glyph pixels are not byte-identical to it. What matches is the idea: DejaVu
//! Sans Bold, `size` pixels to the em, blended in RGB, then the same conversion
//! to RGB565 as images. Unlike the reference, nothing is clipped: text wraps on
//! spaces, and what still does not fit is refused.
//!
//! Sizes are measured from the ink of the glyphs, so a layout that passes the
//! checks here cannot draw outside the canvas.

use fontdue::{Font, FontSettings, Metrics};

use crate::error::{Error, Result};
use crate::image::rgb_to_le565;
use crate::protocol::{HEIGHT, WIDTH};

/// DejaVu Sans Bold 2.37, the font the reference asks for first. Its licence is
/// `assets/DejaVuSans-Bold.LICENSE`, and is also in the font's own `name` table.
static FONT: &[u8] = include_bytes!("../assets/DejaVuSans-Bold.ttf");

/// The glyph index of `.notdef`, which a font shows for what it does not have.
const MISSING_GLYPH: u16 = 0;

/// Far past anything that fits. Glyph metrics stop being meaningful at sizes
/// near `u32::MAX`, where an oversized glyph would pass the fit checks.
const MAX_SIZE: u32 = 1024;

const CANVAS_WIDTH: i64 = WIDTH as i64;
const CANVAS_HEIGHT: i64 = HEIGHT as i64;

/// How to draw the text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Style {
    /// Pixels to the em, as in the reference's `ImageFont.truetype`.
    pub size: u32,
    /// Text colour as `[r, g, b]`.
    pub color: [u8; 3],
    /// Background colour as `[r, g, b]`.
    pub background: [u8; 3],
}

/// Renders `text` to a little-endian RGB565 frame of 108800 bytes. Lines end at
/// a newline, and a line too wide for the panel wraps on its spaces.
///
/// # Errors
///
/// [`Error::InvalidArgument`] if the text is empty, the size is out of range, the
/// font lacks a character, or the text does not fit on the panel at this size.
pub fn render(text: &str, style: &Style) -> Result<Vec<u8>> {
    rgb_to_le565(&render_rgb(text, style)?)
}

fn render_rgb(text: &str, style: &Style) -> Result<Vec<u8>> {
    if !(1..=MAX_SIZE).contains(&style.size) {
        return Err(Error::InvalidArgument(format!(
            "text size {} is out of range: use 1 to {MAX_SIZE}",
            style.size
        )));
    }
    if text.trim().is_empty() {
        return Err(Error::InvalidArgument("the text is empty".into()));
    }
    let font = Font::from_bytes(FONT, FontSettings::default()).map_err(|reason| {
        Error::InvalidArgument(format!("the embedded font does not load: {reason}"))
    })?;
    let typeface = Typeface {
        font: &font,
        size: style.size,
    };

    let mut lines = Vec::new();
    for line in text.lines() {
        typeface.check_characters(line)?;
        typeface.wrap(line, &mut lines)?;
    }
    let glyphs = typeface.layout(&lines)?;

    let mut canvas = style.background.repeat(WIDTH * HEIGHT);
    for glyph in &glyphs {
        typeface.draw(glyph, style.color, &mut canvas);
    }
    Ok(canvas)
}

/// A glyph box. `left` and `top` are inclusive, `right` and `bottom` exclusive,
/// and y grows downwards as on the panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Ink {
    left: i64,
    top: i64,
    right: i64,
    bottom: i64,
}

impl Ink {
    fn union(self, other: Self) -> Self {
        Self {
            left: self.left.min(other.left),
            top: self.top.min(other.top),
            right: self.right.max(other.right),
            bottom: self.bottom.max(other.bottom),
        }
    }

    fn shifted(self, x: i64, y: i64) -> Self {
        Self {
            left: self.left + x,
            top: self.top + y,
            right: self.right + x,
            bottom: self.bottom + y,
        }
    }

    fn width(self) -> i64 {
        self.right - self.left
    }

    fn height(self) -> i64 {
        self.bottom - self.top
    }
}

/// One glyph that leaves ink, and where it goes.
#[derive(Debug, Clone, Copy)]
struct Glyph {
    character: char,
    ink: Ink,
}

/// The font at one size.
struct Typeface<'a> {
    font: &'a Font,
    size: u32,
}

impl Typeface<'_> {
    fn px(&self) -> f32 {
        // Exact, as `MAX_SIZE` is far below 2^24.
        self.size as f32
    }

    /// A character the font lacks would show as a box, and a control character
    /// would show as nothing, so neither reaches the panel unannounced.
    fn check_characters(&self, line: &str) -> Result<()> {
        match line
            .chars()
            .find(|&c| c.is_control() || self.font.lookup_glyph_index(c) == MISSING_GLYPH)
        {
            Some(c) => Err(Error::InvalidArgument(format!(
                "the font cannot show {c:?} (U+{:04X})",
                u32::from(c)
            ))),
            None => Ok(()),
        }
    }

    /// The glyphs of one line that leave ink, with the pen starting at x = 0 and
    /// the baseline at y = 0. Pens are rounded because glyphs rasterise on whole
    /// pixels.
    fn line_glyphs(&self, line: &str) -> Vec<Glyph> {
        let px = self.px();
        let mut glyphs = Vec::new();
        let mut pen = 0.0;
        let mut previous = None;
        for character in line.chars() {
            if let Some(left) = previous {
                pen += self
                    .font
                    .horizontal_kern(left, character, px)
                    .unwrap_or(0.0);
            }
            previous = Some(character);

            let metrics = self.font.metrics(character, px);
            if metrics.width > 0 && metrics.height > 0 {
                glyphs.push(Glyph {
                    character,
                    ink: ink_at(pen.round() as i64, &metrics),
                });
            }
            pen += metrics.advance_width;
        }
        glyphs
    }

    fn line_width(&self, line: &str) -> i64 {
        bounds(&self.line_glyphs(line)).map_or(0, Ink::width)
    }

    /// Appends `line` to `lines`, broken on spaces if it is too wide. The
    /// spaces of a line that fits are left as they are.
    fn wrap(&self, line: &str, lines: &mut Vec<String>) -> Result<()> {
        if self.line_width(line) <= CANVAS_WIDTH {
            lines.push(line.to_owned());
            return Ok(());
        }

        let mut current = String::new();
        for word in line.split_whitespace() {
            let width = self.line_width(word);
            if width > CANVAS_WIDTH {
                return Err(Error::InvalidArgument(format!(
                    "{word:?} is {width} pixels wide at size {}, and the panel has {WIDTH}: \
                     use a smaller size",
                    self.size
                )));
            }
            let joined = format!("{current} {word}");
            if current.is_empty() {
                current = word.to_owned();
            } else if self.line_width(&joined) <= CANVAS_WIDTH {
                current = joined;
            } else {
                lines.push(std::mem::replace(&mut current, word.to_owned()));
            }
        }
        lines.push(current);
        Ok(())
    }

    /// Places the lines: each centred on its own, and the block centred by its
    /// ink. The reference also centres on the text's box, not on its baseline.
    fn layout(&self, lines: &[String]) -> Result<Vec<Glyph>> {
        let line_height = i64::from(self.font_line_height());
        let mut placed = Vec::new();
        for (index, line) in lines.iter().enumerate() {
            let glyphs = self.line_glyphs(line);
            if let Some(ink) = bounds(&glyphs) {
                let x = (CANVAS_WIDTH - ink.width()) / 2 - ink.left;
                let y = index as i64 * line_height;
                placed.extend(glyphs.iter().map(|glyph| Glyph {
                    ink: glyph.ink.shifted(x, y),
                    ..*glyph
                }));
            }
        }

        let block = bounds(&placed)
            .ok_or_else(|| Error::InvalidArgument("the text leaves nothing to show".into()))?;
        if block.height() > CANVAS_HEIGHT {
            return Err(Error::InvalidArgument(format!(
                "the text is {} pixels tall at size {}, and the panel has {HEIGHT}: use a smaller \
                 size or less text",
                block.height(),
                self.size
            )));
        }

        let y = (CANVAS_HEIGHT - block.height()) / 2 - block.top;
        for glyph in &mut placed {
            glyph.ink = glyph.ink.shifted(0, y);
        }
        Ok(placed)
    }

    fn font_line_height(&self) -> u32 {
        self.font
            .horizontal_line_metrics(self.px())
            .map_or(self.size, |metrics| metrics.new_line_size.round() as u32)
    }

    /// Blends the glyph over what is already there, so that pixels without
    /// coverage keep the background exactly. `layout` guarantees the box is on
    /// the canvas.
    fn draw(&self, glyph: &Glyph, color: [u8; 3], canvas: &mut [u8]) {
        let (metrics, coverage) = self.font.rasterize(glyph.character, self.px());
        let left = glyph.ink.left as usize;
        for (row, line) in coverage.chunks(metrics.width).enumerate() {
            let start = (glyph.ink.top as usize + row) * WIDTH + left;
            let pixels = canvas[start * 3..].as_chunks_mut::<3>().0;
            for (pixel, &alpha) in pixels.iter_mut().zip(line) {
                for (channel, ink) in pixel.iter_mut().zip(color) {
                    *channel = blend(*channel, ink, alpha);
                }
            }
        }
    }
}

/// Where a glyph's box lies with its pen at `pen`. `ymin` is the bottom of the
/// box, above the baseline when positive.
fn ink_at(pen: i64, metrics: &Metrics) -> Ink {
    let left = pen + i64::from(metrics.xmin);
    let bottom = -i64::from(metrics.ymin);
    Ink {
        left,
        top: bottom - metrics.height as i64,
        right: left + metrics.width as i64,
        bottom,
    }
}

/// The box around all of `glyphs`, or `None` if there are none.
fn bounds(glyphs: &[Glyph]) -> Option<Ink> {
    glyphs
        .iter()
        .map(|glyph| glyph.ink)
        .reduce(|all, ink| all.union(ink))
}

/// `under` at alpha 0 and `over` at alpha 255, both exactly, and rounded between.
fn blend(under: u8, over: u8, alpha: u8) -> u8 {
    let (under, over, alpha) = (u32::from(under), u32::from(over), u32::from(alpha));
    ((under * (255 - alpha) + over * alpha + 127) / 255) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    const GREY: [u8; 3] = [0x8B, 0x8D, 0x8B];
    const BLACK: [u8; 3] = [0, 0, 0];
    const NAVY: [u8; 3] = [0, 0, 0x40];

    fn style(size: u32) -> Style {
        Style {
            size,
            color: GREY,
            background: BLACK,
        }
    }

    fn rgb(text: &str, style: &Style) -> Vec<u8> {
        render_rgb(text, style).unwrap()
    }

    fn pixel(canvas: &[u8], x: usize, y: usize) -> [u8; 3] {
        let start = (y * WIDTH + x) * 3;
        [canvas[start], canvas[start + 1], canvas[start + 2]]
    }

    /// The box around every pixel that is not the background.
    fn ink_box(canvas: &[u8], background: [u8; 3]) -> Option<(usize, usize, usize, usize)> {
        let mut found = None;
        for y in 0..HEIGHT {
            for x in 0..WIDTH {
                if pixel(canvas, x, y) != background {
                    let (left, top, right, bottom) = found.unwrap_or((x, y, x, y));
                    found = Some((left.min(x), top.min(y), right.max(x), bottom.max(y)));
                }
            }
        }
        found
    }

    fn rows_with_ink(canvas: &[u8]) -> Vec<usize> {
        (0..HEIGHT)
            .filter(|&y| (0..WIDTH).any(|x| pixel(canvas, x, y) != BLACK))
            .collect()
    }

    /// Groups of adjacent rows, which is to say lines of text.
    fn line_count(canvas: &[u8]) -> usize {
        let rows = rows_with_ink(canvas);
        let gaps = rows.iter().zip(&rows[1..]).filter(|(a, b)| **a + 1 != **b);
        1 + gaps.count()
    }

    fn refusal(text: &str, style: &Style) -> String {
        render(text, style).unwrap_err().to_string()
    }

    #[test]
    fn a_frame_is_a_whole_panel() {
        assert_eq!(render("hello", &style(28)).unwrap().len(), 108_800);
    }

    #[test]
    fn text_lands_in_the_middle_of_the_canvas() {
        let (left, top, right, bottom) = ink_box(&rgb("hello", &style(28)), BLACK).unwrap();

        assert!(left > 0 && right < WIDTH - 1 && top > 0 && bottom < HEIGHT - 1);
        // Centred to within a pixel, as both margins are whole pixels.
        assert!(left.abs_diff(WIDTH - 1 - right) <= 1, "{left} {right}");
        assert!(top.abs_diff(HEIGHT - 1 - bottom) <= 1, "{top} {bottom}");
    }

    #[test]
    fn the_size_is_pixels_to_the_em() {
        let (_, top, _, bottom) = ink_box(&rgb("H", &style(100)), BLACK).unwrap();

        // DejaVu's capitals stand 0.729 em tall.
        assert!((72..=74).contains(&(bottom - top + 1)), "{top} {bottom}");
    }

    #[test]
    fn glyphs_are_the_right_way_up() {
        let canvas = rgb("T", &style(60));
        let (left, top, right, bottom) = ink_box(&canvas, BLACK).unwrap();
        let width = |y| {
            (left..=right)
                .filter(|&x| pixel(&canvas, x, y) == GREY)
                .count()
        };

        assert!(width(top + 2) > 3 * width(bottom - 2));
    }

    #[test]
    fn background_pixels_stay_exact() {
        let style = Style {
            background: NAVY,
            ..style(28)
        };

        let canvas = rgb("hello", &style);

        let (left, top, right, bottom) = ink_box(&canvas, NAVY).unwrap();
        for y in 0..HEIGHT {
            for x in 0..WIDTH {
                if x < left || x > right || y < top || y > bottom {
                    assert_eq!(pixel(&canvas, x, y), NAVY, "({x}, {y})");
                }
            }
        }
    }

    #[test]
    fn full_coverage_is_exactly_the_colour() {
        let canvas = rgb("I", &style(60));

        assert!((0..WIDTH * HEIGHT).any(|i| canvas[i * 3..i * 3 + 3] == GREY));
    }

    #[test]
    fn edges_blend_between_the_colours() {
        let canvas = rgb("S", &style(60));

        let between = |pixel: &[u8]| pixel != GREY && pixel != BLACK;
        assert!(canvas.as_chunks::<3>().0.iter().any(|p| between(p)));
    }

    #[test]
    fn colours_end_up_in_the_right_channels() {
        let style = Style {
            color: [0xFF, 0, 0],
            background: [0, 0, 0xFF],
            size: 60,
        };

        let frame = render("I", &style).unwrap();

        let pixels = frame.as_chunks::<2>().0;
        assert!(pixels.contains(&[0x00, 0xF8]), "pure red");
        assert_eq!(pixels[0], [0x1F, 0x00], "pure blue");
    }

    #[test]
    fn a_newline_makes_a_second_line() {
        assert_eq!(line_count(&rgb("one", &style(28))), 1);
        assert_eq!(line_count(&rgb("one\ntwo", &style(28))), 2);
    }

    #[test]
    fn a_blank_line_is_kept() {
        let tight = rows_with_ink(&rgb("HH\nHH", &style(28)));
        let spaced = rows_with_ink(&rgb("HH\n\nHH", &style(28)));

        let span = |rows: &[usize]| rows[rows.len() - 1] - rows[0];
        assert!(span(&spaced) > span(&tight));
    }

    #[test]
    fn each_line_is_centred_on_its_own() {
        let canvas = rgb("WWWW\nI", &style(28));
        let rows = rows_with_ink(&canvas);
        let last = rows[rows.len() - 1];

        let ink: Vec<usize> = (0..WIDTH)
            .filter(|&x| pixel(&canvas, x, last - 2) != BLACK)
            .collect();
        let (left, right) = (ink[0], ink[ink.len() - 1]);
        assert!(left.abs_diff(WIDTH - 1 - right) <= 1, "{left} {right}");
    }

    #[test]
    fn a_long_line_wraps_on_spaces_and_stays_on_the_canvas() {
        let canvas = rgb("the quick brown fox walks", &style(28));

        assert_eq!(line_count(&canvas), 2);
        let (left, _, right, _) = ink_box(&canvas, BLACK).unwrap();
        assert!(left > 0 && right < WIDTH - 1, "{left} {right}");
    }

    #[test]
    fn a_line_that_fits_is_not_wrapped() {
        assert_eq!(line_count(&rgb("two words", &style(28))), 1);
    }

    #[test]
    fn wrapping_keeps_every_word() {
        let font = Font::from_bytes(FONT, FontSettings::default()).unwrap();
        let typeface = Typeface {
            font: &font,
            size: 28,
        };
        let mut lines = Vec::new();

        typeface
            .wrap("the quick brown fox jumps", &mut lines)
            .unwrap();

        assert_eq!(lines, ["the quick brown fox", "jumps"]);
    }

    #[test]
    fn a_word_too_wide_is_refused_with_the_size() {
        let message = refusal(&"W".repeat(40), &style(28));

        assert!(message.contains("at size 28"), "{message}");
        assert!(message.contains("smaller size"), "{message}");
    }

    #[test]
    fn too_many_lines_are_refused_with_the_size() {
        let message = refusal("a\nb\nc\nd\ne\nf\ng\nh", &style(28));

        assert!(message.contains("at size 28"), "{message}");
        assert!(message.contains("pixels tall"), "{message}");
    }

    #[test]
    fn wrapped_text_that_gets_too_tall_is_refused() {
        let words = "word ".repeat(100);

        assert!(refusal(&words, &style(28)).contains("at size 28"));
    }

    #[test]
    fn an_enormous_size_is_refused_rather_than_rasterised() {
        assert!(refusal("x", &style(u32::MAX)).contains("text size 4294967295"));
        assert!(refusal("x", &style(MAX_SIZE + 1)).contains("out of range"));
        assert!(refusal("|", &style(MAX_SIZE)).contains("at size 1024"));
    }

    #[test]
    fn a_glyph_that_just_fits_is_kept() {
        // An em as tall as the panel, but this ink is a small part of it.
        assert!(render(".", &style(170)).is_ok());
    }

    #[test]
    fn empty_text_is_refused() {
        for text in ["", " ", "\n", " \n\n "] {
            assert!(refusal(text, &style(28)).contains("empty"), "{text:?}");
        }
    }

    #[test]
    fn size_zero_is_refused() {
        assert!(refusal("x", &style(0)).contains("size"));
    }

    #[test]
    fn a_character_the_font_lacks_is_refused_by_name() {
        let message = refusal("hello \u{4E2D}", &style(28));

        assert!(message.contains("U+4E2D"), "{message}");
    }

    #[test]
    fn control_characters_are_refused() {
        assert!(refusal("a\tb", &style(28)).contains("U+0009"));
    }

    #[test]
    fn windows_line_ends_count_as_newlines() {
        assert_eq!(line_count(&rgb("one\r\ntwo", &style(28))), 2);
    }

    #[test]
    fn non_ascii_text_renders() {
        assert!(render("Grüße ÄÖÜ €", &style(28)).is_ok());
    }

    #[test]
    fn blending_is_exact_at_both_ends() {
        assert_eq!(blend(0x12, 0xEF, 0), 0x12);
        assert_eq!(blend(0x12, 0xEF, 255), 0xEF);
        assert_eq!(blend(0, 255, 128), 128);
    }
}
