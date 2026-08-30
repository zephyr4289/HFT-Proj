//! MoldUDP64 framing + per-block ITCH validation in one pass. This is the
//! single entry the arbitrator (doc 05) and replay (doc 04) will call.

use crate::{itch5, moldudp64};

#[derive(Debug, PartialEq, Eq)]
pub enum PacketError {
    Framing(moldudp64::FrameError),
    Payload(itch5::ItchError),
}

/// Parse the MoldUDP64 frame and validate every payload block against the
/// ITCH 5.0 LENGTH table. Returns Ok(Parsed) only if both framing and all
/// message payloads are structurally valid.
pub fn validate_frame(buf: &[u8]) -> Result<moldudp64::Parsed<'_>, PacketError> {
    let parsed = moldudp64::parse(buf).map_err(PacketError::Framing)?;
    if let moldudp64::Parsed::Data { ref blocks, .. } = parsed {
        for block in blocks.clone() {
            itch5::validate(block.data).map_err(PacketError::Payload)?;
        }
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_frame_synthetic() {
        const SESSION: moldudp64::SessionId = *b"TESTSESS01";
        let mut frame = Vec::new();
        frame.extend_from_slice(&SESSION);
        frame.extend_from_slice(&100u64.to_be_bytes()); // seq
        frame.extend_from_slice(&2u16.to_be_bytes()); // count = 2

        // Msg 1: System Event (12B)
        let msg1 = [
            0x53, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x4F,
        ];
        frame.extend_from_slice(&(msg1.len() as u16).to_be_bytes());
        frame.extend_from_slice(&msg1);

        // Msg 2: Delete (19B)
        let msg2 = [
            0x44, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1,
        ];
        frame.extend_from_slice(&(msg2.len() as u16).to_be_bytes());
        frame.extend_from_slice(&msg2);

        let res = validate_frame(&frame).expect("validate valid frame");
        match res {
            moldudp64::Parsed::Data { header, blocks } => {
                assert_eq!(header.seq, 100);
                assert_eq!(header.count, 2);
                assert_eq!(blocks.len(), 2);
            }
            _ => panic!("Expected Data"),
        }

        // Corrupt msg2 with invalid type byte 0xFE
        let mut bad_frame = frame.clone();
        let msg2_type_pos = 20 + 2 + 12 + 2;
        bad_frame[msg2_type_pos] = 0xFE;
        assert_eq!(
            validate_frame(&bad_frame),
            Err(PacketError::Payload(itch5::ItchError::UnknownType {
                t: 0xFE
            }))
        );
    }
}
