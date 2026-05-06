//! Embedded browser-shell assets.
//!
//! `index.html` is served with a runtime substitution of
//! `{{TOKEN}}` placeholders so the script and stylesheet
//! subresource fetches carry the per-launch token. `app.js`
//! and `style.css` are served verbatim and gated only by the
//! token middleware.

pub const INDEX_HTML: &str = include_str!("assets/index.html");
pub const APP_JS: &str = include_str!("assets/app.js");
pub const STYLE_CSS: &str = include_str!("assets/style.css");

/// Replace `{{TOKEN}}` placeholders in `INDEX_HTML` with the
/// runtime token. Used by the `GET /` handler so the
/// rendered page's `<script src>` and `<link href>` URLs
/// carry the correct token.
pub fn render_index(token: &str) -> String {
    INDEX_HTML.replace("{{TOKEN}}", token)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_index_substitutes_all_placeholders() {
        let rendered = render_index("testtoken");
        assert!(
            !rendered.contains("{{TOKEN}}"),
            "all placeholders should be replaced"
        );
        assert!(
            rendered.contains("testtoken"),
            "token should appear in rendered output"
        );
    }
}
