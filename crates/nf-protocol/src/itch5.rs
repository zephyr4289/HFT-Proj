/// Total length of an ITCH 5.0 message by type byte, or None if unknown.
/// Backed by a const table; largest legal message is 50 bytes (doc 03).
pub fn msg_len(_type_byte: u8) -> Option<u8> {
    todo!("doc 03")
}
