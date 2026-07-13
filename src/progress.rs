//! A small, reusable progress-bar renderer.
//!
//! Pure rendering — no timers, no state, no I/O — so any caller can drive it: a
//! timed countdown, a byte-count download, an N-of-M job counter. It renders a
//! *determinate* bar (a known fraction in `0..=1`) at 1/8-cell horizontal
//! resolution, so the bar glides smoothly even at small widths and low frame
//! rates instead of jumping a whole cell at a time.
//!
//! The detach-key hint uses it today (a bar that fills as the handoff nears); a
//! longer-running job can reuse the same calls by feeding `done / total`.

use crossterm::queue;
use crossterm::style::{Color, Print, ResetColor, SetBackgroundColor, SetForegroundColor};

// Eighth blocks that fill a cell from the left: index i is (i+1)/8 full, so
// EIGHTHS[7] ('█') is a whole cell. This is what buys sub-cell smoothness.
const EIGHTHS: [char; 8] = [
    '\u{258F}', '\u{258E}', '\u{258D}', '\u{258C}', // ▏ ▎ ▍ ▌  (1/8 .. 4/8)
    '\u{258B}', '\u{258A}', '\u{2589}', '\u{2588}', // ▋ ▊ ▉ █  (5/8 .. 8/8)
];
const FULL: char = '\u{2588}'; // █

/// How a `width`-cell bar splits at a given fraction, to 1/8-cell precision.
/// Invariant: `full + partial.is_some() as u16 + empty == width`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fill {
    /// Fully filled cells.
    pub full: u16,
    /// The partial cell's glyph, when the fill ends mid-cell.
    pub partial: Option<char>,
    /// Empty track cells after the fill.
    pub empty: u16,
}

/// Split `width` cells at `fraction` (clamped to `0..=1`; NaN treated as 0) into
/// full / partial / empty parts. Pure and allocation-free.
pub fn fill(width: u16, fraction: f64) -> Fill {
    let frac = if fraction.is_nan() {
        0.0
    } else {
        fraction.clamp(0.0, 1.0)
    };
    let eighths = (frac * width as f64 * 8.0).round() as u32;
    let full = (eighths / 8) as u16;
    let rem = (eighths % 8) as usize;
    let partial = if rem == 0 {
        None
    } else {
        Some(EIGHTHS[rem - 1])
    };
    let used = full + partial.is_some() as u16;
    Fill {
        full,
        partial,
        empty: width.saturating_sub(used),
    }
}

/// Colours for a rendered bar.
#[derive(Clone, Copy)]
pub struct Style {
    /// The filled portion.
    pub fill: Color,
    /// The empty groove.
    pub track: Color,
}

/// Queue a coloured bar into `buf` (ANSI; no cursor move, no newline, colours
/// reset at the end). The partial cell is drawn in `fill` over a `track`
/// background so its seam with the empty groove is invisible. Matches the
/// in-memory buffer convention used by the rest of the UI.
pub fn queue_into(buf: &mut Vec<u8>, width: u16, fraction: f64, style: Style) {
    let f = fill(width, fraction);
    let _ = queue!(buf, SetForegroundColor(style.fill));
    for _ in 0..f.full {
        let _ = queue!(buf, Print(FULL));
    }
    if let Some(p) = f.partial {
        // fg = fill (the inked left eighths), bg = track (the bare right eighths)
        let _ = queue!(
            buf,
            SetBackgroundColor(style.track),
            Print(p),
            SetBackgroundColor(Color::Reset)
        );
    }
    let _ = queue!(buf, SetForegroundColor(style.track));
    for _ in 0..f.empty {
        let _ = queue!(buf, Print(FULL));
    }
    let _ = queue!(buf, ResetColor);
}

/// A coloured bar as a ready-to-print ANSI string.
pub fn render(width: u16, fraction: f64, style: Style) -> String {
    let mut buf = Vec::new();
    queue_into(&mut buf, width, fraction, style);
    String::from_utf8_lossy(&buf).into_owned()
}

/// The bar as plain glyphs — full blocks, an optional partial, then `track`
/// chars — with no colour. Handy for a log line or a test assertion. Part of the
/// reusable surface (a future job runner can log progress with it), so it stays
/// even though the UI currently only renders the coloured form.
#[allow(dead_code)]
pub fn glyphs(width: u16, fraction: f64, track: char) -> String {
    let f = fill(width, fraction);
    let mut s = String::with_capacity(width as usize * 3);
    for _ in 0..f.full {
        s.push(FULL);
    }
    if let Some(p) = f.partial {
        s.push(p);
    }
    for _ in 0..f.empty {
        s.push(track);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cells(width: u16, frac: f64) -> u16 {
        let f = fill(width, frac);
        f.full + f.partial.is_some() as u16 + f.empty
    }

    #[test]
    fn endpoints() {
        assert_eq!(
            fill(10, 0.0),
            Fill {
                full: 0,
                partial: None,
                empty: 10
            }
        );
        assert_eq!(
            fill(10, 1.0),
            Fill {
                full: 10,
                partial: None,
                empty: 0
            }
        );
    }

    #[test]
    fn half_is_exact() {
        assert_eq!(
            fill(10, 0.5),
            Fill {
                full: 5,
                partial: None,
                empty: 5
            }
        );
    }

    #[test]
    fn sub_cell_partial() {
        // 0.55 * 10 * 8 = 44 eighths -> 5 full, 4/8 partial (▌), 4 empty
        assert_eq!(
            fill(10, 0.55),
            Fill {
                full: 5,
                partial: Some('\u{258C}'),
                empty: 4
            }
        );
        // 0.1 * 4 * 8 = 3.2 -> round 3 eighths -> 3/8 partial (▍) in the first cell
        assert_eq!(
            fill(4, 0.1),
            Fill {
                full: 0,
                partial: Some('\u{258D}'),
                empty: 3
            }
        );
    }

    #[test]
    fn clamps_and_nan() {
        assert_eq!(fill(8, 1.7), fill(8, 1.0));
        assert_eq!(fill(8, -0.3), fill(8, 0.0));
        assert_eq!(fill(8, f64::NAN), fill(8, 0.0));
    }

    #[test]
    fn width_invariant_holds() {
        for w in [0u16, 1, 3, 8, 20, 41] {
            let mut frac = 0.0;
            while frac <= 1.0 {
                assert_eq!(cells(w, frac), w, "w={w} frac={frac}");
                frac += 0.017;
            }
        }
    }

    #[test]
    fn glyphs_span_full_width() {
        // every glyph here is width-1, so char count == cell count
        assert_eq!(glyphs(12, 0.4, '\u{2591}').chars().count(), 12);
        assert_eq!(glyphs(12, 0.0, '\u{2591}').chars().count(), 12);
        assert_eq!(glyphs(12, 1.0, '\u{2591}').chars().count(), 12);
    }
}
