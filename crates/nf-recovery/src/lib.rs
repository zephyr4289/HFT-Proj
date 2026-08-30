//! Recovery Engine crate (doc 08).
//! Zero-allocation SPSC PacketMailbox, latest-wins CmdChannel register, and Thread R TCP recovery client.

#![allow(clippy::disallowed_types, clippy::disallowed_methods)]

pub mod channel;
pub mod client;
pub mod mailbox;
pub mod types;

pub use channel::{CmdChannel, CmdPayload};
pub use client::RecoveryClient;
pub use mailbox::{PacketMailbox, MAILBOX_SLOTS, MAILBOX_SLOT_SIZE};
pub use types::*;
