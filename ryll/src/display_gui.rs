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

use eframe::egui::{ColorImage, Context, TextureFilter, TextureHandle, TextureOptions};

use shakenfist_spice_renderer::DisplaySurface;

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
        let options = TextureOptions {
            magnification: TextureFilter::Nearest,
            minification: TextureFilter::Linear,
            ..Default::default()
        };

        match self.textures.entry(key) {
            Entry::Occupied(e) => {
                let tex = e.into_mut();
                if dirty {
                    tex.set(color_image(surface), options);
                }
                tex
            }
            Entry::Vacant(e) => {
                let name = format!("surface_{}", surface.id);
                e.insert(ctx.load_texture(name, color_image(surface), options))
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
