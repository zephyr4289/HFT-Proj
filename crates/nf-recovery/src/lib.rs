//! Recovery Engine crate (doc 08 v2.0, C9).
//! Non-blocking raw UDP recovery client, widening re-request logic, and fake server fault injector.

#![allow(clippy::disallowed_types, clippy::disallowed_methods)]

pub mod client;
pub mod types;

pub use client::RecoveryClient;
pub use types::*;
