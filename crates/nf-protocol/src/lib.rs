#![forbid(unsafe_code)]
#![cfg_attr(not(test), deny(clippy::disallowed_types))]

pub mod itch5;
pub mod moldudp64;
pub mod packet;

pub const MAX_MSG_LEN: usize = 64;
