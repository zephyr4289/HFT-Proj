//! Negative lint fixture crate: contains violations of banned types and methods (doc 07 §3 L2).

#![deny(clippy::disallowed_types)]
#![deny(clippy::disallowed_methods)]

pub fn tripwire_types() {
    let _v: Vec<u8> = Vec::new();
    let _s: String = String::new();
    let _map: std::collections::HashMap<u32, u32> = std::collections::HashMap::new();
    let _rc = std::rc::Rc::new(42);
    let _arc = std::sync::Arc::new(42);
}

pub fn tripwire_methods() {
    let _s = 42.to_string();
    let slice: &[u8] = b"hello";
    let _owned = slice.to_owned();
}
