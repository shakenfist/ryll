//! A draw's clip, reduced to the part of its destination it leaves.
//!
//! spice.proto's RECTS clip is a list of rectangles whose union is the
//! area a draw may touch. The message size is the only bound on how many
//! a draw carries, and they may overlap, repeat, or reach outside the
//! destination. Applying a draw once per listed rectangle therefore costs
//! the server 16 bytes per rectangle and the client a full copy (or fill,
//! or inversion) of the destination per rectangle: one 16 MB DRAW_COPY of
//! a million full-size rectangles over a cached 1000x1000 image is about
//! 4 TB of memcpy. It is also wrong for an overlap, which DRAW_INVERS
//! would invert twice.
//!
//! spice-common's canvas (`canvas_clip_pixman()` in `canvas_base.c`)
//! instead builds a pixman region from the rectangles and intersects it
//! with the destination, so every pixel is drawn at most once. This
//! module does the same without pixman: [`clip_area`] clamps each
//! rectangle to the destination, drops empty and duplicate ones, and
//! returns their union as disjoint rectangles. The pixels a draw then
//! touches are bounded by its destination however many rectangles the
//! server sent.
//!
//! Neither spice-common nor spice-gtk caps the rectangle count, and
//! spice-server's `red_get_clip_rects()` only checks that the count
//! matches the data. [`MAX_CLIP_RECTS`] is ryll's own bound on the
//! rectangles left after deduplication and on the union's, which is
//! what keeps the union's quadratic worst case and the number of events
//! one draw emits small.

use std::collections::HashSet;

use shakenfist_spice_protocol::constants::clip_type;
use shakenfist_spice_protocol::messages::{Clip, Rect};

/// The most distinct rectangles a clip may keep once clamped to its
/// destination, and the most rectangles their union may take.
///
/// A RECTS clip is the visible part of a drawable after other drawables
/// have covered it, so a real desktop sends a handful, or tens for a
/// window under several others. 1024 leaves a wide margin above that,
/// and keeps [`clip_area`] to a few million cheap steps at worst.
pub const MAX_CLIP_RECTS: usize = 1024;

/// A rectangle as (left, top, right, bottom), right and bottom
/// exclusive: the tuple ryll's channel events carry.
pub type Ltrb = (u32, u32, u32, u32);

/// What a draw's clip leaves of its destination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipArea {
    /// No clip, so the whole destination. Also used for an unknown
    /// clip type, which spice-common treats the same way.
    Whole,
    /// The clip's union with the destination: disjoint, non-empty
    /// rectangles inside the destination, top to bottom then left to
    /// right. Never empty.
    Rects(Vec<Ltrb>),
    /// The clip and the destination do not meet, so the draw paints
    /// nothing. This includes a RECTS clip with no rectangles, which
    /// spice-common reads as an empty region.
    Nothing,
    /// The clip needs more than [`MAX_CLIP_RECTS`] rectangles, either
    /// distinct ones or in its union. `sent` is how many rectangles the
    /// server listed.
    TooComplex { sent: usize },
}

/// Reduce `clip` to the part of `dest` it leaves. See the module
/// documentation.
#[must_use]
pub fn clip_area(clip: &Clip, dest: Ltrb) -> ClipArea {
    if clip.clip_type != clip_type::RECTS {
        return ClipArea::Whole;
    }

    // A HashSet rather than sort-and-dedup, so that a clip of a million
    // rectangles is refused as soon as the distinct ones pass the limit,
    // and a million copies of one rectangle costs one entry.
    let mut distinct: HashSet<Ltrb> = HashSet::new();
    for rect in &clip.rects {
        if let Some(clamped) = clamp(rect, dest) {
            distinct.insert(clamped);
            if distinct.len() > MAX_CLIP_RECTS {
                return ClipArea::TooComplex {
                    sent: clip.rects.len(),
                };
            }
        }
    }
    if distinct.is_empty() {
        return ClipArea::Nothing;
    }

    let mut rects: Vec<Ltrb> = distinct.into_iter().collect();
    rects.sort_unstable();
    match union(&rects) {
        Some(boxes) => ClipArea::Rects(boxes),
        None => ClipArea::TooComplex {
            sent: clip.rects.len(),
        },
    }
}

/// `rect` intersected with `dest`, or `None` if they do not meet.
///
/// The clip's edges are signed on the wire, so a rectangle that starts
/// left of or above the surface still clips the part of `dest` it
/// covers.
fn clamp(rect: &Rect, dest: Ltrb) -> Option<Ltrb> {
    let (dl, dt, dr, db) = dest;
    let left = i64::from(rect.left).max(i64::from(dl));
    let top = i64::from(rect.top).max(i64::from(dt));
    let right = i64::from(rect.right).min(i64::from(dr));
    let bottom = i64::from(rect.bottom).min(i64::from(db));
    if left >= right || top >= bottom {
        return None;
    }
    // Each edge now lies between dest's own u32 edges, so fits a u32.
    Some((left as u32, top as u32, right as u32, bottom as u32))
}

/// The union of `rects` as disjoint rectangles, banded like a pixman
/// region: cut at every distinct top and bottom edge, merge each band's
/// spans, and extend the previous band's rectangles down instead of
/// starting new ones when a band's spans match it. `None` once the
/// union needs more than [`MAX_CLIP_RECTS`] rectangles.
///
/// `rects` is at most [`MAX_CLIP_RECTS`] long, so the scan of every
/// rectangle for each of the at most twice as many bands stays small.
fn union(rects: &[Ltrb]) -> Option<Vec<Ltrb>> {
    let mut edges: Vec<u32> = rects.iter().flat_map(|&(_, t, _, b)| [t, b]).collect();
    edges.sort_unstable();
    edges.dedup();

    let mut out: Vec<Ltrb> = Vec::new();
    // The previous band's merged spans, the indices in `out` of the
    // rectangles they became, and the band's bottom edge.
    let mut prev_spans: Vec<(u32, u32)> = Vec::new();
    let mut prev_boxes: Vec<usize> = Vec::new();
    let mut prev_bottom = None;

    for band in edges.windows(2) {
        let (top, bottom) = (band[0], band[1]);

        let mut spans: Vec<(u32, u32)> = rects
            .iter()
            .filter(|&&(_, t, _, b)| t <= top && b >= bottom)
            .map(|&(l, _, r, _)| (l, r))
            .collect();
        spans.sort_unstable();
        let mut merged: Vec<(u32, u32)> = Vec::with_capacity(spans.len());
        for (left, right) in spans {
            match merged.last_mut() {
                // Overlapping or touching: one span.
                Some(last) if left <= last.1 => last.1 = last.1.max(right),
                _ => merged.push((left, right)),
            }
        }

        if prev_bottom == Some(top) && merged == prev_spans {
            for &i in &prev_boxes {
                out[i].3 = bottom;
            }
        } else {
            prev_boxes.clear();
            for &(left, right) in &merged {
                prev_boxes.push(out.len());
                out.push((left, top, right, bottom));
            }
            if out.len() > MAX_CLIP_RECTS {
                return None;
            }
            prev_spans = merged;
        }
        prev_bottom = Some(bottom);
    }

    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(left: i32, top: i32, right: i32, bottom: i32) -> Rect {
        Rect {
            top,
            left,
            bottom,
            right,
        }
    }

    fn rects_clip(rects: Vec<Rect>) -> Clip {
        Clip::rects(rects)
    }

    /// Every pixel of `dest` that some rectangle of `rects` covers, by
    /// brute force.
    fn covered_by_rects(rects: &[Rect], dest: Ltrb) -> HashSet<(u32, u32)> {
        let mut pixels = HashSet::new();
        for y in dest.1..dest.3 {
            for x in dest.0..dest.2 {
                let (xi, yi) = (i64::from(x), i64::from(y));
                if rects.iter().any(|r| {
                    i64::from(r.left) <= xi
                        && xi < i64::from(r.right)
                        && i64::from(r.top) <= yi
                        && yi < i64::from(r.bottom)
                }) {
                    pixels.insert((x, y));
                }
            }
        }
        pixels
    }

    /// The pixels `boxes` cover, asserting that no two overlap.
    fn covered_by_boxes(boxes: &[Ltrb]) -> HashSet<(u32, u32)> {
        let mut pixels = HashSet::new();
        for &(l, t, r, b) in boxes {
            assert!(l < r && t < b, "empty box {:?}", (l, t, r, b));
            for y in t..b {
                for x in l..r {
                    assert!(pixels.insert((x, y)), "pixel ({}, {}) drawn twice", x, y);
                }
            }
        }
        pixels
    }

    #[test]
    fn no_clip_is_the_whole_destination() {
        assert_eq!(clip_area(&Clip::none(), (0, 0, 10, 10)), ClipArea::Whole);
    }

    #[test]
    fn unknown_clip_type_is_the_whole_destination() {
        let clip = Clip {
            clip_type: 0x7f,
            rects: Vec::new(),
        };
        assert_eq!(clip_area(&clip, (0, 0, 10, 10)), ClipArea::Whole);
    }

    #[test]
    fn rects_clip_with_no_rects_draws_nothing() {
        assert_eq!(
            clip_area(&rects_clip(Vec::new()), (0, 0, 10, 10)),
            ClipArea::Nothing
        );
    }

    #[test]
    fn rects_outside_the_destination_draw_nothing() {
        let clip = rects_clip(vec![rect(20, 20, 30, 30), rect(5, 0, 5, 10)]);
        assert_eq!(clip_area(&clip, (0, 0, 10, 10)), ClipArea::Nothing);
    }

    #[test]
    fn rect_is_clamped_to_the_destination() {
        let clip = rects_clip(vec![rect(-5, 2, 100, 8)]);
        assert_eq!(
            clip_area(&clip, (10, 0, 20, 10)),
            ClipArea::Rects(vec![(10, 2, 20, 8)])
        );
    }

    #[test]
    fn negative_edges_clip_from_the_surface_origin() {
        let clip = rects_clip(vec![rect(-10, -10, 4, 4)]);
        assert_eq!(
            clip_area(&clip, (0, 0, 10, 10)),
            ClipArea::Rects(vec![(0, 0, 4, 4)])
        );
    }

    #[test]
    fn duplicate_full_size_rects_become_one() {
        // The #476 shape: a million copies of one full-size rectangle.
        let clip = rects_clip(vec![rect(0, 0, 1000, 1000); 1_000_000]);
        assert_eq!(
            clip_area(&clip, (0, 0, 1000, 1000)),
            ClipArea::Rects(vec![(0, 0, 1000, 1000)])
        );
    }

    #[test]
    fn too_many_distinct_rects_are_refused() {
        // A million distinct one-pixel rectangles: refused once the
        // distinct count passes the limit, without reading the rest.
        let rects: Vec<Rect> = (0..1_000_000)
            .map(|i| rect(i % 1000, i / 1000, i % 1000 + 1, i / 1000 + 1))
            .collect();
        assert_eq!(
            clip_area(&rects_clip(rects), (0, 0, 1000, 1000)),
            ClipArea::TooComplex { sent: 1_000_000 }
        );
    }

    #[test]
    fn rects_at_the_limit_are_kept() {
        // One-pixel rectangles on alternate columns of one row cannot
        // merge, so the union is as long as the input.
        let rects: Vec<Rect> = (0..MAX_CLIP_RECTS as i32)
            .map(|i| rect(2 * i, 0, 2 * i + 1, 1))
            .collect();
        match clip_area(&rects_clip(rects), (0, 0, 4096, 1)) {
            ClipArea::Rects(boxes) => assert_eq!(boxes.len(), MAX_CLIP_RECTS),
            other => panic!("expected rects, got {:?}", other),
        }
    }

    #[test]
    fn a_union_over_the_limit_is_refused() {
        // 64 thin rows crossed by 64 thin columns: 128 rectangles whose
        // union is banded into far more than MAX_CLIP_RECTS pieces.
        let mut rects = Vec::new();
        for i in 0..64 {
            rects.push(rect(0, 2 * i, 128, 2 * i + 1));
            rects.push(rect(2 * i, 0, 2 * i + 1, 128));
        }
        assert!(matches!(
            clip_area(&rects_clip(rects), (0, 0, 128, 128)),
            ClipArea::TooComplex { .. }
        ));
    }

    #[test]
    fn overlapping_rects_cover_each_pixel_once() {
        let rects = vec![rect(0, 0, 6, 6), rect(3, 3, 9, 9), rect(5, 0, 7, 2)];
        let dest = (0, 0, 10, 10);
        let ClipArea::Rects(boxes) = clip_area(&rects_clip(rects.clone()), dest) else {
            panic!("expected rects");
        };
        assert_eq!(covered_by_boxes(&boxes), covered_by_rects(&rects, dest));
    }

    #[test]
    fn a_banded_region_comes_back_as_sent() {
        // What spice-server sends: a pixman region's disjoint, banded
        // rectangles. Its union must not grow it.
        let rects = vec![
            rect(0, 0, 10, 4),
            rect(0, 4, 3, 8),
            rect(7, 4, 10, 8),
            rect(0, 8, 10, 12),
        ];
        let ClipArea::Rects(boxes) = clip_area(&rects_clip(rects), (0, 0, 10, 12)) else {
            panic!("expected rects");
        };
        assert_eq!(
            boxes,
            vec![(0, 0, 10, 4), (0, 4, 3, 8), (7, 4, 10, 8), (0, 8, 10, 12)]
        );
    }

    #[test]
    fn stacked_rects_with_the_same_span_merge() {
        let rects = vec![rect(2, 0, 8, 3), rect(2, 3, 8, 5), rect(2, 5, 8, 9)];
        assert_eq!(
            clip_area(&rects_clip(rects), (0, 0, 10, 10)),
            ClipArea::Rects(vec![(2, 0, 8, 9)])
        );
    }

    #[test]
    fn union_matches_brute_force_on_pseudo_random_clips() {
        // A small xorshift, so the test is deterministic and needs no
        // dependency.
        let mut state: u32 = 0x9e37_79b9;
        let mut next = |bound: u32| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state % bound
        };
        let dest = (3, 2, 21, 17);
        for _ in 0..500 {
            let count = next(12);
            let rects: Vec<Rect> = (0..count)
                .map(|_| {
                    let l = next(26) as i32 - 2;
                    let t = next(22) as i32 - 2;
                    rect(l, t, l + next(12) as i32, t + next(12) as i32)
                })
                .collect();
            let expected = covered_by_rects(&rects, dest);
            match clip_area(&rects_clip(rects.clone()), dest) {
                ClipArea::Rects(boxes) => {
                    for &(l, t, r, b) in &boxes {
                        assert!(l >= dest.0 && t >= dest.1 && r <= dest.2 && b <= dest.3);
                    }
                    assert_eq!(covered_by_boxes(&boxes), expected, "rects {:?}", rects);
                }
                ClipArea::Nothing => assert!(expected.is_empty(), "rects {:?}", rects),
                other => panic!("unexpected {:?} for {:?}", other, rects),
            }
        }
    }
}
