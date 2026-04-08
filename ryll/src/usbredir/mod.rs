//! USB redirection protocol parser and message types.
//!
//! The usbredir protocol is carried inside SPICE SpiceVMC DATA messages.
//! This module handles parsing and serialisation of the usbredir-level
//! messages independently of the SPICE transport.

pub mod constants;
pub mod messages;
pub mod parser;
