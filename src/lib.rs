//! Lightweight relay library for forwarding framed beacon traffic to a UDC2 Listener.
//!
//! This crate provides a small relay that accepts framed messages from a C2
//! source, spawns a per-beacon proxy that connects to a TeamServer (UDC2
//! listener), and forwards frames back and forth.

mod clue;
pub mod error;
mod udc2;

pub use crate::clue::{C2, Message, Udc2ListenerConfig, clue};
