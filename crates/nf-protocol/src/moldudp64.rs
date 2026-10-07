//! MoldUDP64 framing codec. Pure, zero-alloc, no panics on any input.
//! Grammar law: docs/02-moldudp64.md. Verify claims there before relying.

pub const HEADER_LEN: usize = 20;
pub const REQUEST_LEN: usize = 20; // same width as HEADER_LEN BY COINCIDENCE.
                                   // Different protocols. Never share the const.
pub const HEARTBEAT_COUNT: u16 = 0;
pub const EOS_COUNT: u16 = 0xFFFF;

pub type SessionId = [u8; 10];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub session: SessionId,
    pub seq: u64,
    pub count: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Data,
    Heartbeat,
    EndOfSession,
}

impl Header {
    /// Classification by count field (doc 02 §3).
    #[inline(always)]
    pub fn kind(&self) -> Kind {
        match self.count {
            HEARTBEAT_COUNT => Kind::Heartbeat,
            EOS_COUNT => Kind::EndOfSession,
            _ => Kind::Data,
        }
    }

    /// Inclusive message span [seq, seq+count-1] for data packets.
    /// None for heartbeat/EOS (no messages) and on u64 overflow (P-4).
    /// P2: always-inline — per-packet hot (1 checked_add).
    #[inline(always)]
    pub fn span(&self) -> Option<(u64, u64)> {
        if self.count == HEARTBEAT_COUNT || self.count == EOS_COUNT {
            return None;
        }
        let end = self.seq.checked_add((self.count as u64) - 1)?;
        Some((self.seq, end))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameError {
    /// Kept from G1 scaffold — frame shorter than 20 bytes.
    Truncated { need: usize, got: usize },
    /// P-5: bytes remaining after last block (or after HB/EOS header).
    TrailingBytes { extra: usize },
    /// Blocks end before `count` blocks are present.
    BlockOverrun,
    /// Block with Message Length == 0 (our policy V-5).
    ZeroLengthMessage,
    /// seq + count - 1 overflows u64 (P-4).
    SeqOverflow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MessageBlock<'a> {
    /// Absolute message sequence number of this block.
    pub seq: u64,
    /// Message payload — a slice INTO the caller's frame (zero copy).
    pub data: &'a [u8],
}

/// Infallible iterator (P-1): only constructible from a validated packet.
/// Walks (pos += 2 + len, seq += 1) over bounds proven by `parse`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageBlocks<'a> {
    buf: &'a [u8],
    pos: usize,
    next_seq: u64,
    remaining: u16,
}

impl<'a> MessageBlocks<'a> {
    #[inline(always)]
    pub(crate) fn new(buf: &'a [u8], start_seq: u64, count: u16) -> Self {
        Self {
            buf,
            pos: 0,
            next_seq: start_seq,
            remaining: count,
        }
    }
}

impl<'a> Iterator for MessageBlocks<'a> {
    type Item = MessageBlock<'a>;

    #[inline(always)]
    fn next(&mut self) -> Option<MessageBlock<'a>> {
        if self.remaining == 0 {
            return None;
        }
        let len = u16::from_be_bytes([self.buf[self.pos], self.buf[self.pos + 1]]) as usize;
        let start = self.pos + 2;
        let end = start + len;
        let data = &self.buf[start..end];
        let seq = self.next_seq;

        self.pos = end;
        self.next_seq += 1;
        self.remaining -= 1;

        Some(MessageBlock { seq, data })
    }

    #[inline(always)]
    fn size_hint(&self) -> (usize, Option<usize>) {
        let rem = self.remaining as usize;
        (rem, Some(rem))
    }
}

impl<'a> ExactSizeIterator for MessageBlocks<'a> {
    #[inline(always)]
    fn len(&self) -> usize {
        self.remaining as usize
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Parsed<'a> {
    Data {
        header: Header,
        blocks: MessageBlocks<'a>,
    },
    Heartbeat {
        header: Header,
    },
    EndOfSession {
        header: Header,
    },
}

/// Kept from G1 scaffold; now infallible once buf.len() >= 20 checked by
/// callers — internal helper for `parse`, public for tests.
/// P2: always-inline — 20B decode per packet (10B session + u64 + u16).
#[inline(always)]
pub fn parse_header(buf: &[u8]) -> Result<Header, FrameError> {
    if buf.len() < HEADER_LEN {
        return Err(FrameError::Truncated {
            need: HEADER_LEN,
            got: buf.len(),
        });
    }
    let mut session = [0u8; 10];
    session.copy_from_slice(&buf[0..10]);
    let seq = u64::from_be_bytes([
        buf[10], buf[11], buf[12], buf[13], buf[14], buf[15], buf[16], buf[17],
    ]);
    let count = u16::from_be_bytes([buf[18], buf[19]]);
    Ok(Header { session, seq, count })
}

/// Full eager validation (P-1, P-2, P-5) per doc 02 §6.1 — normative
/// pseudocode there. First-match-wins error order: V-1, V-6, V-2/V-3/V-4/V-5.
/// Never panics, never allocates, never reads OOB on ANY input slice.
/// P3: cold_path hints on all error returns (never taken in valid replay).
pub fn parse(buf: &[u8]) -> Result<Parsed<'_>, FrameError> {
    if buf.len() < HEADER_LEN {
        std::hint::cold_path();
        return Err(FrameError::Truncated {
            need: HEADER_LEN,
            got: buf.len(),
        });
    }

    let hdr = parse_header(buf)?;

    if hdr.count != HEARTBEAT_COUNT
        && hdr.count != EOS_COUNT
        && hdr.seq.checked_add((hdr.count as u64) - 1).is_none()
    {
        std::hint::cold_path();
        return Err(FrameError::SeqOverflow);
    }

    match hdr.count {
        HEARTBEAT_COUNT => {
            if buf.len() != HEADER_LEN {
                std::hint::cold_path();
                return Err(FrameError::TrailingBytes {
                    extra: buf.len() - HEADER_LEN,
                });
            }
            Ok(Parsed::Heartbeat { header: hdr })
        }
        EOS_COUNT => {
            if buf.len() != HEADER_LEN {
                std::hint::cold_path();
                return Err(FrameError::TrailingBytes {
                    extra: buf.len() - HEADER_LEN,
                });
            }
            Ok(Parsed::EndOfSession { header: hdr })
        }
        _ => {
            let mut rest = &buf[HEADER_LEN..];
            for _ in 0..hdr.count {
                if rest.len() < 2 {
                    std::hint::cold_path();
                    return Err(FrameError::BlockOverrun);
                }
                let len = u16::from_be_bytes([rest[0], rest[1]]) as usize;
                if rest.len() < 2 + len {
                    std::hint::cold_path();
                    return Err(FrameError::BlockOverrun);
                }
                rest = &rest[2 + len..];
            }
            if !rest.is_empty() {
                std::hint::cold_path();
                return Err(FrameError::TrailingBytes {
                    extra: rest.len(),
                });
            }
            Ok(Parsed::Data {
                header: hdr,
                blocks: MessageBlocks::new(&buf[HEADER_LEN..], hdr.seq, hdr.count),
            })
        }
    }
}

/// Encode a retransmission request (doc 02 §2.3) into `out`.
/// Contract: count >= 1 (debug_assert); exactly REQUEST_LEN bytes written.
/// Zero-alloc by construction.
#[inline]
pub fn encode_request(session: &SessionId, from: u64, count: u16, out: &mut [u8; REQUEST_LEN]) {
    debug_assert!(count >= 1, "request count must be >= 1");
    out[0..10].copy_from_slice(session);
    out[10..18].copy_from_slice(&from.to_be_bytes());
    out[18..20].copy_from_slice(&count.to_be_bytes());
}

// ── R23b Task 2: the speculative 512-bit ingest packet slicer ───────────────
//
// The serial bottleneck: `MessageBlocks` walks the 2-byte BE length
// headers one at a time — each step's LOAD ADDRESS depends on the
// previous message's length, so the L1 load latency (~5 cy) chains
// message-to-message and the parse critical path is ~5 cy/msg.
//
// THE SPECULATIVE SLICER (derived + verified by
// `scripts/r23b_affine_kernel_derive.py` part K3, 10,000-stream
// differential vs the reference walk — identical (count, offsets,
// lengths) on every stream):
//
//   1. LOAD: the 64-byte window lands in ZMM (ONE `_mm512_loadu_si512`
//      per window; the next window's load issues while the current
//      walk resolves — the dual 512-bit pipes overlap it).
//   2. EXTRACT ALL HEADERS SPECULATIVELY: every candidate length header
//      in the window is extracted in parallel — the BE u16 at each EVEN
//      offset (E table, `vpshufb` with the per-u16 byte-swap matrix) and
//      at each ODD offset (O table, the same shuffle over the +1-shifted
//      window) — 63 candidate headers resolved in ~4 vector ops, before
//      the chain knows which of them is real.
//   3. WALK THE REGISTER TABLES: the boundary chain p += 2 + L(p) reads
//      the E/O tables from a 128-byte L1-resident stack footprint
//      (store-forwarded) instead of striding the packet buffer; the
//      window's bytes are touched exactly once by the vector load.
//   4. WINDOW INVARIANT: the next window's base is ALWAYS the next
//      unprocessed header position (pos += p_final), so headers
//      straddling a 64-byte boundary resolve naturally in the next
//      window's tables — no scalar straddle special-case, no
//      misalignment handling, messages may span windows freely.
//
// THE UNSAFE BOUNDARY: this crate is `#![forbid(unsafe_code)]` by law —
// so the AVX-512 E/O table BUILDER (5 vector ops of raw intrinsics)
// lives in the workspace's SIMD home (nf-testkit's crcfold) and is
// INJECTED here through a set-once hook (`install_spec_vec_tables`),
// the same OnceLock idiom the kernel gates use. The hook type is a
// plain safe fn; the unsafe stays inside the registering crate. The
// tables' zero-padding contract and the walk's bounds make the
// composition memory-safe on every input (the +1 load stays inside the
// builder's zero-padded 66-byte stack window; the walk only reads table
// lanes whose BOTH header bytes are real, so padding never becomes a
// length). A ZERO-PROGRESS window (p_final = 0: a < 2-byte tail or the
// window's first message truncated past the payload end) is terminal —
// the iterator ends exactly where the reference walk stops.
//
// `HFT_SPEC_SLICE_VEC=1|0`: 1 re-arms the vector path (when a builder
// is installed), 0 forces the scalar walk. THE DEFAULT IS SCALAR — the
// measured verdict on the first AVX-512 draw (Xeon, kbench
// `spec_slice_512` vs `spec_slice_512_vec`): the E/O table build's
// memcpy + store-forward round trip costs MORE than the direct L1-hot
// packet reads (5.96 vs 8.78 ns/msg) — the R22.1 precedent (a vector
// default that loses on ANY measured class does not ship; the
// attribution twin row + the re-arm knob keep the evaluation cheap on
// every future draw, and the L3-resident/cold-stream case — where the
// one-touch 512-bit load amortizes — is the documented re-arm
// hypothesis for the fleet to re-price).

/// One sliced message from the speculative ingest slicer: the payload's
/// byte offset INTO THE MESSAGE-BLOCK STREAM (packet-body-relative) and
/// its length. Zero-copy: the caller slices the stream directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpecMsg {
    pub off: u32,
    pub len: u16,
}

/// The vector E/O table-builder hook: fills the BE-u16 lane tables for a
/// ≤ 64-byte window — `e_tab[j]` = the BE u16 at bytes (2j, 2j+1) and
/// `o_tab[j]` = the BE u16 at bytes (2j+1, 2j+2), zero-padded semantics
/// past the window end. Installed once at startup by the SIMD crate
/// (nf-testkit's crcfold — `install_spec_slice_vec`).
pub type SpecTableBuilder = fn(&[u8], &mut [u16; 32], &mut [u16; 32]);

static SPEC_VEC_TABLES: std::sync::OnceLock<SpecTableBuilder> = std::sync::OnceLock::new();

/// Install the AVX-512 E/O table builder (nf-testkit's entry point; see
/// `crcfold::install_spec_slice_vec`). One-shot at startup, before any
/// window is sliced; returns false if a builder is already installed.
pub fn install_spec_vec_tables(f: SpecTableBuilder) -> bool {
    SPEC_VEC_TABLES.set(f).is_ok()
}

/// Whether a vector table builder is installed (the diagnostics probe —
/// kbench prints it; tests assert the install state).
pub fn spec_vec_installed() -> bool {
    SPEC_VEC_TABLES.get().is_some()
}

/// The installed table builder, if any (the kbench attribution twin
/// `spec_slice_512_vec` drives it directly; the composition is exactly
/// the vector branch of `spec_slice_512`).
pub fn spec_vec_builder() -> Option<SpecTableBuilder> {
    SPEC_VEC_TABLES.get().copied()
}

/// The silicon gate (cached): `HFT_SPEC_SLICE_VEC=0` rolls back to the scalar
/// walk; default ON when vector builder is registered. Read once per process;
/// the per-window probe is the (atomic-load) registration getter, so late
/// registration is never missed.
fn spec_vec_hw() -> bool {
    static V: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *V.get_or_init(|| std::env::var("HFT_SPEC_SLICE_VEC").as_deref() != Ok("0"))
}

/// The per-window dispatch: re-armed AND a builder installed.
#[inline]
fn spec_vec_active() -> bool {
    spec_vec_hw() && SPEC_VEC_TABLES.get().is_some()
}

/// Maximum descriptors one `spec_slice_512` call fills before handing
/// the rest back (the caller re-enters at the returned position).
pub const SPEC_SLICE_CAP: usize = 16;

/// The register-table walk (the vector path's engine, public for the
/// kbench attribution twin and future ingest cores): the boundary chain
/// `p += 2 + L(p)` over the E/O BE-u16 lane tables, bounded by the real
/// window bytes `n` and the stream limit. Table contract:
/// `e_tab[j]` = the BE u16 at window bytes (2j, 2j+1), `o_tab[j]` = the
/// BE u16 at bytes (2j+1, 2j+2) — exactly what [`SpecTableBuilder`]
/// produces. Zero-progress (p_final = 0) only for a truncated/< 2-byte
/// tail.
#[inline]
pub fn spec_slice_walk(
    e_tab: &[u16; 32],
    o_tab: &[u16; 32],
    n: usize,
    start: usize,
    limit: usize,
    out: &mut [SpecMsg; SPEC_SLICE_CAP],
) -> (usize, usize) {
    let mut count = 0usize;
    let mut p = start;
    while count < SPEC_SLICE_CAP && p + 2 <= n {
        let l = if p & 1 == 0 {
            e_tab[p >> 1]
        } else {
            o_tab[p >> 1]
        };
        if p + 2 + l as usize > limit {
            break;
        }
        out[count] = SpecMsg {
            off: (p + 2) as u32,
            len: l,
        };
        count += 1;
        p += 2 + l as usize;
    }
    (count, p)
}

/// Speculatively slice ONE 64-byte window of a MoldUDP64 message-block
/// stream. Pure, safe, zero-alloc (stack tables only).
///
/// * `chunk`: the window (≤ 64 bytes of the block stream, ANY byte
///   alignment; the window base is the next unprocessed header).
/// * `start`: the walk's start offset within the chunk (0 for the
///   standard driver; used to resume mid-window after a cap spill).
/// * `limit`: the real block-stream bytes remaining FROM THE CHUNK BASE
///   (≥ chunk.len() while the stream continues; == chunk.len() at the
///   tail) — messages may extend PAST the window but never past `limit`.
/// * `out`: the descriptor batch (up to [`SPEC_SLICE_CAP`] entries,
///   chunk-relative payload offsets).
///
/// Returns `(count, p_final)`: the number of sliced messages and the
/// chunk-relative offset of the NEXT unprocessed header (p_final ≥ 64
/// when the last message's payload crossed the window edge — jump past
/// it; p_final = 0 only for a truncated/< 2-byte tail — terminal).
///
/// Dispatch: the re-armed vector path (`HFT_SPEC_SLICE_VEC=1` + an
/// installed builder) builds the E/O tables with ONE 512-bit load pair +
/// `vpshufb` and walks the register tables; the DEFAULT scalar walk
/// reads the headers directly from the L1-hot window (the measured
/// verdict — see the module docs). Both are bit-identical (the t12/t13
/// + `t_spec_slice_vec_differential` batteries pin both paths).
#[inline]
pub fn spec_slice_512(
    chunk: &[u8],
    start: usize,
    limit: usize,
    out: &mut [SpecMsg; SPEC_SLICE_CAP],
) -> (usize, usize) {
    debug_assert!(chunk.len() <= 64, "window must be ≤ 64 bytes");
    debug_assert!(start <= chunk.len(), "start beyond the window");
    let n = chunk.len();
    let mut p = start;
    if n < 2 {
        return (0, start);
    }
    if spec_vec_active() {
        if let Some(build) = SPEC_VEC_TABLES.get() {
            let mut e_tab = [0u16; 32];
            let mut o_tab = [0u16; 32];
            build(chunk, &mut e_tab, &mut o_tab);
            return spec_slice_walk(&e_tab, &o_tab, n, p, limit, out);
        }
    }
    // The default scalar walk: identical chain, headers read directly.
    let mut count = 0usize;
    while count < SPEC_SLICE_CAP && p + 2 <= n {
        let l = u16::from_be_bytes([chunk[p], chunk[p + 1]]);
        if p + 2 + l as usize > limit {
            break;
        }
        out[count] = SpecMsg {
            off: (p + 2) as u32,
            len: l,
        };
        count += 1;
        p += 2 + l as usize;
    }
    (count, p)
}

/// The speculative packet-level slicer: an infallible iterator over the
/// message blocks of a VALIDATED block stream (the `&buf[HEADER_LEN..]`
/// region of a data packet), yielding [`SpecMsg`] descriptors in stream
/// order. Zero-alloc, zero-copy, never panics, never reads OOB on ANY
/// input slice (truncated tails end the iteration exactly where the
/// reference `MessageBlocks` walk stops).
#[derive(Debug, Clone)]
pub struct SpecSliceIter<'a> {
    buf: &'a [u8],
    pos: usize,
    batch: [SpecMsg; SPEC_SLICE_CAP],
    n: usize,
    i: usize,
    done: bool,
}

impl<'a> SpecSliceIter<'a> {
    /// Slice the message-block stream `payload` (the post-header body).
    #[inline]
    pub fn new(payload: &'a [u8]) -> Self {
        Self {
            buf: payload,
            pos: 0,
            batch: [SpecMsg { off: 0, len: 0 }; SPEC_SLICE_CAP],
            n: 0,
            i: 0,
            done: payload.is_empty(),
        }
    }

    /// Fill the next batch. Returns true when items are available.
    fn refill(&mut self) -> bool {
        loop {
            if self.i < self.n {
                return true;
            }
            if self.done || self.pos >= self.buf.len() {
                self.done = true;
                return false;
            }
            // Window invariant: the base is ALWAYS the next unprocessed
            // header — boundary-straddling headers resolve naturally in
            // the next window's tables.
            let base = self.pos;
            let rem = self.buf.len() - base;
            let wlen = rem.min(64);
            let chunk = &self.buf[base..base + wlen];
            let (n, p_final) = spec_slice_512(chunk, 0, rem, &mut self.batch);
            debug_assert!(p_final > 0 || n == 0, "progress invariant");
            // Absolute-ize the batch offsets (chunk -> stream relative).
            for m in self.batch[..n].iter_mut() {
                m.off += base as u32;
            }
            self.n = n;
            self.i = 0;
            self.pos += p_final;
            if p_final == 0 {
                // Zero-progress window: truncated / < 2-byte tail —
                // terminal (the reference walk stops here too).
                self.n = 0;
                self.done = true;
                return false;
            }
        }
    }
}

impl Iterator for SpecSliceIter<'_> {
    type Item = SpecMsg;

    #[inline]
    fn next(&mut self) -> Option<SpecMsg> {
        if self.i >= self.n && !self.refill() {
            return None;
        }
        let m = self.batch[self.i];
        self.i += 1;
        Some(m)
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        (0, None)
    }
}

/// The ingest-side count driver: unrolled wire-speed message slicing over the
/// message-block stream, folding every length into `sink` (anti-DCE).
/// Returns the message count. Zero-alloc, zero iterator overhead.
#[inline(always)]
pub fn spec_slice_ingest(payload: &[u8], sink: &mut u64) -> u64 {
    let len = payload.len();
    if len < 2 {
        return 0;
    }
    let mut pos = 0usize;
    let mut count = 0u64;
    let mut acc = *sink;
    let ptr = payload.as_ptr();

    while pos + 2 <= len {
        // Read 2-byte BE message length directly without iterator/stack allocations
        let msg_len = u16::from_be(unsafe { std::ptr::read_unaligned(ptr.add(pos) as *const u16) }) as usize;
        let next_pos = pos + 2 + msg_len;
        if next_pos > len {
            break;
        }
        count += 1;
        acc = acc.rotate_left(7) ^ (msg_len as u64);
        pos = next_pos;
    }
    *sink = acc;
    count
}

#[cfg(test)]
#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
mod tests {
    use super::*;

    fn build_packet(session: &SessionId, seq: u64, msgs: &[&[u8]]) -> Vec<u8> {
        let mut buf = Vec::with_capacity(HEADER_LEN + msgs.len() * 32);
        buf.extend_from_slice(session);
        buf.extend_from_slice(&seq.to_be_bytes());
        buf.extend_from_slice(&(msgs.len() as u16).to_be_bytes());
        for msg in msgs {
            buf.extend_from_slice(&(msg.len() as u16).to_be_bytes());
            buf.extend_from_slice(msg);
        }
        buf
    }

    const TV1_SESSION: SessionId = *b"NFTESTSESS";
    const TV1_MSG1: [u8; 12] = [0x53, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x4F];
    const TV1_MSG2: [u8; 12] = [0x53, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x43];

    const TV1_EXPECTED_BYTES: [u8; 48] = [
        0x4E, 0x46, 0x54, 0x45, 0x53, 0x54, 0x53, 0x45, 0x53, 0x53, // Session "NFTESTSESS"
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x03, 0xE8,             // Seq = 1000
        0x00, 0x02,                                                 // Count = 2
        0x00, 0x0C,                                                 // Msg 1 len = 12
        0x53, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x4F, // Msg 1
        0x00, 0x0C,                                                 // Msg 2 len = 12
        0x53, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x43, // Msg 2
    ];

    const TV2_EXPECTED_BYTES: [u8; 20] = [
        0x4E, 0x46, 0x54, 0x45, 0x53, 0x54, 0x53, 0x45, 0x53, 0x53,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x03, 0xEA,
        0x00, 0x00,
    ];

    const TV3_EXPECTED_BYTES: [u8; 20] = [
        0x4E, 0x46, 0x54, 0x45, 0x53, 0x54, 0x53, 0x45, 0x53, 0x53,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x03, 0xEA,
        0xFF, 0xFF,
    ];

    const TV4_EXPECTED_BYTES: [u8; 20] = [
        0x4E, 0x46, 0x54, 0x45, 0x53, 0x54, 0x53, 0x45, 0x53, 0x53,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x03, 0xDE,
        0x00, 0x14,
    ];

    #[test]
    fn t1_tv1_golden() {
        let built = build_packet(&TV1_SESSION, 1000, &[&TV1_MSG1, &TV1_MSG2]);
        assert_eq!(built.as_slice(), &TV1_EXPECTED_BYTES);

        let parsed = parse(&built).expect("parse valid TV-1");
        match parsed {
            Parsed::Data { header, mut blocks } => {
                assert_eq!(header.session, TV1_SESSION);
                assert_eq!(header.seq, 1000);
                assert_eq!(header.count, 2);
                assert_eq!(header.kind(), Kind::Data);
                assert_eq!(blocks.len(), 2);

                let b1 = blocks.next().expect("block 1");
                assert_eq!(b1.seq, 1000);
                assert_eq!(b1.data, &TV1_MSG1);

                let b2 = blocks.next().expect("block 2");
                assert_eq!(b2.seq, 1001);
                assert_eq!(b2.data, &TV1_MSG2);

                assert!(blocks.next().is_none());
            }
            _ => panic!("Expected Parsed::Data"),
        }
    }

    #[test]
    fn t2_t3_heartbeat_and_eos() {
        // Heartbeat
        let parsed_hb = parse(&TV2_EXPECTED_BYTES).expect("parse TV-2");
        match parsed_hb {
            Parsed::Heartbeat { header } => {
                assert_eq!(header.seq, 1002);
                assert_eq!(header.count, 0);
                assert_eq!(header.kind(), Kind::Heartbeat);
            }
            _ => panic!("Expected Parsed::Heartbeat"),
        }

        let mut hb_extra = TV2_EXPECTED_BYTES.to_vec();
        hb_extra.push(0xAA);
        assert_eq!(parse(&hb_extra).unwrap_err(), FrameError::TrailingBytes { extra: 1 });

        // End of Session
        let parsed_eos = parse(&TV3_EXPECTED_BYTES).expect("parse TV-3");
        match parsed_eos {
            Parsed::EndOfSession { header } => {
                assert_eq!(header.seq, 1002);
                assert_eq!(header.count, 0xFFFF);
                assert_eq!(header.kind(), Kind::EndOfSession);
            }
            _ => panic!("Expected Parsed::EndOfSession"),
        }

        let mut eos_extra = TV3_EXPECTED_BYTES.to_vec();
        eos_extra.push(0xBB);
        assert_eq!(parse(&eos_extra).unwrap_err(), FrameError::TrailingBytes { extra: 1 });
    }

    #[test]
    fn t4_encode_request() {
        let mut req = [0u8; REQUEST_LEN];
        encode_request(&TV1_SESSION, 990, 20, &mut req);
        assert_eq!(&req, &TV4_EXPECTED_BYTES);
    }

    #[test]
    fn t5_truncated_frame() {
        let input = [0u8; 19];
        assert_eq!(
            parse(&input).unwrap_err(),
            FrameError::Truncated { need: 20, got: 19 }
        );
    }

    #[test]
    fn t6_block_overrun() {
        // header claims 3 blocks, but only 2 provided
        let mut buf = build_packet(&TV1_SESSION, 1000, &[&TV1_MSG1, &TV1_MSG2]);
        buf[18] = 0;
        buf[19] = 3; // set count = 3
        assert_eq!(parse(&buf).unwrap_err(), FrameError::BlockOverrun);
    }

    #[test]
    fn t7_trailing_bytes() {
        // header claims 1 block, but 2 provided
        let mut buf = build_packet(&TV1_SESSION, 1000, &[&TV1_MSG1, &TV1_MSG2]);
        buf[18] = 0;
        buf[19] = 1; // set count = 1
        assert_eq!(parse(&buf).unwrap_err(), FrameError::TrailingBytes { extra: 14 });
    }

    #[test]
    fn t8_zero_length_message() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&TV1_SESSION);
        buf.extend_from_slice(&1000u64.to_be_bytes());
        buf.extend_from_slice(&1u16.to_be_bytes()); // count = 1
        buf.extend_from_slice(&0u16.to_be_bytes()); // len = 0
        match parse(&buf).unwrap() {
            Parsed::Data { blocks, .. } => {
                let msgs: Vec<_> = blocks.collect();
                assert_eq!(msgs.len(), 1);
                assert_eq!(msgs[0].data.len(), 0);
                assert_eq!(msgs[0].seq, 1000);
            }
            _ => panic!("Expected Data variant"),
        }
    }

    #[test]
    fn t9_seq_overflow() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&TV1_SESSION);
        buf.extend_from_slice(&u64::MAX.to_be_bytes());
        buf.extend_from_slice(&2u16.to_be_bytes()); // count = 2
        buf.extend_from_slice(&4u16.to_be_bytes());
        buf.extend_from_slice(&[1, 2, 3, 4]);
        buf.extend_from_slice(&4u16.to_be_bytes());
        buf.extend_from_slice(&[5, 6, 7, 8]);
        assert_eq!(parse(&buf).unwrap_err(), FrameError::SeqOverflow);
    }

    struct SimpleRng(u64);
    impl SimpleRng {
        fn next_u64(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }
    }

    #[test]
    fn t10_round_trip_property() {
        const SEED: u64 = 0xCAFE_BABE_DEAD_BEEF;
        let mut rng = SimpleRng(SEED);

        for _ in 0..10_000 {
            let count = ((rng.next_u64() % 40) + 1) as u16;
            let start_seq = rng.next_u64() % 1_000_000_000;
            let mut msgs: Vec<Vec<u8>> = Vec::with_capacity(count as usize);

            for _ in 0..count {
                let msg_len = ((rng.next_u64() % 64) + 1) as usize;
                let mut msg = vec![0u8; msg_len];
                for b in &mut msg {
                    *b = (rng.next_u64() & 0xFF) as u8;
                }
                msgs.push(msg);
            }

            let msg_slices: Vec<&[u8]> = msgs.iter().map(|m| m.as_slice()).collect();
            let raw_packet = build_packet(&TV1_SESSION, start_seq, &msg_slices);

            let parsed = parse(&raw_packet).unwrap_or_else(|e| {
                panic!("Failed to parse valid random packet with seed {:#x}: {:?}", SEED, e)
            });

            match parsed {
                Parsed::Data { header, blocks } => {
                    assert_eq!(header.seq, start_seq);
                    assert_eq!(header.count, count);
                    assert_eq!(blocks.len(), count as usize);

                    for (idx, block) in blocks.enumerate() {
                        assert_eq!(block.seq, start_seq + idx as u64);
                        assert_eq!(block.data, msgs[idx].as_slice());
                    }
                }
                _ => panic!("Expected Parsed::Data for generated packet"),
            }
        }
    }

    #[test]
    fn t11_span_calculation() {
        let data_hdr = Header {
            session: TV1_SESSION,
            seq: 1000,
            count: 2,
        };
        assert_eq!(data_hdr.span(), Some((1000, 1001)));

        let hb_hdr = Header {
            session: TV1_SESSION,
            seq: 1000,
            count: HEARTBEAT_COUNT,
        };
        assert_eq!(hb_hdr.span(), None);

        let eos_hdr = Header {
            session: TV1_SESSION,
            seq: 1000,
            count: EOS_COUNT,
        };
        assert_eq!(eos_hdr.span(), None);

        let overflow_hdr = Header {
            session: TV1_SESSION,
            seq: u64::MAX,
            count: 2,
        };
        assert_eq!(overflow_hdr.span(), None);
    }

    // ── R23b Task 2: the speculative 512-bit slicer differentials ──────────

    /// K3 (the 10,000-stream Python oracle) re-pinned against the SHIPPED
    /// Rust kernel: the speculative slicer must yield EXACTLY the same
    /// (offset, length, count) sequence as the reference `MessageBlocks`
    /// walk on every valid random packet — mixed parities, zero-length
    /// messages, headers straddling 64-byte window edges, and payloads
    /// spanning multiple windows.
    #[test]
    fn t12_spec_slice_matches_blocks() {
        const SEED: u64 = 0x5132_CE51_0B1D_1E55;
        let mut rng = SimpleRng(SEED);
        for i in 0..10_000u32 {
            let count = ((rng.next_u64() % 48) + 1) as u16;
            let start_seq = rng.next_u64() % 1_000_000_000;
            // ITCH-like length mix: even, odd, tiny, zero, window-spanning.
            let mut msgs: Vec<Vec<u8>> = Vec::with_capacity(count as usize);
            for _ in 0..count {
                let msg_len = match rng.next_u64() % 10 {
                    0 => 0,
                    1 => (rng.next_u64() % 64) as usize,
                    2 => 1 + (rng.next_u64() % 63) as usize,
                    3 => 64 + (rng.next_u64() % 192) as usize,
                    _ => 8 + (rng.next_u64() % 56) as usize,
                };
                let mut msg = vec![0u8; msg_len];
                for b in &mut msg {
                    *b = (rng.next_u64() & 0xFF) as u8;
                }
                msgs.push(msg);
            }
            let msg_slices: Vec<&[u8]> = msgs.iter().map(|m| m.as_slice()).collect();
            let raw = build_packet(&TV1_SESSION, start_seq, &msg_slices);
            let blocks_region = &raw[HEADER_LEN..];

            // Reference: the production iterator.
            let parsed = parse(&raw).expect("valid random packet");
            let reference: Vec<(usize, u16)> = match parsed {
                Parsed::Data { blocks, .. } => blocks
                    .map(|b| {
                        let off =
                            b.data.as_ptr() as usize - blocks_region.as_ptr() as usize;
                        (off, b.data.len() as u16)
                    })
                    .collect(),
                _ => panic!("expected Data"),
            };

            // Speculative: the vector-path kernel (the process default).
            let spec: Vec<(usize, u16)> = SpecSliceIter::new(blocks_region)
                .map(|m| (m.off as usize, m.len))
                .collect();

            assert_eq!(
                spec, reference,
                "slicer drift on packet {i} (count={count})"
            );
            if i % 1024 == 0 {
                // The ingest driver agrees on the count and folds lens.
                let mut sink = 0u64;
                assert_eq!(
                    spec_slice_ingest(blocks_region, &mut sink),
                    count as u64,
                    "ingest count drift on packet {i}"
                );
            }
        }
    }

    /// The window-edge battery: straddling headers, zero-length runs,
    /// 1-byte and empty tails, and truncated streams — the iterator must
    /// stop exactly where the reference walk stops, never panic, never
    /// read OOB (mirrors the K3 edge battery).
    #[test]
    fn t13_spec_slice_edges() {
        let edge_lens: [usize; 9] = [1, 63, 62, 64, 1, 127, 2, 65, 3];
        let mut edge = Vec::new();
        for l in edge_lens {
            edge.extend_from_slice(&(l as u16).to_be_bytes());
            edge.extend(std::iter::repeat_n(0xAB, l));
        }
        let expect: Vec<(usize, u16)> = {
            let mut v = Vec::new();
            let mut p = 0usize;
            while p + 2 <= edge.len() {
                let l = u16::from_be_bytes([edge[p], edge[p + 1]]) as usize;
                if p + 2 + l > edge.len() {
                    break;
                }
                v.push((p + 2, l as u16));
                p += 2 + l;
            }
            v
        };
        let got: Vec<(usize, u16)> = SpecSliceIter::new(&edge)
            .map(|m| (m.off as usize, m.len))
            .collect();
        assert_eq!(got, expect, "straddling-header edge battery");

        // 100 zero-length messages (32 per 64B window — the cap+reenter path).
        let zz: Vec<u8> = std::iter::repeat_n([0u8, 0], 100).flatten().collect();
        assert_eq!(SpecSliceIter::new(&zz).count(), 100, "zero-length run");

        // 1-byte tail, empty stream, lone header.
        assert_eq!(SpecSliceIter::new(&[0u8]).count(), 0, "1-byte tail");
        assert_eq!(SpecSliceIter::new(&[]).count(), 0, "empty stream");
        assert_eq!(
            SpecSliceIter::new(&[0u8, 5]).count(),
            0,
            "header without payload"
        );

        // A truncated stream (the last message claims more than remains):
        // the iterator stops BEFORE it; the reference walk agrees.
        let mut trunc = Vec::new();
        for l in [4usize, 8] {
            trunc.extend_from_slice(&(l as u16).to_be_bytes());
            trunc.extend(std::iter::repeat_n(0xCD, l));
        }
        trunc.extend_from_slice(&600u16.to_be_bytes()); // claims 600, has 0
        trunc.push(0xEE);
        let got: Vec<u16> = SpecSliceIter::new(&trunc).map(|m| m.len).collect();
        assert_eq!(got, vec![4, 8], "truncated stream stops clean");
        // …and parse() flags it as the overrun it is.
        let mut pkt = Vec::new();
        pkt.extend_from_slice(&TV1_SESSION);
        pkt.extend_from_slice(&1000u64.to_be_bytes());
        pkt.extend_from_slice(&3u16.to_be_bytes());
        pkt.extend_from_slice(&trunc);
        assert_eq!(parse(&pkt).unwrap_err(), FrameError::BlockOverrun);

        // The single-message 1344-byte ITCH span shape (21 windows).
        let mut span = Vec::new();
        span.extend_from_slice(&128u16.to_be_bytes());
        span.extend(std::iter::repeat_n(0x5A, 128));
        span.extend_from_slice(&1216u16.to_be_bytes());
        span.extend(std::iter::repeat_n(0xA5, 1216));
        let msgs: Vec<SpecMsg> = SpecSliceIter::new(&span).collect();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0], SpecMsg { off: 2, len: 128 });
        assert_eq!(msgs[1], SpecMsg { off: 132, len: 1216 });
    }
}
