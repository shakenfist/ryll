//! Embedded browser-shell assets.
//!
//! `index.html` is served with a runtime substitution of
//! `{{TOKEN}}` placeholders so the script and stylesheet
//! subresource fetches carry the per-launch token. `app.js`,
//! `style.css` and the vendored sfui stylesheets are served
//! verbatim and gated only by the token middleware. Nothing in
//! sfui fetches anything of its own -- no `@import`, no `url()`,
//! no `fetch()` -- so every subresource URL passes through the
//! template and carries a token.

pub const INDEX_HTML: &str = include_str!("assets/index.html");
pub const APP_JS: &str = include_str!("assets/app.js");
pub const STYLE_CSS: &str = include_str!("assets/style.css");

/// The sfui design system, vendored under `assets/sfui/` by
/// `tools/vendor.sh` in the shakenfist/sfui repository. Never
/// edit the vendored copy in place: change sfui and re-vendor,
/// or the next sync silently discards the change. The
/// `.sfui-commit` stamp that `vendor.sh` leaves there enrols
/// this repository in the daily fleet-wide `sfui-vendor` audit,
/// which fails if the copy is not verbatim at canonical HEAD.
///
/// Only the two stylesheets are embedded. `vendor.sh` copies a
/// fixed file list and `--check` diffs all of it, so the vendored
/// directory holds the whole distributable -- but the browser
/// shell is a single full-viewport video with four controls on
/// top, so the Lit runtime, morphdom, the components (a data
/// table, a tab strip, a theme toggle) and the logo have nothing
/// to render here, and the theme boot script is deliberately
/// unused because the page pins itself dark. Embedding them would
/// add ~100KB to every ryll binary to serve nothing. Add the
/// `include_str!` and the route when a page element actually
/// needs one.
pub const SFUI_TOKENS_CSS: &str = include_str!("assets/sfui/tokens.css");
pub const SFUI_SF_CSS: &str = include_str!("assets/sfui/sf.css");

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
