/// Display surface management for egui rendering
use eframe::egui::{ColorImage, Context, TextureFilter, TextureHandle, TextureOptions};

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
    /// Create a new surface with the given dimensions
    pub fn new(id: u32, width: u32, height: u32) -> Self {
        let size = (width * height * 4) as usize;
        let mut pixels = vec![0u8; size];

        // Initialize with a dark gray background
        for i in (0..size).step_by(4) {
            pixels[i] = 50; // R
            pixels[i + 1] = 50; // G
            pixels[i + 2] = 50; // B
            pixels[i + 3] = 255; // A
        }

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
