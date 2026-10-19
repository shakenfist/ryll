//! Damage: the parts of a surface written since a consumer last looked.
//!
//! A consumer that re-encodes only what changed (an RDP server sending
//! bitmap updates, say) needs to know where the draws landed, not just
//! that something did. [`DisplaySurface`](super::DisplaySurface) records
//! each rectangle a draw op actually wrote, after clamping to the surface
//! and the clip list, into a [`DamageRegion`], and the consumer takes the
//! whole region in one call.
//!
//! The region is deliberately coarse. A rectangle inside one already held
//! is dropped, and two rectangles merge whenever their bounding box covers
//! no more pixels than the two do separately, so a merge never claims a
//! pixel that neither rectangle covered (beyond counting their overlap
//! twice). Past [`MAX_DAMAGE_RECTS`] the whole region collapses to its
//! bounding box. That keeps every [`DamageRegion::add`] to a scan of a
//! few dozen rectangles, and bounds the memory a region nobody takes can
//! hold. Over-reporting is always safe: a consumer resends pixels that
//! did not change. Under-reporting would leave a stale picture, so the
//! rules only ever grow the region.

/// The most rectangles a [`DamageRegion`] holds before it collapses to
/// its bounding box.
///
/// A burst of scattered small draws (a blinking cursor, a clock, a busy
/// terminal) produces many small rectangles that never merge. Past a few
/// dozen, describing them individually costs a consumer more than
/// resending their bounding box, and the consumers this serves re-diff
/// in tiles anyway.
pub const MAX_DAMAGE_RECTS: usize = 32;

/// A rectangle of surface pixels. `right` and `bottom` are exclusive,
/// matching the `(left, top, right, bottom)` tuples the draw ops take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DamageRect {
    pub left: u32,
    pub top: u32,
    pub right: u32,
    pub bottom: u32,
}

impl DamageRect {
    /// A rectangle from its edges, `right` and `bottom` exclusive.
    pub const fn new(left: u32, top: u32, right: u32, bottom: u32) -> Self {
        Self {
            left,
            top,
            right,
            bottom,
        }
    }

    /// True if the rectangle covers no pixels.
    pub fn is_empty(&self) -> bool {
        self.left >= self.right || self.top >= self.bottom
    }

    /// Width in pixels; zero if `right` is not past `left`.
    pub fn width(&self) -> u32 {
        self.right.saturating_sub(self.left)
    }

    /// Height in pixels; zero if `bottom` is not past `top`.
    pub fn height(&self) -> u32 {
        self.bottom.saturating_sub(self.top)
    }

    /// Pixel count. A `u32` side squared fits in a `u64`, so this never
    /// overflows.
    pub fn area(&self) -> u64 {
        u64::from(self.width()) * u64::from(self.height())
    }

    /// True if every pixel of `other` lies inside `self`.
    pub fn contains(&self, other: &DamageRect) -> bool {
        self.left <= other.left
            && self.top <= other.top
            && self.right >= other.right
            && self.bottom >= other.bottom
    }

    /// The smallest rectangle covering both.
    pub fn union(&self, other: &DamageRect) -> DamageRect {
        DamageRect {
            left: self.left.min(other.left),
            top: self.top.min(other.top),
            right: self.right.max(other.right),
            bottom: self.bottom.max(other.bottom),
        }
    }

    /// True if replacing the two with their bounding box adds no pixel
    /// beyond their combined area. Adjacent rectangles sharing a full
    /// edge, and heavily overlapping ones, qualify; rectangles with a gap
    /// or an L-shaped arrangement do not. The sum saturates, which still
    /// compares correctly because the bounding box's area always fits.
    fn merges_with(&self, other: &DamageRect) -> bool {
        self.union(other).area() <= self.area().saturating_add(other.area())
    }
}

/// A bounded set of damaged rectangles. See the module documentation for
/// the merge and bound rules.
///
/// No two rectangles held ever qualify to merge with each other, so
/// adding one only has to merge the newcomer (and whatever it grows into)
/// with the rest.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DamageRegion {
    rects: Vec<DamageRect>,
}

impl DamageRegion {
    /// An empty region.
    pub fn new() -> Self {
        Self::default()
    }

    /// A region covering a whole `width` x `height` surface; empty if
    /// either side is zero.
    pub fn full(width: u32, height: u32) -> Self {
        let mut region = Self::new();
        region.add(DamageRect::new(0, 0, width, height));
        region
    }

    /// Record `rect` as damaged. Empty rectangles are ignored, and a
    /// rectangle already covered by one held is dropped. Otherwise it is
    /// merged with every held rectangle it qualifies to merge with,
    /// repeatedly, since each merge grows it and may let it absorb
    /// another. If that leaves more than [`MAX_DAMAGE_RECTS`], the region
    /// becomes its bounding box.
    pub fn add(&mut self, rect: DamageRect) {
        if rect.is_empty() || self.rects.iter().any(|r| r.contains(&rect)) {
            return;
        }

        let mut grown = rect;
        while let Some(i) = self.rects.iter().position(|r| r.merges_with(&grown)) {
            grown = grown.union(&self.rects.swap_remove(i));
        }
        self.rects.push(grown);

        if self.rects.len() > MAX_DAMAGE_RECTS {
            if let Some(bbox) = self.bounding_box() {
                self.rects.clear();
                self.rects.push(bbox);
            }
        }
    }

    /// True if nothing has been damaged.
    pub fn is_empty(&self) -> bool {
        self.rects.is_empty()
    }

    /// The damaged rectangles. They may overlap; their order carries no
    /// meaning.
    pub fn rects(&self) -> &[DamageRect] {
        &self.rects
    }

    /// The smallest rectangle covering every damaged one, or `None` if
    /// the region is empty.
    pub fn bounding_box(&self) -> Option<DamageRect> {
        let (first, rest) = self.rects.split_first()?;
        Some(rest.iter().fold(*first, |acc, r| acc.union(r)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(l: u32, t: u32, r: u32, b: u32) -> DamageRect {
        DamageRect::new(l, t, r, b)
    }

    #[test]
    fn new_region_is_empty() {
        let region = DamageRegion::new();
        assert!(region.is_empty());
        assert!(region.rects().is_empty());
        assert_eq!(region.bounding_box(), None);
    }

    #[test]
    fn full_covers_the_surface() {
        let region = DamageRegion::full(640, 480);
        assert_eq!(region.rects(), &[rect(0, 0, 640, 480)]);
        assert_eq!(region.bounding_box(), Some(rect(0, 0, 640, 480)));
    }

    #[test]
    fn full_of_a_zero_sized_surface_is_empty() {
        assert!(DamageRegion::full(0, 480).is_empty());
        assert!(DamageRegion::full(640, 0).is_empty());
    }

    #[test]
    fn empty_rects_are_ignored() {
        let mut region = DamageRegion::new();
        region.add(rect(5, 5, 5, 10));
        region.add(rect(5, 5, 10, 5));
        region.add(rect(10, 10, 5, 5));
        assert!(region.is_empty());
    }

    #[test]
    fn a_contained_rect_is_dropped() {
        let mut region = DamageRegion::new();
        region.add(rect(0, 0, 10, 10));
        region.add(rect(2, 2, 5, 5));
        region.add(rect(0, 0, 10, 10));
        assert_eq!(region.rects(), &[rect(0, 0, 10, 10)]);
    }

    #[test]
    fn a_containing_rect_absorbs_the_smaller() {
        let mut region = DamageRegion::new();
        region.add(rect(2, 2, 5, 5));
        region.add(rect(0, 0, 10, 10));
        assert_eq!(region.rects(), &[rect(0, 0, 10, 10)]);
    }

    #[test]
    fn adjacent_rects_merge() {
        let mut region = DamageRegion::new();
        region.add(rect(0, 0, 4, 4));
        region.add(rect(4, 0, 8, 4));
        assert_eq!(region.rects(), &[rect(0, 0, 8, 4)]);

        region.add(rect(0, 4, 8, 6));
        assert_eq!(region.rects(), &[rect(0, 0, 8, 6)]);
    }

    #[test]
    fn overlapping_rects_merge_when_the_box_adds_nothing() {
        // 10x10 and 10x10 offset by 5 horizontally: box 15x10 = 150,
        // sum 200.
        let mut region = DamageRegion::new();
        region.add(rect(0, 0, 10, 10));
        region.add(rect(5, 0, 15, 10));
        assert_eq!(region.rects(), &[rect(0, 0, 15, 10)]);
    }

    #[test]
    fn distant_rects_do_not_merge() {
        let mut region = DamageRegion::new();
        region.add(rect(0, 0, 4, 4));
        region.add(rect(100, 100, 104, 104));
        assert_eq!(
            region.rects(),
            &[rect(0, 0, 4, 4), rect(100, 100, 104, 104)]
        );
        assert_eq!(region.bounding_box(), Some(rect(0, 0, 104, 104)));
    }

    #[test]
    fn diagonal_neighbours_do_not_merge() {
        // Touching only at a corner: the box doubles the area.
        let mut region = DamageRegion::new();
        region.add(rect(0, 0, 4, 4));
        region.add(rect(4, 4, 8, 8));
        assert_eq!(region.rects().len(), 2);
    }

    #[test]
    fn merging_repeats_until_nothing_qualifies() {
        // Two strips with a gap between them stay apart; the rect that
        // fills the gap merges with one, and what it grows into then
        // merges with the other.
        let mut region = DamageRegion::new();
        region.add(rect(0, 0, 4, 4));
        region.add(rect(8, 0, 12, 4));
        assert_eq!(region.rects().len(), 2);
        region.add(rect(4, 0, 8, 4));
        assert_eq!(region.rects(), &[rect(0, 0, 12, 4)]);
    }

    /// `count` 1x1 rects on row 0, each a pixel apart so none merge.
    fn scattered(count: u32) -> DamageRegion {
        let mut region = DamageRegion::new();
        for i in 0..count {
            region.add(rect(i * 2, 0, i * 2 + 1, 1));
        }
        region
    }

    #[test]
    fn the_bound_holds_exactly_max_rects() {
        let region = scattered(MAX_DAMAGE_RECTS as u32);
        assert_eq!(region.rects().len(), MAX_DAMAGE_RECTS);
    }

    #[test]
    fn one_past_the_bound_collapses_to_the_bounding_box() {
        let region = scattered(MAX_DAMAGE_RECTS as u32 + 1);
        let right = (MAX_DAMAGE_RECTS as u32) * 2 + 1;
        assert_eq!(region.rects(), &[rect(0, 0, right, 1)]);
    }

    #[test]
    fn areas_near_u32_max_do_not_overflow() {
        let mut region = DamageRegion::new();
        region.add(rect(0, 0, u32::MAX, u32::MAX));
        region.add(rect(1, 1, u32::MAX, u32::MAX));
        assert_eq!(region.rects(), &[rect(0, 0, u32::MAX, u32::MAX)]);

        let mut region = DamageRegion::new();
        region.add(rect(0, 0, u32::MAX - 1, u32::MAX));
        region.add(rect(u32::MAX - 1, 0, u32::MAX, u32::MAX));
        assert_eq!(region.rects(), &[rect(0, 0, u32::MAX, u32::MAX)]);
    }
}
