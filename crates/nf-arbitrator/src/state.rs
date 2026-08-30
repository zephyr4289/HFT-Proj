//! Macro state machine for the sequencer (doc 05 §3).

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum State {
    /// Initial unanchored state before first data packet in session.
    Init = 0,
    /// Contiguous in-order stream (steady state).
    Contig = 1,
    /// Gap opened: actively staging disordered packets.
    Gap = 2,
    /// End-of-session persist train: actively recovering remaining gaps (doc 05 C11).
    EosPersist = 3,
    /// End-of-session reached for current session.
    Ended = 4,
    /// Permanently sealed / dead.
    Dead = 5,
}
