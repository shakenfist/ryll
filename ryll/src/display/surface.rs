/// Display surface management for egui rendering
use eframe::egui::{ColorImage, Context, TextureFilter, TextureHandle, TextureOptions};
use tracing::warn;

/// Maximum surface side length. Server-supplied or auto-computed
/// dimensions larger than this are clamped to prevent
/// `width * height * 4` from overflowing `usize` (which on 64-bit
/// would still allocate an absurd buffer even without overflow;
/// 16 384 × 16 384 × 4 = 1 GiB — already past any real display).
pub(crate) const MAX_SURFACE_DIMENSION: u32 = 16_384;

/// A display surface that holds pixel data and can be rendered by egui
pub struct DisplaySurface {
    pub id: u32,
    pub width: u32,
    pub height: u32,
    pixels: Vec<u8>, // RGBA format
    texture: Option<TextureHandle>,
    dirty: bool,
}

impl DisplaySurface {
    /// Create a new surface with the given dimensions. Server-supplied
    /// dimensions are clamped to `MAX_SURFACE_DIMENSION` on each axis
    /// so that `width * height * 4` cannot overflow `usize` via a
    /// malicious `SurfaceCreate` (or via the app-side auto-create
    /// path computing `left + width` without bounds).
    pub fn new(id: u32, width: u32, height: u32) -> Self {
        let (width, height) = if width > MAX_SURFACE_DIMENSION || height > MAX_SURFACE_DIMENSION {
            warn!(
                "surface {}: requested {}×{} exceeds {}×{} cap; clamping",
                id, width, height, MAX_SURFACE_DIMENSION, MAX_SURFACE_DIMENSION
            );
            (
                width.min(MAX_SURFACE_DIMENSION),
                height.min(MAX_SURFACE_DIMENSION),
            )
        } else {
            (width, height)
        };
        // Both axes are now ≤ 16 384, so width * height ≤ 2^28 and
        // width * height * 4 ≤ 2^30 — well inside usize on 32-bit or
        // 64-bit.
        let num_pixels = (width as usize) * (height as usize);
        // RGBA: R=0, G=0, B=0, A=255 (opaque black) for each pixel
        let pixels: Vec<u8> = (0..num_pixels)
            .flat_map(|_| [0u8, 0u8, 0u8, 255u8])
            .collect();

        DisplaySurface {
            id,
            width,
            height,
            pixels,
            texture: None,
            dirty: true,
        }
    }

    /// Blit pixel data onto the surface at the given position
    pub fn blit(&mut self, left: u32, top: u32, width: u32, height: u32, pixels: &[u8]) {
        let src_stride = (width * 4) as usize;
        let dst_stride = (self.width * 4) as usize;

        for y in 0..height {
            let src_y = y as usize;
            let dst_y = (top + y) as usize;

            if dst_y >= self.height as usize {
                break;
            }

            let src_start = src_y * src_stride;
            let dst_start = dst_y * dst_stride + (left * 4) as usize;

            let copy_width = src_stride.min(dst_stride - (left * 4) as usize);

            if src_start + copy_width <= pixels.len() && dst_start + copy_width <= self.pixels.len()
            {
                self.pixels[dst_start..dst_start + copy_width]
                    .copy_from_slice(&pixels[src_start..src_start + copy_width]);
            }
        }

        self.dirty = true;
    }

    /// Paint `colour` (RGBA) into `rect` on this surface, clipped to the
    /// surface bounds and to the union of `clip` rects (empty = no extra
    /// clipping).
    pub fn fill_rect(
        &mut self,
        left: u32,
        top: u32,
        right: u32,
        bottom: u32,
        colour: [u8; 4],
        clip: &[(u32, u32, u32, u32)],
    ) {
        let Some((l, t, r, b)) =
            Self::clip_to_bounds(self.width, self.height, left, top, right, bottom)
        else {
            return;
        };

        let mut wrote = false;
        if clip.is_empty() {
            self.fill_subrect(l, t, r, b, colour);
            wrote = true;
        } else {
            for &(cl, ct, cr, cb) in clip {
                let il = l.max(cl);
                let it = t.max(ct);
                let ir = r.min(cr);
                let ib = b.min(cb);
                if il >= ir || it >= ib {
                    continue;
                }
                self.fill_subrect(il, it, ir, ib, colour);
                wrote = true;
            }
        }

        if wrote {
            self.dirty = true;
        }
    }

    /// Copy a `(dest_right - dest_left, dest_bottom - dest_top)` rect from
    /// (src_x, src_y) on this surface to (dest_left, dest_top), correctly
    /// handling overlapping source and destination rects (memmove semantics).
    #[allow(clippy::too_many_arguments)]
    pub fn copy_bits(
        &mut self,
        src_x: u32,
        src_y: u32,
        dest_left: u32,
        dest_top: u32,
        dest_right: u32,
        dest_bottom: u32,
        clip: &[(u32, u32, u32, u32)],
    ) {
        // Reject degenerate dest rects up front.
        if dest_left >= dest_right || dest_top >= dest_bottom {
            return;
        }

        // Step 1: clip dest to surface bounds; shrink src by matching edges.
        let dl = dest_left;
        let dt = dest_top;
        let mut dr = dest_right;
        let mut db = dest_bottom;
        let sx = src_x;
        let sy = src_y;

        let w = self.width;
        let h = self.height;

        // Clip dest right/bottom.
        if dr > w {
            dr = w;
        }
        if db > h {
            db = h;
        }
        if dl >= dr || dt >= db {
            return;
        }

        // Step 2: clip src to surface bounds; shrink dest by matching edges.
        // src_left/top cannot be negative (u32); if src starts past the
        // right/bottom edge, shrink dest accordingly.
        if sx >= w || sy >= h {
            return;
        }
        let src_r = sx.saturating_add(dr - dl);
        let src_b = sy.saturating_add(db - dt);
        if src_r > w {
            let overflow = src_r - w;
            if overflow >= dr - dl {
                return;
            }
            dr -= overflow;
        }
        if src_b > h {
            let overflow = src_b - h;
            if overflow >= db - dt {
                return;
            }
            db -= overflow;
        }
        if dl >= dr || dt >= db {
            return;
        }

        // Step 3: apply clip rects (or just surface bounds if empty).
        let mut wrote = false;
        if clip.is_empty() {
            self.copy_subrect(sx, sy, dl, dt, dr, db);
            wrote = true;
        } else {
            for &(cl, ct, cr, cb) in clip {
                let il = dl.max(cl);
                let it = dt.max(ct);
                let ir = dr.min(cr);
                let ib = db.min(cb);
                if il >= ir || it >= ib {
                    continue;
                }
                // Shift src by the corresponding amount.
                let ssx = sx + (il - dl);
                let ssy = sy + (it - dt);
                self.copy_subrect(ssx, ssy, il, it, ir, ib);
                wrote = true;
            }
        }

        if wrote {
            self.dirty = true;
        }
    }

    /// Invert the RGB channels of every pixel in `rect`; leave alpha
    /// unchanged. Clipping semantics match fill_rect.
    pub fn invert_rect(
        &mut self,
        left: u32,
        top: u32,
        right: u32,
        bottom: u32,
        clip: &[(u32, u32, u32, u32)],
    ) {
        let Some((l, t, r, b)) =
            Self::clip_to_bounds(self.width, self.height, left, top, right, bottom)
        else {
            return;
        };

        let mut wrote = false;
        if clip.is_empty() {
            self.invert_subrect(l, t, r, b);
            wrote = true;
        } else {
            for &(cl, ct, cr, cb) in clip {
                let il = l.max(cl);
                let it = t.max(ct);
                let ir = r.min(cr);
                let ib = b.min(cb);
                if il >= ir || it >= ib {
                    continue;
                }
                self.invert_subrect(il, it, ir, ib);
                wrote = true;
            }
        }

        if wrote {
            self.dirty = true;
        }
    }

    /// Paint `pixels` (RGBA) into (left, top) but skip any source
    /// pixel whose RGB equals the lower 24 bits of `chroma_rgba`.
    /// The alpha byte of `chroma_rgba` is ignored (we compare R/G/B
    /// only, matching pixman_utils.c:827-829).
    pub fn blit_chroma(
        &mut self,
        left: u32,
        top: u32,
        width: u32,
        height: u32,
        pixels: &[u8],
        chroma_rgba: [u8; 4],
    ) {
        if left >= self.width || top >= self.height {
            return;
        }
        if width == 0 || height == 0 {
            return;
        }

        let clip_w = width.min(self.width - left);
        let clip_h = height.min(self.height - top);
        if clip_w == 0 || clip_h == 0 {
            return;
        }

        let src_stride = (width as usize) * 4;
        let dst_stride = (self.width as usize) * 4;
        let cr = chroma_rgba[0];
        let cg = chroma_rgba[1];
        let cb = chroma_rgba[2];

        let mut wrote = false;
        for y in 0..clip_h as usize {
            let src_row = y * src_stride;
            let dst_row = ((top as usize) + y) * dst_stride + (left as usize) * 4;
            for x in 0..clip_w as usize {
                let sp = src_row + x * 4;
                if sp + 4 > pixels.len() {
                    break;
                }
                let sr = pixels[sp];
                let sg = pixels[sp + 1];
                let sb = pixels[sp + 2];
                let sa = pixels[sp + 3];
                if sr == cr && sg == cg && sb == cb {
                    continue;
                }
                let dp = dst_row + x * 4;
                self.pixels[dp] = sr;
                self.pixels[dp + 1] = sg;
                self.pixels[dp + 2] = sb;
                self.pixels[dp + 3] = sa;
                wrote = true;
            }
        }

        if wrote {
            self.dirty = true;
        }
    }

    /// Paint `pixels` (RGBA, straight/non-premultiplied) into
    /// (left, top) via source-over alpha blending with a constant
    /// `alpha` multiplier (0-255). Per-pixel source alpha multiplies
    /// through: effective_a = src_a * alpha / 255, rounded.
    pub fn blit_alpha(
        &mut self,
        left: u32,
        top: u32,
        width: u32,
        height: u32,
        pixels: &[u8],
        alpha: u8,
    ) {
        if left >= self.width || top >= self.height {
            return;
        }
        if width == 0 || height == 0 || alpha == 0 {
            return;
        }

        let clip_w = width.min(self.width - left);
        let clip_h = height.min(self.height - top);
        if clip_w == 0 || clip_h == 0 {
            return;
        }

        let src_stride = (width as usize) * 4;
        let dst_stride = (self.width as usize) * 4;
        let alpha_u16 = alpha as u16;

        for y in 0..clip_h as usize {
            let src_row = y * src_stride;
            let dst_row = ((top as usize) + y) * dst_stride + (left as usize) * 4;
            for x in 0..clip_w as usize {
                let sp = src_row + x * 4;
                if sp + 4 > pixels.len() {
                    break;
                }
                let sr = pixels[sp] as u16;
                let sg = pixels[sp + 1] as u16;
                let sb = pixels[sp + 2] as u16;
                let sa = pixels[sp + 3] as u16;

                let effective_a = (sa * alpha_u16 + 127) / 255;
                let inv_a = 255 - effective_a;

                let dp = dst_row + x * 4;
                let dr = self.pixels[dp] as u16;
                let dg = self.pixels[dp + 1] as u16;
                let db = self.pixels[dp + 2] as u16;

                let out_r = (sr * effective_a + dr * inv_a + 127) / 255;
                let out_g = (sg * effective_a + dg * inv_a + 127) / 255;
                let out_b = (sb * effective_a + db * inv_a + 127) / 255;

                self.pixels[dp] = out_r as u8;
                self.pixels[dp + 1] = out_g as u8;
                self.pixels[dp + 2] = out_b as u8;
                self.pixels[dp + 3] = 255;
            }
        }

        self.dirty = true;
    }

    /// Intersect `(left,top,right,bottom)` with surface bounds. Returns None
    /// if the result is empty.
    fn clip_to_bounds(
        width: u32,
        height: u32,
        left: u32,
        top: u32,
        right: u32,
        bottom: u32,
    ) -> Option<(u32, u32, u32, u32)> {
        let l = left.min(width);
        let t = top.min(height);
        let r = right.min(width);
        let b = bottom.min(height);
        if l >= r || t >= b {
            None
        } else {
            Some((l, t, r, b))
        }
    }

    /// Fill a pre-clipped sub-rect with `colour`. Assumes bounds are valid.
    fn fill_subrect(&mut self, left: u32, top: u32, right: u32, bottom: u32, colour: [u8; 4]) {
        let stride = (self.width * 4) as usize;
        for y in top..bottom {
            let row_start = (y as usize) * stride;
            for x in left..right {
                let p = row_start + (x as usize) * 4;
                self.pixels[p] = colour[0];
                self.pixels[p + 1] = colour[1];
                self.pixels[p + 2] = colour[2];
                self.pixels[p + 3] = colour[3];
            }
        }
    }

    /// Invert the RGB channels of every pixel in a pre-clipped sub-rect.
    fn invert_subrect(&mut self, left: u32, top: u32, right: u32, bottom: u32) {
        let stride = (self.width * 4) as usize;
        for y in top..bottom {
            let row_start = (y as usize) * stride;
            for x in left..right {
                let p = row_start + (x as usize) * 4;
                self.pixels[p] = 255 - self.pixels[p];
                self.pixels[p + 1] = 255 - self.pixels[p + 1];
                self.pixels[p + 2] = 255 - self.pixels[p + 2];
                // alpha untouched
            }
        }
    }

    /// Copy a pre-clipped sub-rect from (sx, sy) to (dl, dt)-(dr, db).
    /// Aliasing-safe: snapshots the source first, then writes to dest.
    #[allow(clippy::too_many_arguments)]
    fn copy_subrect(&mut self, sx: u32, sy: u32, dl: u32, dt: u32, dr: u32, db: u32) {
        let stride = (self.width * 4) as usize;
        let w = (dr - dl) as usize;
        let h = (db - dt) as usize;
        let row_bytes = w * 4;

        let mut scratch: Vec<u8> = vec![0; row_bytes * h];
        for row in 0..h {
            let src_row_start = ((sy as usize) + row) * stride + (sx as usize) * 4;
            let scratch_start = row * row_bytes;
            scratch[scratch_start..scratch_start + row_bytes]
                .copy_from_slice(&self.pixels[src_row_start..src_row_start + row_bytes]);
        }
        for row in 0..h {
            let dst_row_start = ((dt as usize) + row) * stride + (dl as usize) * 4;
            let scratch_start = row * row_bytes;
            self.pixels[dst_row_start..dst_row_start + row_bytes]
                .copy_from_slice(&scratch[scratch_start..scratch_start + row_bytes]);
        }
    }

    /// Get the current texture handle, creating/updating if needed
    pub fn texture(&mut self, ctx: &Context) -> &TextureHandle {
        if self.texture.is_none() || self.dirty {
            let image = ColorImage::from_rgba_unmultiplied(
                [self.width as usize, self.height as usize],
                &self.pixels,
            );

            let options = TextureOptions {
                magnification: TextureFilter::Nearest,
                minification: TextureFilter::Linear,
                ..Default::default()
            };

            if let Some(ref mut tex) = self.texture {
                tex.set(image, options);
            } else {
                let name = format!("surface_{}", self.id);
                self.texture = Some(ctx.load_texture(name, image, options));
            }

            self.dirty = false;
        }

        self.texture.as_ref().unwrap()
    }

    /// Get the surface dimensions
    #[allow(dead_code)]
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Check if the surface needs to be redrawn
    #[allow(dead_code)]
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Get raw pixel data (for headless mode statistics)
    #[allow(dead_code)]
    pub fn pixels(&self) -> &[u8] {
        &self.pixels
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn surface_init_is_black() {
        let surface = DisplaySurface::new(0, 2, 2);
        let pixels = surface.pixels();
        for chunk in pixels.chunks(4) {
            assert_eq!(chunk, &[0, 0, 0, 255], "pixel should be opaque black");
        }
    }

    /// Helper: fetch the RGBA pixel at (x, y).
    fn pixel_at(s: &DisplaySurface, x: u32, y: u32) -> [u8; 4] {
        let stride = (s.width * 4) as usize;
        let p = (y as usize) * stride + (x as usize) * 4;
        let px = &s.pixels()[p..p + 4];
        [px[0], px[1], px[2], px[3]]
    }

    #[test]
    fn fill_rect_within_bounds() {
        let mut s = DisplaySurface::new(0, 4, 4);
        s.fill_rect(1, 1, 3, 3, [255, 0, 0, 255], &[]);
        // Interior (1..3, 1..3) is red.
        assert_eq!(pixel_at(&s, 1, 1), [255, 0, 0, 255]);
        assert_eq!(pixel_at(&s, 2, 1), [255, 0, 0, 255]);
        assert_eq!(pixel_at(&s, 1, 2), [255, 0, 0, 255]);
        assert_eq!(pixel_at(&s, 2, 2), [255, 0, 0, 255]);
        // Corners untouched.
        assert_eq!(pixel_at(&s, 0, 0), [0, 0, 0, 255]);
        assert_eq!(pixel_at(&s, 3, 3), [0, 0, 0, 255]);
        assert_eq!(pixel_at(&s, 0, 3), [0, 0, 0, 255]);
        assert_eq!(pixel_at(&s, 3, 0), [0, 0, 0, 255]);
        // Edges untouched.
        assert_eq!(pixel_at(&s, 1, 0), [0, 0, 0, 255]);
        assert_eq!(pixel_at(&s, 0, 1), [0, 0, 0, 255]);
        assert_eq!(pixel_at(&s, 3, 1), [0, 0, 0, 255]);
        assert_eq!(pixel_at(&s, 1, 3), [0, 0, 0, 255]);
        assert!(s.is_dirty());
    }

    #[test]
    fn fill_rect_clipped_to_surface() {
        let mut s = DisplaySurface::new(0, 4, 4);
        s.fill_rect(2, 2, 10, 10, [255, 0, 0, 255], &[]);
        // Only (2,2), (3,2), (2,3), (3,3) change.
        assert_eq!(pixel_at(&s, 2, 2), [255, 0, 0, 255]);
        assert_eq!(pixel_at(&s, 3, 2), [255, 0, 0, 255]);
        assert_eq!(pixel_at(&s, 2, 3), [255, 0, 0, 255]);
        assert_eq!(pixel_at(&s, 3, 3), [255, 0, 0, 255]);
        // Pixels outside the clipped rect untouched.
        assert_eq!(pixel_at(&s, 0, 0), [0, 0, 0, 255]);
        assert_eq!(pixel_at(&s, 1, 1), [0, 0, 0, 255]);
        assert_eq!(pixel_at(&s, 1, 2), [0, 0, 0, 255]);
        assert_eq!(pixel_at(&s, 2, 1), [0, 0, 0, 255]);
    }

    #[test]
    fn fill_rect_with_clip_union() {
        let mut s = DisplaySurface::new(0, 8, 8);
        s.fill_rect(0, 0, 8, 8, [255, 0, 0, 255], &[(0, 0, 2, 2), (6, 6, 8, 8)]);
        // Top-left 2x2 painted.
        for y in 0..2 {
            for x in 0..2 {
                assert_eq!(pixel_at(&s, x, y), [255, 0, 0, 255], "tl ({x},{y})");
            }
        }
        // Bottom-right 2x2 painted.
        for y in 6..8 {
            for x in 6..8 {
                assert_eq!(pixel_at(&s, x, y), [255, 0, 0, 255], "br ({x},{y})");
            }
        }
        // A few middle pixels untouched.
        assert_eq!(pixel_at(&s, 3, 3), [0, 0, 0, 255]);
        assert_eq!(pixel_at(&s, 5, 5), [0, 0, 0, 255]);
        assert_eq!(pixel_at(&s, 2, 0), [0, 0, 0, 255]);
        assert_eq!(pixel_at(&s, 0, 2), [0, 0, 0, 255]);
        assert_eq!(pixel_at(&s, 7, 5), [0, 0, 0, 255]);
        assert_eq!(pixel_at(&s, 5, 7), [0, 0, 0, 255]);
    }

    #[test]
    fn fill_rect_noop() {
        let mut s = DisplaySurface::new(0, 4, 4);
        let before: Vec<u8> = s.pixels().to_vec();
        let dirty_before = s.is_dirty();
        s.fill_rect(3, 3, 3, 3, [255, 0, 0, 255], &[]);
        assert_eq!(s.pixels(), before.as_slice());
        assert_eq!(s.is_dirty(), dirty_before);
    }

    #[test]
    fn copy_bits_non_overlapping() {
        let mut s = DisplaySurface::new(0, 4, 4);
        s.fill_rect(0, 0, 2, 2, [255, 0, 0, 255], &[]);
        // Copy the 2x2 red square from (0,0) to (2,0).
        s.copy_bits(0, 0, 2, 0, 4, 2, &[]);
        // Dest is red.
        assert_eq!(pixel_at(&s, 2, 0), [255, 0, 0, 255]);
        assert_eq!(pixel_at(&s, 3, 0), [255, 0, 0, 255]);
        assert_eq!(pixel_at(&s, 2, 1), [255, 0, 0, 255]);
        assert_eq!(pixel_at(&s, 3, 1), [255, 0, 0, 255]);
        // Source still red.
        assert_eq!(pixel_at(&s, 0, 0), [255, 0, 0, 255]);
        assert_eq!(pixel_at(&s, 1, 0), [255, 0, 0, 255]);
        assert_eq!(pixel_at(&s, 0, 1), [255, 0, 0, 255]);
        assert_eq!(pixel_at(&s, 1, 1), [255, 0, 0, 255]);
    }

    #[test]
    fn copy_bits_overlapping_down() {
        let mut s = DisplaySurface::new(0, 4, 4);
        // Unique pattern in row 0: four distinct colours.
        let r0_colours = [
            [10, 0, 0, 255],
            [20, 0, 0, 255],
            [30, 0, 0, 255],
            [40, 0, 0, 255],
        ];
        for (x, c) in r0_colours.iter().enumerate() {
            s.fill_rect(x as u32, 0, x as u32 + 1, 1, *c, &[]);
        }
        // Copy row 0 down into row 1.
        s.copy_bits(0, 0, 0, 1, 4, 2, &[]);
        for (x, c) in r0_colours.iter().enumerate() {
            assert_eq!(pixel_at(&s, x as u32, 1), *c, "col {x}");
        }
        // Row 0 still has its original pattern.
        for (x, c) in r0_colours.iter().enumerate() {
            assert_eq!(pixel_at(&s, x as u32, 0), *c, "row0 col {x}");
        }
    }

    #[test]
    fn copy_bits_overlapping_right() {
        let mut s = DisplaySurface::new(0, 4, 4);
        // Unique pattern in column 0.
        let c0_colours = [
            [0, 10, 0, 255],
            [0, 20, 0, 255],
            [0, 30, 0, 255],
            [0, 40, 0, 255],
        ];
        for (y, c) in c0_colours.iter().enumerate() {
            s.fill_rect(0, y as u32, 1, y as u32 + 1, *c, &[]);
        }
        // Copy column 0 (0,0)-(1,4) to dest (1,0)-(2,4).
        s.copy_bits(0, 0, 1, 0, 2, 4, &[]);
        for (y, c) in c0_colours.iter().enumerate() {
            assert_eq!(pixel_at(&s, 1, y as u32), *c, "row {y}");
        }
        // Column 0 still has its original pattern.
        for (y, c) in c0_colours.iter().enumerate() {
            assert_eq!(pixel_at(&s, 0, y as u32), *c, "col0 row {y}");
        }
    }

    #[test]
    fn copy_bits_source_off_surface() {
        let mut s = DisplaySurface::new(0, 4, 4);
        // Fill something identifiable at source position (3,0).
        s.fill_rect(3, 0, 4, 1, [77, 77, 77, 255], &[]);
        // src=(3,0) dest=(0,0)-(4,1) — would need 4 px wide, but only 1 in
        // bounds. After clipping: dest shrinks to (0,0)-(1,1), src=(3,0).
        s.copy_bits(3, 0, 0, 0, 4, 1, &[]);
        assert_eq!(pixel_at(&s, 0, 0), [77, 77, 77, 255]);
        // Pixels (1,0), (2,0) untouched (still black).
        assert_eq!(pixel_at(&s, 1, 0), [0, 0, 0, 255]);
        assert_eq!(pixel_at(&s, 2, 0), [0, 0, 0, 255]);
        // (3,0) still as written.
        assert_eq!(pixel_at(&s, 3, 0), [77, 77, 77, 255]);
    }

    #[test]
    fn invert_rect_basic() {
        let mut s = DisplaySurface::new(0, 2, 2);
        s.fill_rect(0, 0, 2, 2, [100, 150, 200, 255], &[]);
        s.invert_rect(0, 0, 2, 2, &[]);
        for y in 0..2 {
            for x in 0..2 {
                assert_eq!(pixel_at(&s, x, y), [155, 105, 55, 255], "({x},{y})");
            }
        }
    }

    #[test]
    fn blit_chroma_skips_matching_pixels() {
        let mut s = DisplaySurface::new(0, 2, 2);
        s.fill_rect(0, 0, 2, 2, [0, 0, 64, 255], &[]);
        let src: Vec<u8> = vec![
            255, 0, 255, 255, // (0,0) chroma
            100, 200, 50, 255, // (1,0)
            100, 200, 50, 255, // (0,1)
            100, 200, 50, 255, // (1,1)
        ];
        s.blit_chroma(0, 0, 2, 2, &src, [255, 0, 255, 255]);
        assert_eq!(pixel_at(&s, 0, 0), [0, 0, 64, 255]);
        assert_eq!(pixel_at(&s, 1, 0), [100, 200, 50, 255]);
        assert_eq!(pixel_at(&s, 0, 1), [100, 200, 50, 255]);
        assert_eq!(pixel_at(&s, 1, 1), [100, 200, 50, 255]);
    }

    #[test]
    fn blit_chroma_ignores_chroma_alpha_byte() {
        let mut s = DisplaySurface::new(0, 2, 2);
        s.fill_rect(0, 0, 2, 2, [0, 0, 64, 255], &[]);
        let src: Vec<u8> = vec![
            255, 0, 255, 255, // (0,0) chroma
            100, 200, 50, 255, // (1,0)
            100, 200, 50, 255, // (0,1)
            100, 200, 50, 255, // (1,1)
        ];
        // alpha byte 0 rather than 255 — should still match.
        s.blit_chroma(0, 0, 2, 2, &src, [255, 0, 255, 0]);
        assert_eq!(pixel_at(&s, 0, 0), [0, 0, 64, 255]);
        assert_eq!(pixel_at(&s, 1, 0), [100, 200, 50, 255]);
        assert_eq!(pixel_at(&s, 0, 1), [100, 200, 50, 255]);
        assert_eq!(pixel_at(&s, 1, 1), [100, 200, 50, 255]);
    }

    #[test]
    fn blit_chroma_clipped_to_surface() {
        let mut s = DisplaySurface::new(0, 2, 2);
        // Pre-filled opaque black via constructor.
        // 3x3 all-red source; chroma is magenta so nothing matches.
        let mut src: Vec<u8> = Vec::with_capacity(3 * 3 * 4);
        for _ in 0..9 {
            src.extend_from_slice(&[255, 0, 0, 255]);
        }
        s.blit_chroma(1, 0, 3, 3, &src, [255, 0, 255, 255]);
        // In-bounds pixels: (1,0) and (1,1).
        assert_eq!(pixel_at(&s, 1, 0), [255, 0, 0, 255]);
        assert_eq!(pixel_at(&s, 1, 1), [255, 0, 0, 255]);
        // (0,0) and (0,1) untouched.
        assert_eq!(pixel_at(&s, 0, 0), [0, 0, 0, 255]);
        assert_eq!(pixel_at(&s, 0, 1), [0, 0, 0, 255]);
    }

    #[test]
    fn blit_alpha_full_opaque() {
        let mut s = DisplaySurface::new(0, 2, 2);
        let mut src: Vec<u8> = Vec::with_capacity(2 * 2 * 4);
        for _ in 0..4 {
            src.extend_from_slice(&[255, 0, 0, 255]);
        }
        s.blit_alpha(0, 0, 2, 2, &src, 255);
        for y in 0..2 {
            for x in 0..2 {
                assert_eq!(pixel_at(&s, x, y), [255, 0, 0, 255], "({x},{y})");
            }
        }
    }

    #[test]
    fn blit_alpha_half() {
        let mut s = DisplaySurface::new(0, 2, 2);
        let mut src: Vec<u8> = Vec::with_capacity(2 * 2 * 4);
        for _ in 0..4 {
            src.extend_from_slice(&[255, 0, 0, 255]);
        }
        s.blit_alpha(0, 0, 2, 2, &src, 128);
        for y in 0..2 {
            for x in 0..2 {
                let px = pixel_at(&s, x, y);
                assert!(
                    (px[0] as i32 - 128).abs() <= 1,
                    "r out of range at ({x},{y}): {px:?}"
                );
                assert_eq!(px[1], 0);
                assert_eq!(px[2], 0);
                assert_eq!(px[3], 255);
            }
        }
    }

    #[test]
    fn blit_alpha_zero_is_noop() {
        let mut s = DisplaySurface::new(0, 2, 2);
        s.fill_rect(0, 0, 2, 2, [0, 255, 255, 255], &[]);
        let before: Vec<u8> = s.pixels().to_vec();
        let mut src: Vec<u8> = Vec::with_capacity(2 * 2 * 4);
        for _ in 0..4 {
            src.extend_from_slice(&[255, 0, 0, 255]);
        }
        s.blit_alpha(0, 0, 2, 2, &src, 0);
        assert_eq!(s.pixels(), before.as_slice());
    }

    #[test]
    fn blit_alpha_per_pixel_alpha_multiplies() {
        let mut s = DisplaySurface::new(0, 2, 2);
        let mut src: Vec<u8> = Vec::with_capacity(2 * 2 * 4);
        for _ in 0..4 {
            src.extend_from_slice(&[200, 0, 0, 128]);
        }
        s.blit_alpha(0, 0, 2, 2, &src, 128);
        for y in 0..2 {
            for x in 0..2 {
                let px = pixel_at(&s, x, y);
                assert!(
                    (px[0] as i32 - 50).abs() <= 2,
                    "r out of range at ({x},{y}): {px:?}"
                );
                assert_eq!(px[1], 0);
                assert_eq!(px[2], 0);
                assert_eq!(px[3], 255);
            }
        }
    }

    #[test]
    fn blit_alpha_clipped_to_surface() {
        let mut s = DisplaySurface::new(0, 2, 2);
        let mut src: Vec<u8> = Vec::with_capacity(3 * 3 * 4);
        for _ in 0..9 {
            src.extend_from_slice(&[255, 0, 0, 255]);
        }
        // Ensure dirty starts false by consuming it through a fresh run:
        // the constructor sets dirty=true, so just observe it stays/sets
        // true. The key assertion is no panic and in-bounds pixel written.
        s.blit_alpha(1, 0, 3, 3, &src, 255);
        assert_eq!(pixel_at(&s, 1, 0), [255, 0, 0, 255]);
        assert_eq!(pixel_at(&s, 1, 1), [255, 0, 0, 255]);
        assert_eq!(pixel_at(&s, 0, 0), [0, 0, 0, 255]);
        assert_eq!(pixel_at(&s, 0, 1), [0, 0, 0, 255]);
        assert!(s.is_dirty());
    }

    #[test]
    fn surface_new_clamps_oversized_dimensions() {
        // width * height would overflow usize; constructor must clamp
        // to MAX_SURFACE_DIMENSION rather than panic. u32::MAX on both
        // axes is the pathological case.
        let s = DisplaySurface::new(0, u32::MAX, u32::MAX);
        assert_eq!(s.width, MAX_SURFACE_DIMENSION);
        assert_eq!(s.height, MAX_SURFACE_DIMENSION);
        // Sanity: the pixel buffer has the clamped size, not u32::MAX^2.
        assert_eq!(
            s.pixels.len(),
            (MAX_SURFACE_DIMENSION as usize) * (MAX_SURFACE_DIMENSION as usize) * 4
        );
    }
}
