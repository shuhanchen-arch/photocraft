//! Alignment correction for Generative Fill's surrounding-image mode (`genai_cmds`).
//!
//! The image models redraw the whole picture they are given, and the redraw is often a few
//! pixels off: the reply's trees, shorelines and reflections no longer meet the original's at the
//! selection edge. [`estimate`] finds the translation that best lines the reply up with the
//! original on a ring just outside the selection (pixels the layer mask hides anyway, so the
//! reply's own content there is free to compare), and the reply is moved by it before it is
//! placed.
//!
//! The search compares luminance with the mean difference removed (so a slight overall
//! brightness change doesn't pull it), first over the whole range on a copy at most
//! [`COARSE_SIDE`] px on its long side, then pixel by pixel around the coarse answer at full
//! size. An offset is only used when it clearly beats leaving the reply where it is
//! ([`MIN_GAIN`]) and isn't at the edge of the range; otherwise the result is no offset.

use rayon::prelude::*;

/// Long side of the coarse search's copies (px).
const COARSE_SIDE: usize = 1024;
/// Fewest ring pixels worth comparing.
const MIN_RING: usize = 200;
/// Most ring pixels compared per offset (more are subsampled evenly).
const MAX_RING: usize = 250_000;
/// An offset must cut the zero-offset error to this fraction or less to be used.
const MIN_GAIN: f64 = 0.95;

/// The translation found, in window pixels: place the reply `dx`, `dy` further right and down.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Alignment {
    pub dx: i32,
    pub dy: i32,
    /// RMS luminance difference on the ring at the offset found and at no offset (0..1).
    pub error: f64,
    pub error_unshifted: f64,
}

/// Where to compare and how far to look: the ring from `gap` to `gap + band` px outside the
/// selection, and shifts of up to `max_shift` px each way.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Reach {
    pub gap: f32,
    pub band: f32,
    pub max_shift: f32,
}

/// Rec. 709 luminance of interleaved RGB.
pub(crate) fn luma(rgb: &[f32]) -> Vec<f32> {
    rgb.chunks_exact(3).map(|p| 0.2126 * p[0] + 0.7152 * p[1] + 0.0722 * p[2]).collect()
}

/// A single-channel `w × h` image resampled to `ow × oh`: area averages when shrinking, bilinear
/// when growing (per axis). Empty when the sizes don't match the data.
pub(crate) fn resample(src: &[f32], w: usize, h: usize, ow: usize, oh: usize) -> Vec<f32> {
    if w == 0 || h == 0 || ow == 0 || oh == 0 || src.len() != w * h {
        return Vec::new();
    }
    if (w, h) == (ow, oh) {
        return src.to_vec();
    }
    // Per output index: the source span (shrinking) or the two taps and weight (growing).
    let axis = |n: usize, on: usize| -> Vec<(usize, usize, f32)> {
        (0..on)
            .map(|o| {
                if on <= n {
                    let a = o * n / on;
                    (a, ((o + 1) * n / on).clamp(a + 1, n), -1.0)
                } else {
                    let x = ((o as f64 + 0.5) * n as f64 / on as f64 - 0.5).clamp(0.0, (n - 1) as f64);
                    let a = x.floor() as usize;
                    (a, (a + 1).min(n - 1), (x - a as f64) as f32)
                }
            })
            .collect()
    };
    let (xs, ys) = (axis(w, ow), axis(h, oh));
    let at = |x: usize, y: usize| src.get(y * w + x).copied().unwrap_or(0.0);
    // A span (shrinking: average over [a, b)) or a tap pair (growing: lerp by t) on one axis.
    let sample_x = |y: usize, &(a, b, t): &(usize, usize, f32)| -> f32 {
        if t < 0.0 { (a..b).map(|x| at(x, y)).sum::<f32>() / (b - a).max(1) as f32 } else { at(a, y) * (1.0 - t) + at(b, y) * t }
    };
    let mut out = vec![0.0f32; ow * oh];
    out.par_chunks_mut(ow).zip(ys.par_iter()).for_each(|(row, &(a, b, t))| {
        for (o, xs) in row.iter_mut().zip(&xs) {
            *o = if t < 0.0 { (a..b).map(|y| sample_x(y, xs)).sum::<f32>() / (b - a).max(1) as f32 } else { sample_x(a, xs) * (1.0 - t) + sample_x(b, xs) * t };
        }
    });
    out
}

/// The ring compared: pixels more than `gap` and at most `gap + band` px from the selection
/// (`cover > 0`), as (x, y), subsampled to at most [`MAX_RING`].
pub(crate) fn ring(cover: &[f32], w: usize, h: usize, gap: f32, band: f32) -> Vec<(usize, usize)> {
    if cover.len() != w * h {
        return Vec::new();
    }
    let inside: Vec<bool> = cover.iter().map(|c| *c > 0.0).collect();
    let d = photocraft_algo::selection::edt(&inside, w, h);
    if d.len() != w * h {
        return Vec::new();
    }
    let all: Vec<(usize, usize)> = d.iter().enumerate().filter(|(_, d)| **d > gap && **d <= gap + band).map(|(i, _)| (i % w, i / w)).collect();
    let step = all.len().div_ceil(MAX_RING).max(1);
    all.into_iter().step_by(step).collect()
}

/// RMS of `orig(p) − gen(p − (dx, dy))` over the ring with its mean removed, or `None` when
/// fewer than half the ring pixels have a source pixel.
fn error_at(orig: &[f32], gen_: &[f32], ring: &[(usize, usize)], w: usize, h: usize, (dx, dy): (i32, i32)) -> Option<f64> {
    let (mut n, mut sum, mut sq) = (0usize, 0.0f64, 0.0f64);
    for &(x, y) in ring {
        let (gx, gy) = (x as i64 - i64::from(dx), y as i64 - i64::from(dy));
        if gx < 0 || gy < 0 || gx >= w as i64 || gy >= h as i64 {
            continue;
        }
        let (Some(o), Some(g)) = (orig.get(y * w + x), gen_.get(gy as usize * w + gx as usize)) else { continue };
        let d = f64::from(o - g);
        n += 1;
        sum += d;
        sq += d * d;
    }
    if n < MIN_RING.min(ring.len()).max(1) || n * 2 < ring.len() {
        return None;
    }
    let mean = sum / n as f64;
    Some((sq / n as f64 - mean * mean).max(0.0).sqrt())
}

/// The offset within `radius` of `centre` with the lowest [`error_at`], and that error.
fn search(orig: &[f32], gen_: &[f32], ring: &[(usize, usize)], w: usize, h: usize, centre: (i32, i32), radius: i32) -> Option<((i32, i32), f64)> {
    let side = radius * 2 + 1;
    (0..side * side)
        .into_par_iter()
        .filter_map(|i| {
            let off = (centre.0 + i % side - radius, centre.1 + i / side - radius);
            error_at(orig, gen_, ring, w, h, off).map(|e| (off, e))
        })
        // Ties go to the offset closest to no offset, so flat rings don't wander.
        .min_by(|a, b| a.1.total_cmp(&b.1).then((a.0.0.abs() + a.0.1.abs()).cmp(&(b.0.0.abs() + b.0.1.abs()))))
}

/// The translation that lines the reply (`gen_`, luminance, `gw × gh`) up with the original
/// window (`orig`, luminance, `w × h`) on the ring `reach` describes outside the selection
/// coverage `cover` (`w × h`). No offset (`dx = dy = 0`) when nothing clearly better is found.
pub(crate) fn estimate(orig: &[f32], cover: &[f32], (w, h): (usize, usize), gen_: &[f32], (gw, gh): (usize, usize), reach: Reach) -> Alignment {
    let Reach { gap, band, max_shift } = reach;
    let none = Alignment::default();
    if w == 0 || h == 0 || orig.len() != w * h || cover.len() != w * h || gen_.len() != gw * gh || !max_shift.is_finite() {
        return none;
    }
    // Coarse: the whole range on small copies.
    let s = (COARSE_SIDE as f64 / w.max(h) as f64).min(1.0);
    let (cw, ch) = (((w as f64 * s).round() as usize).max(1), ((h as f64 * s).round() as usize).max(1));
    let (o_c, g_c, cov_c) = (resample(orig, w, h, cw, ch), resample(gen_, gw, gh, cw, ch), resample(cover, w, h, cw, ch));
    let ring_c = ring(&cov_c, cw, ch, gap * s as f32, (band * s as f32).max(4.0));
    if ring_c.len() < MIN_RING {
        return none;
    }
    // One step past the limit, so a shift right at the limit is still inside the range.
    let radius_c = ((f64::from(max_shift) * s).ceil() as i32).max(2) + 1;
    let Some((best_c, _)) = search(&o_c, &g_c, &ring_c, cw, ch, (0, 0), radius_c) else { return none };
    if best_c.0.abs() == radius_c || best_c.1.abs() == radius_c {
        return none; // At the edge of the range: the true offset may lie beyond it.
    }
    // Fine: one coarse step around the answer, at full size.
    let g_f = resample(gen_, gw, gh, w, h);
    let ring_f = ring(cover, w, h, gap, band);
    let centre = ((f64::from(best_c.0) / s).round() as i32, (f64::from(best_c.1) / s).round() as i32);
    let radius_f = (1.0 / s).ceil() as i32 + 1;
    let Some((best, error)) = search(orig, &g_f, &ring_f, w, h, centre, radius_f) else { return none };
    let Some(error_unshifted) = error_at(orig, &g_f, &ring_f, w, h, (0, 0)) else { return none };
    let limit = max_shift.ceil() as i32;
    if best.0.abs() > limit || best.1.abs() > limit || error > error_unshifted * MIN_GAIN {
        return Alignment { error: error_unshifted, error_unshifted, ..none };
    }
    Alignment { dx: best.0, dy: best.1, error, error_unshifted }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REACH: Reach = Reach { gap: 8.0, band: 24.0, max_shift: 16.0 };

    /// A textured test image: smooth waves plus deterministic noise, so every offset differs.
    fn texture(w: usize, h: usize) -> Vec<f32> {
        let mut seed = 12345u32;
        (0..w * h)
            .map(|i| {
                seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
                let (x, y) = ((i % w) as f32, (i / w) as f32);
                let noise = (seed >> 16) as f32 / 65536.0;
                0.5 + 0.2 * (x * 0.21).sin() * (y * 0.13).cos() + 0.15 * (x * 0.05 + y * 0.07).sin() + 0.15 * (noise - 0.5)
            })
            .collect()
    }

    /// `src` (`w × h`) moved so that `out(x, y) = src(x + sx, y + sy)`, edges repeated.
    fn shifted(src: &[f32], w: usize, h: usize, sx: i32, sy: i32) -> Vec<f32> {
        (0..w * h)
            .map(|i| {
                let x = ((i % w) as i32 + sx).clamp(0, w as i32 - 1) as usize;
                let y = ((i / w) as i32 + sy).clamp(0, h as i32 - 1) as usize;
                src[y * w + x]
            })
            .collect()
    }

    fn cover(w: usize, h: usize, x0: usize, y0: usize, x1: usize, y1: usize) -> Vec<f32> {
        (0..w * h).map(|i| if (x0..x1).contains(&(i % w)) && (y0..y1).contains(&(i / w)) { 1.0 } else { 0.0 }).collect()
    }

    #[test]
    fn finds_a_shifted_reply() {
        let (w, h) = (300, 200);
        let orig = texture(w, h);
        let cov = cover(w, h, 80, 60, 220, 140);
        for (sx, sy) in [(5, -3), (-7, 4), (0, 2), (11, 9)] {
            // The model drew everything (sx, sy) px up-left: it goes back down-right by that much.
            let reply = shifted(&orig, w, h, sx, sy);
            let a = estimate(&orig, &cov, (w, h), &reply, (w, h), REACH);
            assert_eq!((a.dx, a.dy), (sx, sy), "{a:?}");
            assert!(a.error < a.error_unshifted * 0.5);
        }
    }

    #[test]
    fn finds_the_shift_through_a_change_of_resolution_and_brightness() {
        let (w, h) = (300, 200);
        let orig = texture(w, h);
        let cov = cover(w, h, 80, 60, 220, 140);
        // The reply is twice the window's size and a little brighter.
        let reply = resample(&shifted(&orig, w, h, 6, -4), w, h, w * 2, h * 2).iter().map(|v| v + 0.05).collect::<Vec<_>>();
        let a = estimate(&orig, &cov, (w, h), &reply, (w * 2, h * 2), REACH);
        assert!((a.dx - 6).abs() <= 1 && (a.dy + 4).abs() <= 1, "{a:?}");
    }

    #[test]
    fn a_large_window_is_searched_coarse_then_fine() {
        let (w, h) = (2400, 1600);
        let orig = texture(w, h);
        let cov = cover(w, h, 900, 600, 1500, 1000);
        let reply = shifted(&orig, w, h, -9, 13);
        let a = estimate(&orig, &cov, (w, h), &reply, (w, h), Reach { gap: 12.0, band: 32.0, max_shift: 24.0 });
        assert_eq!((a.dx, a.dy), (-9, 13), "{a:?}");
    }

    #[test]
    fn an_aligned_reply_stays_put() {
        let (w, h) = (300, 200);
        let orig = texture(w, h);
        let cov = cover(w, h, 80, 60, 220, 140);
        let a = estimate(&orig, &cov, (w, h), &orig, (w, h), REACH);
        assert_eq!((a.dx, a.dy), (0, 0));
    }

    #[test]
    fn no_offset_without_evidence() {
        let (w, h) = (300, 200);
        let flat = vec![0.5f32; w * h];
        let cov = cover(w, h, 80, 60, 220, 140);
        // A flat picture: every offset is as good, so none is used.
        assert_eq!(estimate(&flat, &cov, (w, h), &flat, (w, h), REACH), Alignment::default());
        // A selection filling the window leaves no ring.
        let all = vec![1.0f32; w * h];
        let tex = texture(w, h);
        assert_eq!(estimate(&tex, &all, (w, h), &shifted(&tex, w, h, 3, 3), (w, h), REACH), Alignment::default());
        // Mismatched or empty buffers.
        assert_eq!(estimate(&tex, &cov, (w, h), &tex[..10], (w, h), REACH), Alignment::default());
        assert_eq!(estimate(&[], &[], (0, 0), &[], (0, 0), REACH), Alignment::default());
        assert_eq!(estimate(&tex, &cov, (w, h), &tex, (w, h), Reach { max_shift: f32::NAN, ..REACH }), Alignment::default());
    }

    #[test]
    fn resample_averages_down_and_interpolates_up() {
        assert_eq!(resample(&[0.0, 1.0, 0.0, 1.0], 4, 1, 2, 1), vec![0.5, 0.5]);
        assert_eq!(resample(&[0.0, 0.0, 1.0, 1.0], 4, 1, 2, 1), vec![0.0, 1.0]);
        let up = resample(&[0.0, 1.0], 2, 1, 4, 1);
        assert_eq!(up.len(), 4);
        assert!(up[0] <= up[1] && up[1] <= up[2] && up[2] <= up[3] && up[0] == 0.0 && up[3] == 1.0, "{up:?}");
        assert!(resample(&[1.0; 3], 2, 2, 4, 4).is_empty(), "wrong length");
    }

    #[test]
    fn the_ring_skips_the_feathered_edge() {
        let (w, h) = (100, 100);
        let r = ring(&cover(w, h, 40, 40, 60, 60), w, h, 5.0, 10.0);
        assert!(!r.is_empty());
        // Distance from a pixel centre to the nearest selected one (40..=59 on both axes).
        let dist = |x: usize, y: usize| {
            let d = |v: usize| if v < 40 { 40 - v } else { v.saturating_sub(59) } as f32;
            d(x).hypot(d(y))
        };
        assert!(r.iter().all(|&(x, y)| dist(x, y) > 5.0 && dist(x, y) <= 15.0), "only the band past the gap");
        assert!(r.contains(&(34, 50)) && r.contains(&(50, 74)) && !r.contains(&(36, 50)) && !r.contains(&(50, 76)));
    }
}
