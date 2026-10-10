//! egui texture cache for `DisplaySurface`.
//!
//! `DisplaySurface` (in `display/surface.rs`) owns an RGBA pixel
//! buffer plus a dirty flag and is rendering-framework agnostic.
//! The GUI keeps its surfaces in a `SurfaceMirror`, the same type
//! the headless and `--web` modes use, and keeps the egui textures
//! derived from them here in a `TextureCache` beside it: one
//! `TextureHandle` per surface key, refreshed whenever the surface
//! signals a dirty bit.
//!
//! Only this file knows about egui; the renderer crate stays
//! egui-free.

use std::collections::hash_map::Entry;
use std::collections::HashMap;

use eframe::egui::{
    ColorImage, Context, TextureFilter, TextureHandle, TextureOptions, TextureWrapMode,
};

use shakenfist_spice_renderer::DisplaySurface;

/// Nearest-neighbour when scaled up so guest pixels stay sharp,
/// linear when scaled down.
const SURFACE_TEXTURE_OPTIONS: TextureOptions = TextureOptions {
    magnification: TextureFilter::Nearest,
    minification: TextureFilter::Linear,
    wrap_mode: TextureWrapMode::ClampToEdge,
    mipmap_mode: None,
};

/// Cached egui texture handles, keyed like `SurfaceMirror::surfaces`
/// by `(display_channel_id, surface_id)`.
///
/// A texture is allocated lazily on the first call to
/// [`TextureCache::texture`] for its key and refreshed in place
/// whenever the surface reports a dirty bit. Idle frames reuse the
/// existing handle without touching the GPU. Callers drop a key's
/// texture with [`TextureCache::remove`] when the surface behind it
/// is replaced or destroyed, so a new surface never inherits a
/// stale-sized texture.
#[derive(Default)]
pub struct TextureCache {
    textures: HashMap<(u8, u32), TextureHandle>,
}

impl TextureCache {
    /// Empty cache; textures are allocated on first paint.
    pub fn new() -> Self {
        Self::default()
    }

    /// Get the cached texture for `key`, allocating it on first use
    /// and refreshing it whenever `surface` reports a dirty bit.
    /// Idle frames reuse the existing handle.
    ///
    /// The dirty bit is consumed on every call, so `surface` must be
    /// the surface the mirror holds at `key`.
    pub fn texture(
        &mut self,
        ctx: &Context,
        key: (u8, u32),
        surface: &mut DisplaySurface,
    ) -> &TextureHandle {
        let dirty = surface.consume_dirty();

        match self.textures.entry(key) {
            Entry::Occupied(e) => {
                let tex = e.into_mut();
                if dirty {
                    tex.set(color_image(surface), SURFACE_TEXTURE_OPTIONS);
                }
                tex
            }
            Entry::Vacant(e) => {
                let name = format!("surface_{}_{}", key.0, key.1);
                e.insert(ctx.load_texture(name, color_image(surface), SURFACE_TEXTURE_OPTIONS))
            }
        }
    }

    /// Drop the texture for `key`, if any. The next
    /// [`TextureCache::texture`] call for it allocates afresh.
    pub fn remove(&mut self, key: (u8, u32)) {
        self.textures.remove(&key);
    }

    /// Drop every cached texture.
    pub fn clear(&mut self) {
        self.textures.clear();
    }
}

/// Copy a surface's pixels into an egui image.
fn color_image(surface: &DisplaySurface) -> ColorImage {
    ColorImage::from_rgba_unmultiplied(
        [surface.width as usize, surface.height as usize],
        surface.pixels(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: (u8, u32) = (0, 0);

    /// One texture id per upload since the last call, draining
    /// egui's pending delta (which must be cleared before it drops).
    fn uploads(ctx: &Context) -> Vec<eframe::egui::TextureId> {
        let mut delta = ctx.tex_manager().write().take_delta();
        let ids = delta
            .set
            .iter()
            .flat_map(|(id, deltas)| std::iter::repeat_n(*id, deltas.len()))
            .collect();
        delta.clear();
        ids
    }

    fn setup() -> (Context, TextureCache) {
        let ctx = Context::default();
        uploads(&ctx);
        (ctx, TextureCache::new())
    }

    #[test]
    fn first_call_allocates_at_surface_size() {
        let (ctx, mut cache) = setup();
        let mut surface = DisplaySurface::new(0, 64, 48);

        let tex = cache.texture(&ctx, KEY, &mut surface);
        assert_eq!(tex.size(), [64, 48]);
        let id = tex.id();
        assert_eq!(uploads(&ctx), vec![id]);
        assert!(!surface.is_dirty(), "texture() must consume the dirty bit");
    }

    #[test]
    fn idle_frame_does_not_upload() {
        let (ctx, mut cache) = setup();
        let mut surface = DisplaySurface::new(0, 8, 8);
        cache.texture(&ctx, KEY, &mut surface);
        uploads(&ctx);

        cache.texture(&ctx, KEY, &mut surface);
        assert!(
            uploads(&ctx).is_empty(),
            "a clean surface must not be re-uploaded"
        );
    }

    #[test]
    fn dirty_surface_refreshes_the_same_handle() {
        let (ctx, mut cache) = setup();
        let mut surface = DisplaySurface::new(0, 8, 8);
        let id = cache.texture(&ctx, KEY, &mut surface).id();
        uploads(&ctx);

        surface.blit(0, 0, 1, 1, &[255, 0, 0, 255]);
        let tex = cache.texture(&ctx, KEY, &mut surface);
        assert_eq!(tex.id(), id, "a refresh must reuse the handle");
        assert_eq!(
            uploads(&ctx),
            vec![id],
            "a dirty surface must be re-uploaded"
        );
        assert!(!surface.is_dirty());
    }

    #[test]
    fn remove_forces_a_fresh_allocation_at_the_new_size() {
        let (ctx, mut cache) = setup();
        let mut surface = DisplaySurface::new(0, 8, 8);
        let old = cache.texture(&ctx, KEY, &mut surface).id();

        // What the GUI does on DrawOutcome::Created: the mirror holds
        // a new surface at the key and the cache drops its texture.
        cache.remove(KEY);
        let mut replaced = DisplaySurface::new(0, 32, 16);
        let tex = cache.texture(&ctx, KEY, &mut replaced);
        assert_ne!(tex.id(), old);
        assert_eq!(tex.size(), [32, 16]);
    }

    #[test]
    fn clear_drops_every_texture() {
        let (ctx, mut cache) = setup();
        let mut a = DisplaySurface::new(0, 8, 8);
        let mut b = DisplaySurface::new(1, 8, 8);
        let a_id = cache.texture(&ctx, (0, 0), &mut a).id();
        let b_id = cache.texture(&ctx, (1, 1), &mut b).id();

        cache.clear();
        assert_ne!(cache.texture(&ctx, (0, 0), &mut a).id(), a_id);
        assert_ne!(cache.texture(&ctx, (1, 1), &mut b).id(), b_id);
    }

    #[test]
    fn keys_do_not_share_textures() {
        let (ctx, mut cache) = setup();
        let mut a = DisplaySurface::new(0, 8, 8);
        let mut b = DisplaySurface::new(0, 16, 16);
        let a_id = cache.texture(&ctx, (0, 0), &mut a).id();
        let tex = cache.texture(&ctx, (1, 0), &mut b);
        assert_ne!(
            tex.id(),
            a_id,
            "same surface id on another channel needs its own texture"
        );
        assert_eq!(tex.size(), [16, 16]);
    }
}
