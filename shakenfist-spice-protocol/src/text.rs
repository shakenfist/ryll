//! Making server-supplied text safe to show a human.
//!
//! Some SPICE messages carry text the server chose, for the client to
//! log or display: `SPICE_MSG_NOTIFY` is the one this crate parses.
//! The wire bounds its length and nothing else, so the bytes are as
//! hostile as any other field, but unlike a width or a pixel format
//! they are not refused by being out of range — every byte sequence
//! is a "valid" string.
//!
//! The danger is where the string ends up. A client writes it to a
//! log and shows it in a notification list, so a server that embeds
//! control characters reaches past the message it was given:
//!
//! - A newline forges log lines. One NOTIFY becomes what looks like
//!   several independent records, with whatever level, timestamp and
//!   channel prefix the server likes, in a file someone later reads
//!   to work out what a session did.
//! - An escape sequence drives the terminal of whoever reads that
//!   log, or watches `stderr` live: `ESC[` and the 8-bit CSI `U+009B`
//!   move the cursor, recolour, clear the screen, retitle the window
//!   and on some terminals respond on the input stream.
//! - A bidi override reorders what a human sees without changing the
//!   bytes, so the text read aloud from a screen is not the text that
//!   was sent.
//!
//! [`sanitise_untrusted`] replaces every such character with
//! `U+FFFD`, the replacement character the lossy UTF-8 decode of the
//! same bytes already produces for invalid sequences. The result is
//! one line of printable text, as long as the input and no longer,
//! which is safe to pass to `tracing` or to put in a UI label.
//!
//! It is deliberately not a filter on *meaning*: a server can still
//! say anything it likes, including a lie about the client's own
//! state. Text that reads as a ryll message is a problem a prefix
//! cannot solve and this function does not try to.

use std::borrow::Cow;

/// Whether `c` is a character this crate refuses to pass on to a log
/// or a UI, as described in the [module docs](self).
///
/// The set is the C0 controls and `DEL`, the C1 controls, and the
/// bidirectional formatting characters. Every other character is
/// passed through unchanged, including the non-ASCII text a server is
/// entitled to send: a notify is localised, so filtering by script or
/// to printable ASCII would mangle a legitimate message.
fn is_refused(c: char) -> bool {
    match c {
        // C0 controls and DEL. Tab and newline are refused with the
        // rest: a log record is one line, and a column a server chose
        // is as much of a forgery as a line it chose.
        '\u{0}'..='\u{1f}' | '\u{7f}' => true,
        // C1 controls, which include the 8-bit CSI (U+009B) that some
        // terminals honour exactly as they honour `ESC[`.
        '\u{80}'..='\u{9f}' => true,
        // Bidi marks, embeddings, overrides and isolates: they
        // reorder the rendering of the text that follows them.
        '\u{200e}' | '\u{200f}' => true,
        '\u{202a}'..='\u{202e}' => true,
        '\u{2066}'..='\u{2069}' => true,
        _ => false,
    }
}

/// `text` with every character [`is_refused`] rejects replaced by
/// `U+FFFD`.
///
/// Borrows when there is nothing to replace, which is every message a
/// well-behaved server sends, so the common path does not allocate.
/// The replacement is one character for one character, so the result
/// is never longer than the input and a caller that has already
/// bounded the length keeps its bound.
pub fn sanitise_untrusted(text: &str) -> Cow<'_, str> {
    if !text.contains(is_refused) {
        return Cow::Borrowed(text);
    }
    Cow::Owned(
        text.chars()
            .map(|c| if is_refused(c) { '\u{fffd}' } else { c })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::sanitise_untrusted;
    use std::borrow::Cow;

    #[test]
    fn ordinary_text_is_borrowed_unchanged() {
        let text = "the guest's disk is full";
        assert!(matches!(sanitise_untrusted(text), Cow::Borrowed(_)));
        assert_eq!(sanitise_untrusted(text), text);
    }

    #[test]
    fn non_ascii_text_is_kept() {
        // A notify is localised: filtering to printable ASCII would
        // mangle a legitimate message.
        for text in [
            "disque plein",
            "ディスクがいっぱいです",
            "диск заполнен",
            "🔈",
        ] {
            assert!(matches!(sanitise_untrusted(text), Cow::Borrowed(_)));
            assert_eq!(sanitise_untrusted(text), text);
        }
    }

    #[test]
    fn a_newline_cannot_forge_a_log_line() {
        assert_eq!(
            sanitise_untrusted("disk full\nWARN authentication disabled"),
            "disk full\u{fffd}WARN authentication disabled"
        );
        // A CRLF is two refused characters, not one.
        assert_eq!(sanitise_untrusted("a\r\nb"), "a\u{fffd}\u{fffd}b");
    }

    #[test]
    fn an_escape_sequence_cannot_drive_a_terminal() {
        // ESC[2J clears the screen; ESC]0;...BEL retitles the window.
        assert_eq!(
            sanitise_untrusted("\u{1b}[2Jgone"),
            "\u{fffd}[2Jgone",
            "the ESC is replaced, leaving the rest as inert text"
        );
        // The 8-bit CSI needs no ESC to introduce it.
        assert_eq!(sanitise_untrusted("\u{9b}2J"), "\u{fffd}2J");
        // A bell and a DEL are refused too.
        assert_eq!(sanitise_untrusted("a\u{7}\u{7f}b"), "a\u{fffd}\u{fffd}b");
    }

    #[test]
    fn a_tab_is_refused_with_the_other_controls() {
        assert_eq!(sanitise_untrusted("level\tWARN"), "level\u{fffd}WARN");
    }

    #[test]
    fn bidi_overrides_cannot_reorder_the_rendering() {
        for c in [
            '\u{200e}', '\u{200f}', '\u{202a}', '\u{202e}', '\u{2066}', '\u{2069}',
        ] {
            let text = format!("a{}b", c);
            assert_eq!(
                sanitise_untrusted(&text),
                "a\u{fffd}b",
                "U+{:04X} was passed through",
                c as u32
            );
        }
    }

    #[test]
    fn the_result_is_never_longer_than_the_input() {
        let text = "\u{1b}\u{0}\u{9b}\u{202e}ok";
        assert_eq!(
            sanitise_untrusted(text).chars().count(),
            text.chars().count()
        );
    }

    #[test]
    fn an_empty_string_is_unchanged() {
        assert_eq!(sanitise_untrusted(""), "");
    }
}
