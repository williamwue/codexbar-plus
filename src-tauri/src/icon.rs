//! Tray icon renderer: the little usage meter.
//!
//! Geometry ported from upstream `Sources/CodexBar/IconRenderer.swift:151-215,679-682`:
//! a 36×36 px canvas (18 pt at 2×), 30 px wide bars centred at x=3, a 12 px tall top lane
//! (session) at y=19 and an 8 px tall bottom lane (weekly) at y=5, capsule corners
//! (radius = height/2), track fill at 28 % alpha, 2 px outline at 44 % alpha, and a
//! left-to-right fill showing REMAINING quota.
//!
//! Differences from macOS: there is no template-image tinting on Windows, so the icon is
//! rendered in an explicit foreground colour chosen for the current tray theme, and the
//! canvas is scaled to the DPI-appropriate tray size.

use tiny_skia::{Color, FillRule, Paint, PathBuilder, Pixmap, Rect, Stroke, Transform};

/// Upstream's design canvas, in pixels.
const CANVAS: u32 = 36;
const BAR_WIDTH: f32 = 30.0;
const BAR_X: f32 = 3.0;
/// Lanes are expressed bottom-up like Core Graphics, then flipped when drawn.
const TOP_LANE: Lane = Lane { y: 19.0, h: 12.0 };
const BOTTOM_LANE: Lane = Lane { y: 5.0, h: 8.0 };
/// Single-lane layout (upstream `creditsRectPx`) used when only one window exists.
const SINGLE_LANE: Lane = Lane { y: 14.0, h: 16.0 };

const TRACK_FILL_ALPHA: f32 = 0.28;
const TRACK_FILL_ALPHA_STALE: f32 = 0.18;
const TRACK_STROKE_ALPHA: f32 = 0.44;
const TRACK_STROKE_ALPHA_STALE: f32 = 0.28;
const STROKE_WIDTH: f32 = 2.0;

#[derive(Debug, Clone, Copy)]
struct Lane {
    y: f32,
    h: f32,
}

/// What the meter should show.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct IconState {
    /// Remaining fraction of the session window, `0.0..=1.0`.
    pub primary_remaining: Option<f64>,
    /// Remaining fraction of the weekly window.
    pub secondary_remaining: Option<f64>,
    /// Data is old or the last fetch failed: dim everything.
    pub stale: bool,
    /// Provider status incident: draw the overlay dot.
    pub incident: bool,
    /// True when the tray background is light (dark glyphs needed).
    pub light_theme: bool,
}

impl Default for IconState {
    fn default() -> Self {
        Self {
            primary_remaining: None,
            secondary_remaining: None,
            stale: true,
            incident: false,
            light_theme: false,
        }
    }
}

/// An RGBA icon ready for `TrayIcon::set_icon`.
pub struct RenderedIcon {
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// Renders the meter at `size` px (square). 16 px at 100 % DPI, 32 px at 200 %.
pub fn render(state: IconState, size: u32) -> RenderedIcon {
    let mut pixmap = Pixmap::new(size, size).expect("non-zero icon size");
    let scale = size as f32 / CANVAS as f32;
    let transform = Transform::from_scale(scale, scale);

    let fg = if state.light_theme {
        Color::from_rgba8(0, 0, 0, 255)
    } else {
        Color::from_rgba8(255, 255, 255, 255)
    };
    let fill_alpha = if state.stale { 0.55 } else { 1.0 };
    let track_fill = if state.stale {
        TRACK_FILL_ALPHA_STALE
    } else {
        TRACK_FILL_ALPHA
    };
    let track_stroke = if state.stale {
        TRACK_STROKE_ALPHA_STALE
    } else {
        TRACK_STROKE_ALPHA
    };

    // One meaningful quota reads as one meter: reserving an empty second lane would make
    // 46 % look like 23 % of the icon (upstream comment, IconRenderer.swift:750-753).
    let lanes: Vec<(Lane, Option<f64>)> = match (state.primary_remaining, state.secondary_remaining)
    {
        (Some(p), Some(s)) => vec![(TOP_LANE, Some(p)), (BOTTOM_LANE, Some(s))],
        (Some(p), None) => vec![(SINGLE_LANE, Some(p))],
        (None, Some(s)) => vec![(SINGLE_LANE, Some(s))],
        (None, None) => vec![(TOP_LANE, None), (BOTTOM_LANE, None)],
    };

    for (lane, remaining) in lanes {
        draw_bar(
            &mut pixmap,
            transform,
            lane,
            remaining,
            fg,
            track_fill,
            track_stroke,
            fill_alpha,
        );
    }

    if state.incident {
        draw_incident_dot(&mut pixmap, transform, fg);
    }

    RenderedIcon {
        rgba: pixmap.take(),
        width: size,
        height: size,
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_bar(
    pixmap: &mut Pixmap,
    transform: Transform,
    lane: Lane,
    remaining: Option<f64>,
    fg: Color,
    track_fill_alpha: f32,
    track_stroke_alpha: f32,
    fill_alpha: f32,
) {
    // Flip from bottom-up (Core Graphics) to top-down (tiny-skia).
    let top = CANVAS as f32 - (lane.y + lane.h);
    let radius = lane.h / 2.0;

    let track = rounded_rect(BAR_X, top, BAR_WIDTH, lane.h, radius);

    let mut paint = Paint::default();
    paint.anti_alias = true;
    paint.set_color(with_alpha(fg, track_fill_alpha));
    pixmap.fill_path(&track, &paint, FillRule::Winding, transform, None);

    // Stroke an inset path so the 2 px outline stays inside the pixel bounds.
    let inset = STROKE_WIDTH / 2.0;
    let stroke_path = rounded_rect(
        BAR_X + inset,
        top + inset,
        (BAR_WIDTH - STROKE_WIDTH).max(0.0),
        (lane.h - STROKE_WIDTH).max(0.0),
        (radius - inset).max(0.0),
    );
    let mut stroke_paint = Paint::default();
    stroke_paint.anti_alias = true;
    stroke_paint.set_color(with_alpha(fg, track_stroke_alpha));
    let stroke = Stroke {
        width: STROKE_WIDTH,
        ..Stroke::default()
    };
    pixmap.stroke_path(&stroke_path, &stroke_paint, &stroke, transform, None);

    // Progress fill: a straight-edged rect clipped by the capsule, so the leading edge is
    // flat rather than following the round cap.
    let Some(remaining) = remaining else { return };
    let width = fill_width(remaining, BAR_WIDTH);
    if width <= 0.0 {
        return;
    }
    let Some(clip) = clip_mask(pixmap.width(), pixmap.height(), &track, transform) else {
        return;
    };
    let mut fill_paint = Paint::default();
    fill_paint.anti_alias = true;
    fill_paint.set_color(with_alpha(fg, fill_alpha));
    if let Some(rect) = Rect::from_xywh(BAR_X, top, width, lane.h) {
        let mut pb = PathBuilder::new();
        pb.push_rect(rect);
        if let Some(path) = pb.finish() {
            pixmap.fill_path(
                &path,
                &fill_paint,
                FillRule::Winding,
                transform,
                Some(&clip),
            );
        }
    }
}

/// A tiny dot in the top-right corner marking a provider incident
/// (upstream `IconRenderer.swift:1009-1063` draws the same affordance).
fn draw_incident_dot(pixmap: &mut Pixmap, transform: Transform, fg: Color) {
    let mut pb = PathBuilder::new();
    pb.push_circle(CANVAS as f32 - 6.0, 6.0, 5.0);
    let Some(path) = pb.finish() else { return };

    // Punch a halo so the dot stays legible over a filled bar.
    let mut clear = Paint::default();
    clear.anti_alias = true;
    clear.set_color(Color::TRANSPARENT);
    clear.blend_mode = tiny_skia::BlendMode::Clear;
    pixmap.fill_path(&path, &clear, FillRule::Winding, transform, None);

    let mut pb = PathBuilder::new();
    pb.push_circle(CANVAS as f32 - 6.0, 6.0, 3.5);
    if let Some(dot) = pb.finish() {
        let mut paint = Paint::default();
        paint.anti_alias = true;
        paint.set_color(fg);
        pixmap.fill_path(&dot, &paint, FillRule::Winding, transform, None);
    }
}

/// Rounded-rect path; degenerates to a plain rect when the radius is zero.
fn rounded_rect(x: f32, y: f32, w: f32, h: f32, radius: f32) -> tiny_skia::Path {
    let mut pb = PathBuilder::new();
    let r = radius.min(w / 2.0).min(h / 2.0).max(0.0);
    if r <= f32::EPSILON {
        if let Some(rect) = Rect::from_xywh(x, y, w, h) {
            pb.push_rect(rect);
        }
        return pb.finish().unwrap_or_else(|| empty_path());
    }

    pb.move_to(x + r, y);
    pb.line_to(x + w - r, y);
    pb.quad_to(x + w, y, x + w, y + r);
    pb.line_to(x + w, y + h - r);
    pb.quad_to(x + w, y + h, x + w - r, y + h);
    pb.line_to(x + r, y + h);
    pb.quad_to(x, y + h, x, y + h - r);
    pb.line_to(x, y + r);
    pb.quad_to(x, y, x + r, y);
    pb.close();
    pb.finish().unwrap_or_else(|| empty_path())
}

fn empty_path() -> tiny_skia::Path {
    let mut pb = PathBuilder::new();
    pb.move_to(0.0, 0.0);
    pb.close();
    pb.finish().expect("degenerate path")
}

fn clip_mask(
    width: u32,
    height: u32,
    path: &tiny_skia::Path,
    transform: Transform,
) -> Option<tiny_skia::Mask> {
    let mut mask = tiny_skia::Mask::new(width, height)?;
    mask.fill_path(path, FillRule::Winding, true, transform);
    Some(mask)
}

fn with_alpha(color: Color, alpha: f32) -> Color {
    Color::from_rgba(
        color.red(),
        color.green(),
        color.blue(),
        (color.alpha() * alpha).clamp(0.0, 1.0),
    )
    .unwrap_or(color)
}

/// Remaining quota → filled pixels. Any non-zero remainder shows at least one pixel so a
/// nearly-exhausted window never looks identical to an empty one.
fn fill_width(remaining: f64, bar_width: f32) -> f32 {
    let clamped = remaining.clamp(0.0, 1.0);
    if clamped <= 0.0 {
        return 0.0;
    }
    let raw = clamped as f32 * bar_width;
    raw.max(1.0).min(bar_width)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alpha_at(icon: &RenderedIcon, x: u32, y: u32) -> u8 {
        let idx = ((y * icon.width + x) * 4 + 3) as usize;
        icon.rgba[idx]
    }

    #[test]
    fn renders_requested_size_and_rgba_length() {
        for size in [16u32, 20, 24, 32] {
            let icon = render(IconState::default(), size);
            assert_eq!(icon.width, size);
            assert_eq!(icon.height, size);
            assert_eq!(icon.rgba.len(), (size * size * 4) as usize);
        }
    }

    #[test]
    fn full_window_paints_more_than_an_empty_one() {
        let full = render(
            IconState {
                primary_remaining: Some(1.0),
                secondary_remaining: Some(1.0),
                stale: false,
                ..IconState::default()
            },
            32,
        );
        let empty = render(
            IconState {
                primary_remaining: Some(0.0),
                secondary_remaining: Some(0.0),
                stale: false,
                ..IconState::default()
            },
            32,
        );
        let ink = |icon: &RenderedIcon| icon.rgba.chunks(4).map(|px| px[3] as u64).sum::<u64>();
        assert!(
            ink(&full) > ink(&empty),
            "a full meter must be visibly denser"
        );
    }

    #[test]
    fn fill_grows_from_the_left_edge() {
        let icon = render(
            IconState {
                primary_remaining: Some(0.25),
                secondary_remaining: Some(0.25),
                stale: false,
                ..IconState::default()
            },
            36,
        );
        // Top lane spans y=19..31 bottom-up, i.e. y=5..17 top-down; sample its middle row.
        let row = 11;
        let left = alpha_at(&icon, 6, row);
        let right = alpha_at(&icon, 28, row);
        assert!(
            left > right,
            "left edge ({left}) should be denser than right ({right})"
        );
    }

    #[test]
    fn a_sliver_of_quota_still_shows_a_pixel() {
        assert_eq!(fill_width(0.0, 30.0), 0.0);
        assert_eq!(fill_width(0.001, 30.0), 1.0);
        assert_eq!(fill_width(1.0, 30.0), 30.0);
        assert_eq!(fill_width(2.0, 30.0), 30.0, "over-100 % is clamped");
    }

    #[test]
    fn stale_icons_are_dimmer_than_fresh_ones() {
        let fresh = render(
            IconState {
                primary_remaining: Some(1.0),
                secondary_remaining: Some(1.0),
                stale: false,
                ..IconState::default()
            },
            32,
        );
        let stale = render(
            IconState {
                primary_remaining: Some(1.0),
                secondary_remaining: Some(1.0),
                stale: true,
                ..IconState::default()
            },
            32,
        );
        let ink = |icon: &RenderedIcon| icon.rgba.chunks(4).map(|px| px[3] as u64).sum::<u64>();
        assert!(ink(&stale) < ink(&fresh));
    }

    #[test]
    fn incident_overlay_adds_ink_in_the_top_right_corner() {
        let base = render(
            IconState {
                primary_remaining: Some(0.5),
                secondary_remaining: Some(0.5),
                stale: false,
                ..IconState::default()
            },
            36,
        );
        let flagged = render(
            IconState {
                primary_remaining: Some(0.5),
                secondary_remaining: Some(0.5),
                stale: false,
                incident: true,
                ..IconState::default()
            },
            36,
        );
        assert!(alpha_at(&flagged, 30, 6) > alpha_at(&base, 30, 6));
    }

    #[test]
    fn single_window_uses_the_tall_lane() {
        let one = render(
            IconState {
                primary_remaining: Some(1.0),
                secondary_remaining: None,
                stale: false,
                ..IconState::default()
            },
            36,
        );
        // The tall single lane spans y=14..30 bottom-up → y=6..22 top-down, so the row at
        // y=20 is inside it while the two-lane top lane would have ended at y=17.
        assert!(alpha_at(&one, 10, 20) > 0);
    }

    #[test]
    fn light_theme_flips_the_glyph_colour() {
        let dark = render(
            IconState {
                primary_remaining: Some(1.0),
                secondary_remaining: Some(1.0),
                stale: false,
                ..IconState::default()
            },
            32,
        );
        let light = render(
            IconState {
                primary_remaining: Some(1.0),
                secondary_remaining: Some(1.0),
                stale: false,
                light_theme: true,
                ..IconState::default()
            },
            32,
        );
        let brightest = |icon: &RenderedIcon| {
            icon.rgba
                .chunks(4)
                .filter(|px| px[3] > 200)
                .map(|px| px[0])
                .max()
                .unwrap_or(0)
        };
        assert!(brightest(&dark) > brightest(&light));
    }
}
