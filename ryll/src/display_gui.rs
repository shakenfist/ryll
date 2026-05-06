//! egui texture cache for `DisplaySurface`.
//!
//! `DisplaySurface` (in `display/surface.rs`) owns an RGBA pixel
//! buffer plus a dirty flag and is rendering-framework agnostic.
//! `GuiSurface` is the egui-flavoured wrapper used by the GUI mode:
//! it caches a `TextureHandle` derived from the surface's pixels
//! and refreshes it whenever the surface signals a dirty bit.
//!
//! Future frontends (H.264 encoder for `--web` mode, etc.) live
//! alongside this file as their own wrappers; only this one knows
//! about egui.

use eframe::egui::{ColorImage, Context, TextureFilter, TextureHandle, TextureOptions};

use shakenfist_spice_renderer::DisplaySurface;

/// `DisplaySurface` plus a cached egui texture handle.
///
/// The texture is allocated lazily on the first call to
/// [`GuiSurface::texture`] and refreshed in place whenever the
/// inner surface reports a dirty bit. Idle frames reuse the
/// existing handle without touching the GPU.
pub struct GuiSurface {
    inner: DisplaySurface,
    texture: Option<TextureHandle>,
}

impl GuiSurface {
    /// Create a new `GuiSurface` wrapping a fresh `DisplaySurface`
    /// of the given dimensions. The texture handle is allocated on
    /// the first paint.
    pub fn new(id: u32, width: u32, height: u32) -> Self {
        GuiSurface {
            inner: DisplaySurface::new(id, width, height),
            texture: None,
        }
    }

    /// Borrow the inner pixel substrate.
    pub fn surface(&self) -> &DisplaySurface {
        &self.inner
    }

    /// Borrow the inner pixel substrate mutably (for SPICE draw
    /// ops that need `&mut DisplaySurface`).
    pub fn surface_mut(&mut self) -> &mut DisplaySurface {
        &mut self.inner
    }

    /// Get the cached texture handle, allocating on first use and
    /// refreshing it whenever the inner surface reports a dirty
    /// bit. Idle frames reuse the existing handle.
    pub fn texture(&mut self, ctx: &Context) -> &TextureHandle {
        let dirty = self.inner.consume_dirty();
        if self.texture.is_none() || dirty {
            let image = ColorImage::from_rgba_unmultiplied(
                [self.inner.width as usize, self.inner.height as usize],
                self.inner.pixels(),
            );

            let options = TextureOptions {
                magnification: TextureFilter::Nearest,
                minification: TextureFilter::Linear,
                ..Default::default()
            };

            if let Some(ref mut tex) = self.texture {
                tex.set(image, options);
            } else {
                let name = format!("surface_{}", self.inner.id);
                self.texture = Some(ctx.load_texture(name, image, options));
            }
        }

        self.texture
            .as_ref()
            .expect("texture was just initialised above")
    }
}
