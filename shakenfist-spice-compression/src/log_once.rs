//! Warn once per process for each kind of refused input.
//!
//! The decoders here refuse input a server controls, and a server can
//! send the same refused image in a loop. A plain `warn!` at each
//! refusal would grow the log at the server's message rate, so the
//! refusal sites use [`warn_once!`] instead: the first refusal of each
//! kind logs at `warn`, and later ones are silent.
//!
//! This mirrors `shakenfist-spice-protocol`'s `warn_once!`, which this
//! crate cannot use as it does not depend on the protocol crate. The
//! registry is separate and is not part of the renderer's gap count;
//! the renderer records each failed decode under its own key.

use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};

fn registry() -> &'static Mutex<HashSet<&'static str>> {
    static SEEN: OnceLock<Mutex<HashSet<&'static str>>> = OnceLock::new();
    SEEN.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Record `key`, returning `true` the first time it is seen in this
/// process and `false` after.
pub(crate) fn first_time(key: &'static str) -> bool {
    registry()
        .lock()
        .expect("warn_once registry lock poisoned")
        .insert(key)
}

/// Whether `key` has been recorded, for tests that check a refusal
/// went through [`warn_once!`].
#[cfg(test)]
pub(crate) fn seen(key: &'static str) -> bool {
    registry()
        .lock()
        .expect("warn_once registry lock poisoned")
        .contains(key)
}

/// `tracing::warn!` the first time `key` fires in this process, and
/// nothing after. The key must be a string literal, so the registry
/// is bounded by the number of call sites whatever a server sends.
/// The message is only formatted when it is logged.
macro_rules! warn_once {
    ($key:literal, $($arg:tt)+) => {
        if $crate::log_once::first_time($key) {
            ::tracing::warn!($($arg)+);
        }
    };
}

pub(crate) use warn_once;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_is_new_once() {
        assert!(first_time("log_once:tests:a_key_is_new_once"));
        assert!(!first_time("log_once:tests:a_key_is_new_once"));
        assert!(seen("log_once:tests:a_key_is_new_once"));
    }

    #[test]
    fn the_macro_records_its_key() {
        for _ in 0..2 {
            warn_once!("log_once:tests:the_macro_records_its_key", "logged once");
        }
        assert!(!first_time("log_once:tests:the_macro_records_its_key"));
    }
}
