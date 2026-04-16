use tracing::debug;

pub trait ClipboardProvider: Send + Sync {
    fn set_text(&self, text: &str);
    fn get_text(&self) -> Option<String>;
}

pub struct ArboardProvider;

impl ClipboardProvider for ArboardProvider {
    fn set_text(&self, text: &str) {
        match arboard::Clipboard::new().and_then(|mut cb| cb.set_text(text)) {
            Ok(()) => {}
            Err(e) => debug!("clipboard: set_text failed: {}", e),
        }
    }

    fn get_text(&self) -> Option<String> {
        arboard::Clipboard::new()
            .and_then(|mut cb| cb.get_text())
            .ok()
    }
}
