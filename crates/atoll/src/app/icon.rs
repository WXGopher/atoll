//! The tray icon, drawn at runtime.
//!
//! A tray icon has to say two things at a glance — how many sessions Atoll is
//! watching, and whether any of them is waiting on the user — and it has to say
//! them in sixteen pixels. That rules out a static asset per state, and pulling
//! in a rasteriser to draw one filled circle and one digit would be a lot of
//! dependency for very little drawing. So: a disc with an anti-aliased edge, and
//! a hand-cut 3×5 numeral on top.

/// Every glyph the icon can need, as five rows of three bits, most significant
/// bit leftmost.
const GLYPHS: [(char, [u8; 5]); 11] = [
    ('0', [0b111, 0b101, 0b101, 0b101, 0b111]),
    ('1', [0b010, 0b110, 0b010, 0b010, 0b111]),
    ('2', [0b111, 0b001, 0b111, 0b100, 0b111]),
    ('3', [0b111, 0b001, 0b111, 0b001, 0b111]),
    ('4', [0b101, 0b101, 0b111, 0b001, 0b001]),
    ('5', [0b111, 0b100, 0b111, 0b001, 0b111]),
    ('6', [0b111, 0b100, 0b111, 0b101, 0b111]),
    ('7', [0b111, 0b001, 0b010, 0b010, 0b010]),
    ('8', [0b111, 0b101, 0b111, 0b101, 0b111]),
    ('9', [0b111, 0b101, 0b111, 0b001, 0b111]),
    ('+', [0b000, 0b010, 0b111, 0b010, 0b000]),
];

const GLYPH_WIDTH: u32 = 3;
const GLYPH_HEIGHT: u32 = 5;

type Rgba = [u8; 4];

/// Resting: a dark disc that disappears into either taskbar theme.
const IDLE_FILL: Rgba = [0x24, 0x24, 0x30, 0xff];
const IDLE_TEXT: Rgba = [0xea, 0xea, 0xf2, 0xff];
/// Waiting: Claude's orange, brightened and dimmed by the caller's pulse so the
/// icon breathes in step with the card.
const WAIT_FILL_LOW: Rgba = [0xc9, 0x66, 0x33, 0xff];
const WAIT_FILL_HIGH: Rgba = [0xf5, 0x9b, 0x60, 0xff];
/// Dark text on the orange disc: white on that fill is unreadable at 16 px.
const WAIT_TEXT: Rgba = [0x1c, 0x11, 0x06, 0xff];
const RING: Rgba = [0xff, 0xff, 0xff, 0x4d];

/// What the icon has to say this frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct IconState {
    /// How many sessions Atoll is tracking.
    pub sessions: usize,
    /// How many of them are blocked on the user.
    pub waiting: usize,
    /// 0.0 → 1.0, the phase of the breathing animation.
    pub pulse: f32,
}

/// Render `state` as `size`×`size` premultiplication-free RGBA, ready for
/// [`tray_icon::Icon::from_rgba`].
pub fn render(state: IconState, size: u32) -> Vec<u8> {
    let size = size.max(8);
    let mut pixels = vec![0u8; (size * size * 4) as usize];

    let waiting = state.waiting > 0;
    let fill = if waiting {
        lerp(WAIT_FILL_LOW, WAIT_FILL_HIGH, state.pulse.clamp(0.0, 1.0))
    } else {
        IDLE_FILL
    };
    let text = if waiting { WAIT_TEXT } else { IDLE_TEXT };

    let centre = size as f32 / 2.0;
    let radius = centre - 0.5;
    for y in 0..size {
        for x in 0..size {
            let dx = x as f32 + 0.5 - centre;
            let dy = y as f32 + 0.5 - centre;
            let distance = (dx * dx + dy * dy).sqrt();
            // One pixel of feathering at the rim is the whole anti-aliasing
            // scheme, and at this size it is enough.
            let coverage = (radius - distance + 0.5).clamp(0.0, 1.0);
            if coverage > 0.0 {
                blend(&mut pixels, size, x, y, fill, coverage);
            }
            // A faint ring keeps the disc from vanishing into a same-coloured
            // taskbar.
            let rim = 1.0 - (distance - (radius - 0.6)).abs().min(1.0);
            if rim > 0.0 {
                blend(&mut pixels, size, x, y, RING, rim * coverage);
            }
        }
    }

    draw_label(&mut pixels, size, &label(state.sessions), text);
    pixels
}

/// What to write on the disc. Two glyphs is the most that fits legibly, so
/// anything past nine becomes `9+`.
fn label(sessions: usize) -> String {
    match sessions {
        0 => String::new(),
        1..=9 => sessions.to_string(),
        _ => "9+".to_string(),
    }
}

fn draw_label(pixels: &mut [u8], size: u32, label: &str, colour: Rgba) {
    let glyphs: Vec<[u8; 5]> = label.chars().filter_map(glyph).collect();
    if glyphs.is_empty() {
        return;
    }

    // Pick the largest whole-pixel scale that still leaves the text inside the
    // disc. Whole pixels matter: a half-pixel scale turns a 3-pixel-wide stroke
    // into a grey smear.
    let columns = glyphs.len() as u32 * GLYPH_WIDTH + (glyphs.len() as u32 - 1);
    let budget = (size as f32 * 0.72) as u32;
    let scale = (budget / columns).max(1).min(budget / GLYPH_HEIGHT).max(1);

    let text_width = columns * scale;
    let text_height = GLYPH_HEIGHT * scale;
    let origin_x = (size.saturating_sub(text_width)) / 2;
    let origin_y = (size.saturating_sub(text_height)) / 2;

    for (index, rows) in glyphs.iter().enumerate() {
        let glyph_x = origin_x + index as u32 * (GLYPH_WIDTH + 1) * scale;
        for (row, bits) in rows.iter().enumerate() {
            for column in 0..GLYPH_WIDTH {
                if bits & (1 << (GLYPH_WIDTH - 1 - column)) == 0 {
                    continue;
                }
                for dy in 0..scale {
                    for dx in 0..scale {
                        let x = glyph_x + column * scale + dx;
                        let y = origin_y + row as u32 * scale + dy;
                        if x < size && y < size {
                            blend(pixels, size, x, y, colour, 1.0);
                        }
                    }
                }
            }
        }
    }
}

fn glyph(character: char) -> Option<[u8; 5]> {
    GLYPHS
        .iter()
        .find(|(name, _)| *name == character)
        .map(|(_, rows)| *rows)
}

fn lerp(from: Rgba, to: Rgba, t: f32) -> Rgba {
    let mix = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * t).round() as u8;
    [
        mix(from[0], to[0]),
        mix(from[1], to[1]),
        mix(from[2], to[2]),
        mix(from[3], to[3]),
    ]
}

/// Source-over one colour onto the buffer at `coverage` strength.
fn blend(pixels: &mut [u8], size: u32, x: u32, y: u32, colour: Rgba, coverage: f32) {
    let index = ((y * size + x) * 4) as usize;
    let Some(destination) = pixels.get_mut(index..index + 4) else {
        return;
    };
    let source_alpha = colour[3] as f32 / 255.0 * coverage.clamp(0.0, 1.0);
    if source_alpha <= 0.0 {
        return;
    }
    let destination_alpha = destination[3] as f32 / 255.0;
    let out_alpha = source_alpha + destination_alpha * (1.0 - source_alpha);
    if out_alpha <= 0.0 {
        return;
    }
    for channel in 0..3 {
        let source = colour[channel] as f32;
        let existing = destination[channel] as f32;
        let value = (source * source_alpha + existing * destination_alpha * (1.0 - source_alpha))
            / out_alpha;
        destination[channel] = value.round().clamp(0.0, 255.0) as u8;
    }
    destination[3] = (out_alpha * 255.0).round().clamp(0.0, 255.0) as u8;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(sessions: usize, waiting: usize) -> IconState {
        IconState {
            sessions,
            waiting,
            pulse: 0.0,
        }
    }

    fn pixel(buffer: &[u8], size: u32, x: u32, y: u32) -> Rgba {
        let index = ((y * size + x) * 4) as usize;
        [
            buffer[index],
            buffer[index + 1],
            buffer[index + 2],
            buffer[index + 3],
        ]
    }

    #[test]
    fn the_buffer_is_exactly_the_size_the_tray_expects() {
        for size in [16u32, 20, 24, 32] {
            assert_eq!(render(state(1, 0), size).len(), (size * size * 4) as usize);
        }
    }

    #[test]
    fn the_corners_stay_transparent_and_the_middle_does_not() {
        let size = 16;
        let buffer = render(state(0, 0), size);
        assert_eq!(
            pixel(&buffer, size, 0, 0)[3],
            0,
            "the disc must not be square"
        );
        assert_eq!(pixel(&buffer, size, size - 1, size - 1)[3], 0);
        assert_eq!(pixel(&buffer, size, size / 2, size / 2)[3], 255);
    }

    #[test]
    fn waiting_turns_the_disc_orange() {
        let size = 16;
        let idle = pixel(&render(state(2, 0), size), size, 3, 8);
        let waiting = pixel(&render(state(2, 1), size), size, 3, 8);
        assert!(
            waiting[0] > idle[0] + 60,
            "the waiting disc must be visibly warmer: {waiting:?} vs {idle:?}"
        );
    }

    #[test]
    fn the_pulse_moves_the_fill() {
        let size = 16;
        let low = pixel(
            &render(
                IconState {
                    sessions: 1,
                    waiting: 1,
                    pulse: 0.0,
                },
                size,
            ),
            size,
            3,
            8,
        );
        let high = pixel(
            &render(
                IconState {
                    sessions: 1,
                    waiting: 1,
                    pulse: 1.0,
                },
                size,
            ),
            size,
            3,
            8,
        );
        assert!(high[0] > low[0], "the pulse must brighten the disc");
    }

    #[test]
    fn a_session_count_actually_gets_drawn() {
        let size = 16;
        let blank = render(state(0, 0), size);
        let one = render(state(1, 0), size);
        assert_ne!(blank, one, "the digit has to change some pixels");
    }

    #[test]
    fn the_count_saturates_rather_than_overflowing_the_disc() {
        assert_eq!(label(0), "");
        assert_eq!(label(9), "9");
        assert_eq!(label(10), "9+");
        assert_eq!(label(400), "9+");
        // Two glyphs still have to fit inside a 16 px icon.
        let size = 16;
        let buffer = render(state(12, 0), size);
        assert_eq!(buffer.len(), (size * size * 4) as usize);
    }

    #[test]
    fn every_glyph_the_label_can_produce_is_drawable() {
        for count in [0usize, 1, 5, 9, 10, 99] {
            for character in label(count).chars() {
                assert!(glyph(character).is_some(), "no glyph for {character:?}");
            }
        }
    }
}
