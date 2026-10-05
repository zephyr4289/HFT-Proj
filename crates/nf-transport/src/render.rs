//! Renderer and virtual-clock driven transport (Tier F: zero heap allocations in hot path).

#![cfg_attr(not(test), deny(clippy::disallowed_types))]

use crate::sched_types::{ReplaySchedule, SchedEvent, SchedKind};
// R8: construction-time shared handle for the triple store (the RX-pipelined
// consumer's read-only view). Never touched in a hot path — the Tier-F
// allow mirrors the construction-time Vec allows below.
#[allow(clippy::disallowed_types)]
use std::sync::Arc;
use crate::{FeedId, FrameBatch, Transport};
use nf_protocol::itch5;
use nf_protocol::moldudp64::{EOS_COUNT, HEADER_LEN, HEARTBEAT_COUNT};
use nf_protocol::packet::FrameMemo;

#[derive(Debug, Clone, Copy, Default)]
pub struct Cursor {
    pub byte_offset: usize,
    pub msg_index: u64,
}

impl Cursor {
    /// P3: always-inline — steady-state is 1 compare + return (cursor in sync).
    #[inline(always)]
    pub fn seek_msg(&mut self, gt: &[u8], target_msg_index: u64) -> Option<usize> {
        if self.msg_index > target_msg_index {
            self.byte_offset = 0;
            self.msg_index = 0;
        }
        while self.msg_index < target_msg_index {
            if self.byte_offset + 2 > gt.len() {
                return None;
            }
            let len =
                u16::from_be_bytes([gt[self.byte_offset], gt[self.byte_offset + 1]]) as usize;
            self.byte_offset += 2 + len;
            self.msg_index += 1;
        }
        if self.byte_offset <= gt.len() {
            Some(self.byte_offset)
        } else {
            None
        }
    }
}

pub const ARENA_SLOT_SIZE: usize = 1500;
pub const ARENA_SLOTS: usize = 256;

/// Pre-rendered frame directory entry. `offset/len` locate the frame bytes in
/// `frames`; `patch` marks frames whose 10B session prefix is reset()-mutable
/// (zeroed at build, patched with the live session at poll time).
/// `blk_base/blk_count` locate the frame's precomputed `(seq, start, end)`
/// block triples in `triples` (Q1 indexed ingest — kills the serial length
/// chain in-window; empty for HB/EOS/tombstones).
/// `valid` (R2) is the frame's ITCH validation verdict memo: the EXACT leading
/// prefix of blocks passing `itch5::validate`, computed once here from the
/// immutable rendered bytes. Session patching never touches bodies, so the
/// verdict survives reset(). Consumed via `batch_memo()`.
///
/// R8 layout: 24 bytes — `release_vt` moved to the parallel `vts` array,
/// and `first_seq` (the frame's first block sequence number, from the
/// schedule) added so the sequencer's steady scan never touches the triple
/// store (first/last come from the slot; body bounds are derived — see the
/// FrameView doc).
#[derive(Debug, Clone, Copy)]
struct FrameMeta {
    offset: u32,
    len: u16,
    feed: FeedId,
    blk_base: u32,
    blk_count: u16,
    valid: u16,
    first_seq: u64,
}

/// F-1 (HFT_RXWARM): the RX pipeline's frame-entry warm-start emit
/// context (see `ReplayTransport::poll_warm` and pipeline.rs's RX
/// thread). Owned by the RX thread for the pipeline's life; `out` is the
/// mailbox buffer window the VERIFIED entries land in — the store that
/// REPLACES the classic per-frame slot push plus the accumulate-loop
/// build (the R12b sidecar's added-store failure mode is designed out:
/// nothing new crosses a cache line on the steady path).
pub(crate) struct WarmEmitCtx<'a> {
    /// The mailbox EntryBuf window for this publication step
    /// (`buf.entries[acc..]`; at least 256 slots by the accumulate
    /// loop's shape).
    pub(crate) out: &'a mut [nf_protocol::packet::FrameEntry<'static>],
    /// W: the frame-indexed warm array — the last pass's VERIFIED
    /// entries (the exact `FrameEntry` payload, sess words included).
    pub(crate) warm: &'a mut [nf_protocol::packet::FrameEntry<'static>],
    /// The pass-local frame index (restarts at every bake point — the
    /// schedule replays the same frame sequence every pass).
    pub(crate) idx: usize,
    /// Cumulative check-and-fix divergences (the rxdesc-law telemetry).
    pub(crate) fixes: u64,
    /// Frames emitted beyond `warm`'s capacity (a schedule that grew past
    /// its construction size; uncovered frames still publish the
    /// live-derived entry — the fallback IS the classic semantics).
    pub(crate) uncovered: u64,
    /// The current pass's baked session template (the elig bit-7
    /// compare's reference).
    pub(crate) sess_lo_tmpl: u64,
    pub(crate) sess_hi_tmpl: u64,
    /// Whether the elig byte's steady-ok bit is computed at all (the
    /// consumer ladder's gate — see pipeline.rs).
    pub(crate) compute_elig: bool,
}

/// The warm compare — all ten payload fields (the correspondence proof
/// that the remembered entry IS this pass's walk fact; padding never
/// participates).
#[inline(always)]
pub(crate) fn warm_entry_neq(
    a: &nf_protocol::packet::FrameEntry<'_>,
    b: &nf_protocol::packet::FrameEntry<'_>,
) -> bool {
    a.bytes.as_ptr() != b.bytes.as_ptr()
        || a.bytes.len() != b.bytes.len()
        || a.blocks.as_ptr() != b.blocks.as_ptr()
        || a.blocks.len() != b.blocks.len()
        || a.feed != b.feed
        || a.memo != b.memo
        || a.first_seq != b.first_seq
        || a.sess_lo != b.sess_lo
        || a.sess_hi != b.sess_hi
        || a.elig != b.elig
}

/// F-1: the pass-boundary template rewrite. After every bake point the
/// blob holds the new session everywhere (the auto-advance's synchronous
/// tail is the catch-all; the plain reset serves re-bake in full), so the
/// warm entries' session words and the elig byte's session-derived bit
/// are rewritten from the fresh template to keep the steady-state
/// compare clean — and the per-frame check then PROVES the assumption
/// (a region that somehow escaped the bake diverges the compare and is
/// fixed + counted, never silently trusted). The static fields are
/// never rewritten: `poll_warm`'s compare checks them against the walk's
/// live facts every pass.
pub(crate) fn warm_rewrite_session(
    warm: &mut [nf_protocol::packet::FrameEntry<'static>],
    sess_lo_tmpl: u64,
    sess_hi_tmpl: u64,
    compute_elig: bool,
) {
    for e in warm.iter_mut() {
        e.sess_lo = sess_lo_tmpl;
        e.sess_hi = sess_hi_tmpl;
        let static_ok = compute_elig
            && !e.blocks.is_empty()
            && e.memo
                == Some(nf_protocol::packet::FrameMemo {
                    valid_count: e.blocks.len() as u16,
                });
        e.elig = (e.elig & 3) | ((static_ok as u8) << 7);
    }
}

/// R9: the rendered blob, backed by an anonymous mmap marked
/// MADV_HUGEPAGE with a 2MB-ALIGNED base. WHY: the verification workers
/// stream ~7.5MB each over the shared blob — ~1875 4KB pages per worker,
/// right at the shared L2 STLB's capacity on the runner silicon, so a
/// measurable slice of the workers' line fetches pays a page-walk. With
/// transparent huge pages the same footprint is 8 TLB entries
/// (kbench-vs-fabric gap attribution: the packed pair ceiling is 29.8
/// GB/s on the 8573C draw, the real fabric delivers 26.6 — the length
/// mix is uniform (all spans >= 512B, measured), so the TLB is the
/// remaining structural suspect). THP mode `never` degrades to 4KB
/// pages harmlessly; `always`/`madvise` get the big pages at fault time
/// (the construction-time render writes every byte, so all faults happen
/// here, outside every measurement window).
struct MmapBlob {
    /// Fallback owner when mmap is unavailable (THP is an optimization,
    /// never a correctness dependency) — the heap block is leaked at Drop
    /// time instead of munmapped.
    heap: Option<Box<[u8]>>,
    /// The mmap's true base (what munmap needs; valid when heap.is_none()).
    map_base: *mut u8,
    map_len: usize,
    /// The 2MB-aligned slice base handed to the renderer.
    data: *mut u8,
    len: usize,
}

// SAFETY: MmapBlob owns its mapping/heap block exclusively; moving the
// transport across threads moves the owner (no shared aliasing).
unsafe impl Send for MmapBlob {}

impl std::ops::Deref for MmapBlob {
    type Target = [u8];
    #[inline]
    fn deref(&self) -> &[u8] {
        // SAFETY: the mapping is live for the owner's life; the renderer
        // only hands out immutable borrows after construction.
        unsafe { std::slice::from_raw_parts(self.data, self.len) }
    }
}

impl std::ops::DerefMut for MmapBlob {
    #[inline]
    fn deref_mut(&mut self) -> &mut [u8] {
        // SAFETY: as Deref; the owner holds the exclusive borrow.
        unsafe { std::slice::from_raw_parts_mut(self.data, self.len) }
    }
}

impl Drop for MmapBlob {
    fn drop(&mut self) {
        if self.heap.is_none() {
            // SAFETY: the mapping was created by `from_vec` and is unmapped
            // exactly once, here.
            unsafe {
                libc::munmap(self.map_base as *mut libc::c_void, self.map_len);
            }
        }
        // The heap fallback's Box drops here (or was never set).
    }
}

impl MmapBlob {
    /// Copy `src` into a fresh 2MB-aligned MADV_HUGEPAGE mapping sized to
    /// `src.len()` (plus alignment slack). The advice is set BEFORE the
    /// copy so the copy's faults occur on an advised VMA and the kernel
    /// allocates hugepages AT FAULT TIME (deterministic on madvise-mode
    /// runners); a post-copy fault pattern would leave the conversion to
    /// khugepaged's asynchronous collapse — a lottery (see R10b).
    fn from_vec(src: &[u8]) -> Self {
        const HP: usize = 2 << 20; // 2 MiB hugepage granularity
        let slack = if src.len().is_multiple_of(HP) { 0 } else { HP };
        let map_len = src.len() + slack;
        // SAFETY: fresh anonymous mapping; PROT_READ|PROT_WRITE.
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                map_len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if base == libc::MAP_FAILED {
            // Cold fallback: ordinary heap pages (Send-safe exclusive Box
            // owner; the block frees when the transport drops).
            log_backing_once(None, src.len());
            let b: Box<[u8]> = src.to_vec().into_boxed_slice();
            let len = b.len();
            let data = b.as_ptr() as *mut u8;
            return Self {
                heap: Some(b),
                map_base: data,
                map_len: len,
                data,
                len,
            };
        }
        let map_base = base as *mut u8;
        // Align the usable base up to a 2MB boundary so fault-time THP
        // covers everything except (at most) the final partial page run.
        let data = unsafe { map_base.add(HP - (map_base as usize & (HP - 1))) };
        debug_assert!(data as usize & (HP - 1) == 0);
        // SAFETY: [data, data+len) is within [map_base, map_base+map_len)
        // by the slack arithmetic above.
        unsafe {
            // R10b: THE ORDERING IS LOAD-BEARING. `MADV_HUGEPAGE` must
            // precede the copy: on a madvise-mode kernel, a fault on an
            // un-advised VMA allocates 4KB pages, and the advice set after
            // the fact does not convert them — khugepaged's asynchronous
            // collapse would decide, mid-measurement, whether the blob runs
            // on 8 TLB entries or ~1875 (observed: span-arm medians
            // 4.24B / 3.07B / 1.867B on identical code, cv 27% on the
            // losing draw — the collapse landed inside the window).
            libc::madvise(
                map_base as *mut libc::c_void,
                map_len,
                libc::MADV_HUGEPAGE,
            );
            std::ptr::copy_nonoverlapping(src.as_ptr(), data, src.len());
        }
        log_backing_once(Some((map_base, map_len)), src.len());
        Self {
            heap: None,
            map_base,
            map_len,
            data,
            len: src.len(),
        }
    }
}

/// R10b: one-time blob-backing telemetry. The THP dividend is only real if
/// the kernel actually granted hugepages at fault time — fragmentation or
/// `never` mode fall back to 4KB pages SILENTLY, and a gate draw that lost
/// the TLB lottery must be attributable from the log alone. One line per
/// process, printed at construction (outside every measurement window).
fn log_backing_once(map: Option<(*mut u8, usize)>, len: usize) {
    static LOGGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if LOGGED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    match map {
        None => {
            eprintln!(
                "BLOB_BACKING mode=heap-fallback len_mb={:.1} verdict=no-thp",
                len as f64 / 1048576.0
            );
        }
        Some((base, map_len)) => {
            let kb = smaps_anon_huge_kb(base as usize, base as usize + map_len);
            let verdict = match kb {
                Some(k) if k >= 2048 => "thp-granted",
                Some(_) => "thp-denied-or-partial",
                None => "smaps-unreadable",
            };
            eprintln!(
                "BLOB_BACKING mode=mmap anon_huge_kb={} map_mb={:.1} verdict={}",
                kb.unwrap_or(0),
                map_len as f64 / 1048576.0,
                verdict
            );
        }
    }
}

/// The mapping's `AnonHugePages` total from `/proc/self/smaps` (kB), or
/// `None` when smaps cannot be read. Cold, construction-time, once per
/// process — the scan cost never touches a measurement window.
fn smaps_anon_huge_kb(lo: usize, hi: usize) -> Option<u64> {
    use std::io::BufRead;
    let f = std::fs::File::open("/proc/self/smaps").ok()?;
    let mut in_vma = false;
    let mut kb: u64 = 0;
    for line in std::io::BufReader::new(f).lines() {
        let line = line.ok()?;
        if let Some((a, b)) = parse_smaps_header(&line) {
            if in_vma {
                // Our VMA ended; its AnonHugePages total is complete.
                return Some(kb);
            }
            in_vma = a <= lo && hi <= b;
            kb = 0;
        } else if in_vma {
            if let Some(v) = line.strip_prefix("AnonHugePages:") {
                kb = v.trim().trim_end_matches("kB").trim().parse().unwrap_or(0);
            }
        }
    }
    in_vma.then_some(kb)
}

/// smaps VMA headers look like `7f3a20000000-7f3a20100000 rw-p 00:00 0`;
/// field lines all carry a `keyword:` prefix that cannot parse as a
/// `<hex>-<hex>` range. Allocation-free (the zero-alloc law extends to
/// cold paths by discipline).
fn parse_smaps_header(line: &str) -> Option<(usize, usize)> {
    let (a, rest) = line.split_once('-')?;
    if a.is_empty() || !a.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let lo = usize::from_str_radix(a, 16).ok()?;
    let hi_end = rest
        .find(|c: char| !c.is_ascii_hexdigit())
        .unwrap_or(rest.len());
    if hi_end == 0 {
        return None;
    }
    let hi = usize::from_str_radix(&rest[..hi_end], 16).ok()?;
    Some((lo, hi))
}

pub struct ReplayTransport {
    schedule: ReplaySchedule,
    event_idx: usize,
    virtual_clock: u64,
    frames: MmapBlob,
    meta: Box<[FrameMeta]>,
    /// R8: parallel release-time stream (event order, one u64 per event).
    vts: Box<[u64]>,
    /// R8: vt-group boundaries (see poll_clamped's release proof).
    group_end: Box<[u32]>,
    /// Flat precomputed block index: one `(seq, start, end)` triple per
    /// message block, grouped per event. R8: shared (Arc) so the
    /// RX-pipelined consumer can build block slices cross-thread — the
    /// ONLY part of the rendered state the consumer touches directly
    /// (frame bytes reach it through the raw pointers the RX thread's
    /// slots carry, and the directory/pacing stay RX-private).
    #[allow(clippy::disallowed_types)]
    triples: Arc<[(u64, u32, u32)]>,
    /// R8: precomputed blob offsets of every patchable frame's 10B session
    /// prefix (reset-time session bake). R9: EVENT-ORDERED — one
    /// `(event_idx, offset)` pair per UNIQUE patchable frame (aliased
    /// duplicates share the primary's site and never append), positioned at
    /// the frame's first-render event. The prepatch walks this list against
    /// a CONSUMED-EVENT frontier (monotone by construction — events are
    /// consumed strictly in schedule order, whatever the blob's memory
    /// layout), which is what makes it aliasing-compatible: the old
    /// offset-ordered list assumed blob offsets grow with the schedule,
    /// an assumption R9's aliasing broke.
    patch_evts: Box<[(u32, u32)]>,
    /// R8: RX coalescing — vt-groups per poll (1 = exact pre-R8 pacing).
    coalesce: usize,
    body_prefetch: bool,
    session: [u8; 10],
    clock_clamp: Option<u64>,
    batch_event: [u32; 256],
    batch_event_len: usize,
    /// R9: number of duplicate-feed deliveries aliased onto a primary's
    /// blob region at construction (0 = no aliasing happened). Exposed so
    /// the RX thread can refuse the prepatch when blob offsets are no
    /// longer monotone in event order (see blob_aliasing()).
    aliased_frames: u32,
}

impl ReplayTransport {
    pub fn new(gt: &[u8], schedule: ReplaySchedule, session: [u8; 10]) -> Self {
        let first_vt = schedule
            .events
            .first()
            .map(|e| e.release_vt)
            .unwrap_or(0);
        // P9a: render every event frame NOW (startup, outside windows) through the
        // exact same render_event_standalone path poll() used before — byte-identical
        // output, ~zero per-frame cost in-window. Vec use is construction-only;
        // the hot path never allocates (PR-3 ALLOC_DELTA still 0 in-window).
        #[allow(clippy::disallowed_types)]
        let mut blob: Vec<u8> =
            Vec::with_capacity(schedule.events.len().saturating_mul(768));
        #[allow(clippy::disallowed_types)]
        let mut meta: Vec<FrameMeta> = Vec::with_capacity(schedule.events.len());
        // R8: parallel release-time stream (one u64 per event).
        #[allow(clippy::disallowed_types)]
        let mut vts: Vec<u64> = Vec::with_capacity(schedule.events.len());
        // Flat triple store: ~16B per message block, appended per rendered frame.
        #[allow(clippy::disallowed_types)]
        let mut triples: Vec<(u64, u32, u32)> = Vec::new();
        // R9: aliased-delivery count (assigned unconditionally by the
        // construction sweep below).
        let self_aliased_frames: u32;
        // R9: event-ordered patch list — (event_idx, blob offset) per
        // UNIQUE patchable frame, appended at its first render. The
        // prepatch's frontier is a consumed-EVENT index; the list's
        // order is therefore exactly the frontier's order.
        #[allow(clippy::disallowed_types)]
        let mut patch_evts: Vec<(u32, u32)> = Vec::new();
        {
            let mut cursors = [Cursor::default(), Cursor::default()];
            let mut scratch = [0u8; ARENA_SLOT_SIZE];
            // R9: blob aliasing map — (first_seq, first_msg, count) →
            // (blob offset, blk_base, blk_count, valid, last referencing
            // event). A duplicate-feed delivery of the same message range
            // renders BYTE-IDENTICAL frame bytes (same session prefix, same
            // seq/count header, same gt slice), so it can share the
            // primary's blob region. The map is construction-only (outside
            // every window).
            //
            // THE LAST-EVENT GATE (why the map carries `last_evt`): an
            // aliased region's bytes are read by the RX entry-build of
            // EVERY delivery that references them — the primary's render
            // AND each duplicate's render. A prepatch may rewrite the
            // session prefix only after ALL of those renders have happened,
            // i.e. only after the LAST referencing event's publication is
            // consumed. Gating the shared patch site on max(referencing
            // events) — instead of the primary's own event — is what keeps
            // the event-indexed prepatch sound under aliasing: patching a
            // region whose dup delivery is still pending would hand that
            // delivery's entry the NEXT session (the exact stale-session
            // divergence class the R8 kill switch exists for).
            type AliasKey = (u64, u64, u16);
            type AliasVal = (u32, u32, u16, u16, u32);
            #[allow(clippy::disallowed_types)]
            let mut alias_map: std::collections::HashMap<AliasKey, AliasVal> =
                std::collections::HashMap::new();
            let mut aliased_frames = 0u32;
            for (ev_i, ev) in schedule.events.iter().enumerate() {
                let feed_idx = (ev.feed as usize) & 1;
                if let Some(len) = render_event_standalone(
                    gt,
                    ev,
                    &schedule,
                    session,
                    &mut scratch,
                    &mut cursors[feed_idx],
                ) {
                    // Mirror of render_event_standalone's session rule (source of
                    // truth): patch iff the frame carries the resettable session.
                    let patch = match schedule.session_split {
                        Some((split_m, _)) => match ev.kind {
                            SchedKind::Packet { first_msg, .. } => first_msg < split_m,
                            SchedKind::Heartbeat { .. } | SchedKind::EndOfSession { .. } => false,
                        },
                        None => true,
                    };
                    // R9: alias lookup — a Packet whose (first_seq, first_msg,
                    // count) was rendered before AND whose immutable suffix
                    // (bytes [10..len], past the session prefix) memcmps equal
                    // reuses the primary's blob region + triple range. The
                    // session prefix [0..10] is per-pass state (zeroed here,
                    // baked per reset) and is intentionally excluded from the
                    // comparison; the patch flag is a function of first_msg
                    // (identical by the key), so both deliveries share one
                    // patch site — the bake cost halves with the blob.
                    //
                    // Outcome trichotomy drives the patch-site bookkeeping:
                    // * Aliased — the shared region's LAST-EVENT gate extends
                    //   to this event (this delivery reads those bytes too).
                    // * Primary (map miss) — the region registers in the map;
                    //   its site (gated by its own + future aliases' events)
                    //   is emitted by the post-sweep walk.
                    // * Rejected (memcmp fail — unreachable for deterministic
                    //   renders, guarded anyway) — an independent region that
                    //   must carry its OWN site, pushed in-loop below.
                    enum AliasOutcome {
                        Aliased,
                        Primary,
                        Rejected,
                    }
                    let mut alias: Option<(u32, u32, u16, u16)> = None;
                    let mut outcome = AliasOutcome::Primary;
                    if let SchedKind::Packet {
                        first_seq,
                        first_msg,
                        count,
                    } = ev.kind
                    {
                        if count > 0 {
                            if let Some(v) = alias_map.get_mut(&(first_seq, first_msg, count)) {
                                let a = v.0 as usize;
                                if a + len <= blob.len()
                                    && blob[a + HEADER_LEN..a + len] == scratch[HEADER_LEN..len]
                                {
                                    alias = Some((v.0, v.1, v.2, v.3));
                                    aliased_frames += 1;
                                    outcome = AliasOutcome::Aliased;
                                    // THE LAST-EVENT GATE: this delivery also
                                    // reads the shared region at ITS render —
                                    // extend the site's gate to this event.
                                    v.4 = v.4.max(ev_i as u32);
                                } else {
                                    outcome = AliasOutcome::Rejected;
                                }
                            }
                        }
                    }
                    let (off, blk_base, blk_count, valid_prefix) = if let Some(
                        (a_off, a_blk_base, a_blk_count, a_valid),
                    ) = alias
                    {
                        // Aliased: no blob append, no re-zero (the primary's
                        // region already carries the patch-state prefix), no
                        // triple re-walk (the frame bytes — and therefore the
                        // [len|msg] chain and its validation verdicts — are
                        // identical by the memcmp above).
                        (a_off, a_blk_base, a_blk_count, a_valid)
                    } else {
                        let off = blob.len() as u32;
                        blob.extend_from_slice(&scratch[..len]);
                        if patch {
                            let base = off as usize;
                            blob[base..base + 10].fill(0);
                        }
                        // Q1: index this frame's [len|msg] chain once (startup). The
                        // frame was just rendered valid, so the walk below only fails
                        // on internal inconsistency — then tombstone (never emit).
                        // R2: while walking, memoize the EXACT leading prefix of blocks
                        // passing ITCH validation — verdict of a pure function over
                        // these immutable bytes, one-time, outside every window.
                        let (blk_base, blk_count, valid_prefix) = match ev.kind {
                            SchedKind::Packet {
                                first_seq,
                                count,
                                ..
                            } => {
                                let base = triples.len() as u32;
                                let mut pos = HEADER_LEN;
                                let mut seq = first_seq;
                                let mut n: u16 = 0;
                                let mut ok = true;
                                let mut valid: u16 = 0;
                                for _ in 0..count {
                                    if scratch.len() < pos + 2 {
                                        ok = false;
                                        break;
                                    }
                                    let blen =
                                        u16::from_be_bytes([scratch[pos], scratch[pos + 1]])
                                            as usize;
                                    let start = pos + 2;
                                    let end = start + blen;
                                    if end > len {
                                        ok = false;
                                        break;
                                    }
                                    triples.push((seq, start as u32, end as u32));
                                    if valid == n
                                        && itch5::validate(&scratch[start..end]).is_ok()
                                    {
                                        valid += 1;
                                    }
                                    pos = end;
                                    seq = seq.wrapping_add(1);
                                    n += 1;
                                }
                                if !ok || pos != len {
                                    // Inconsistent with just-rendered bytes: drop the
                                    // partial triples and tombstone the frame.
                                    std::hint::cold_path();
                                    triples.truncate(base as usize);
                                    (0, 0, 0)
                                } else {
                                    (base, n, valid)
                                }
                            }
                            SchedKind::Heartbeat { .. } | SchedKind::EndOfSession { .. } => {
                                (0, 0, 0)
                            }
                        };
                        // Register the primary for future aliases (successful
                        // non-empty Packets only — tombstoned walks never
                        // registered, so dups of them render independently;
                        // a memcmp-failed re-render does NOT overwrite the
                        // first primary — its site stays gated by its own
                        // referencing events). last_evt starts at THIS
                        // event; future aliases extend it.
                        if let SchedKind::Packet {
                            first_seq,
                            first_msg,
                            count,
                        } = ev.kind
                        {
                            if count > 0 && blk_count == count {
                                alias_map
                                    .entry((first_seq, first_msg, count))
                                    .or_insert((
                                        off,
                                        blk_base,
                                        blk_count,
                                        valid_prefix,
                                        ev_i as u32,
                                    ));
                            }
                        }
                        (off, blk_base, blk_count, valid_prefix)
                    };
                    // A tombstoned-by-index frame must not be emitted: convert a
                    // (0,0) triple range on a NON-EMPTY Packet into a meta
                    // tombstone so poll skips it exactly like an unrenderable
                    // event. (Empty packets / HB / EOS legitimately carry no
                    // triples and keep their rendered 20B frame.)
                    let ev_count = match ev.kind {
                        SchedKind::Packet { count, .. } => count,
                        SchedKind::Heartbeat { .. } | SchedKind::EndOfSession { .. } => 0,
                    };
                    let tombstoned = ev_count > 0 && blk_count == 0;
                    let meta_first_seq = match ev.kind {
                        SchedKind::Packet { first_seq, .. } => first_seq,
                        _ => 0,
                    };
                    if tombstoned {
                        std::hint::cold_path();
                        meta.push(FrameMeta {
                            offset: 0,
                            len: 0,
                            feed: ev.feed,
                            blk_base: 0,
                            blk_count: 0,
                            valid: 0,
                            first_seq: 0,
                        });
                    } else {
                        // R9: patchable NON-map frames (HB / EOS / empty
                        // packets — nothing aliases onto them) and memcmp-
                        // REJECTED independent renders record their site at
                        // their own event. Primary full-Packet sites come
                        // from the alias map post-sweep, gated on the LAST
                        // referencing event (see the map's doc).
                        let non_map_frame = !matches!(
                            ev.kind,
                            SchedKind::Packet { count, .. } if count > 0
                        );
                        if patch && (non_map_frame || matches!(outcome, AliasOutcome::Rejected))
                        {
                            patch_evts.push((ev_i as u32, off));
                        }
                        meta.push(FrameMeta {
                            offset: off,
                            len: len as u16,
                            feed: ev.feed,
                            blk_base,
                            blk_count,
                            valid: valid_prefix,
                            first_seq: meta_first_seq,
                        });
                    }
                    vts.push(ev.release_vt);
                } else {
                    // Unrenderable event (frame >1500B scratch — unreachable for
                    // MTU-bound schedules): tombstone keeps event_idx aligned,
                    // exactly as the old skip-without-push did.
                    std::hint::cold_path();
                    meta.push(FrameMeta {
                        offset: 0,
                        len: 0,
                        feed: ev.feed,
                        blk_base: 0,
                        blk_count: 0,
                        valid: 0,
                        first_seq: 0,
                    });
                    vts.push(ev.release_vt);
                }
            }
            self_aliased_frames = aliased_frames;
            // R9: post-sweep — full-Packet patch sites come from the alias
            // map, each gated on its LAST referencing event (the primary's
            // render plus every aliased duplicate's render — the latest
            // read of those bytes this pass). The event-indexed prepatch
            // never rewrites a region before every reader of the current
            // pass has rendered it. Sorted by gate so the frontier walk is
            // a single linear sweep.
            for (&(_fs, first_msg, _c), &(off, _bb, _bc, _v, last_evt)) in alias_map.iter() {
                let patchable = match schedule.session_split {
                    Some((split_m, _)) => first_msg < split_m,
                    None => true,
                };
                if patchable {
                    patch_evts.push((last_evt, off));
                }
            }
            patch_evts.sort_unstable();
        }
        // R8: vt-group boundaries (see field doc). Single O(n) sweep: `j`
        // advances monotonically; each event inside a group maps to the
        // group's exclusive end.
        #[allow(clippy::disallowed_types)]
        let mut group_end: Vec<u32> = vec![0u32; vts.len()];
        {
            let mut i = 0usize;
            while i < vts.len() {
                let v = vts[i];
                let mut j = i + 1;
                while j < vts.len() && vts[j] <= v {
                    j += 1;
                }
                group_end[i..j].fill(j as u32);
                i = j;
            }
        }
        // R8/R9: the patch list is built in EVENT order at first render
        // (see patch_evts' field doc) — no derivation sweep needed.
        // R9: the blob moves into a 2MB-aligned MADV_HUGEPAGE mapping
        // (construction-time copy — every page faults here, outside every
        // measurement window; see MmapBlob).
        let mut t = Self {
            schedule,
            event_idx: 0,
            virtual_clock: first_vt,
            frames: MmapBlob::from_vec(&blob),
            meta: meta.into_boxed_slice(),
            vts: vts.into_boxed_slice(),
            group_end: group_end.into_boxed_slice(),
            #[allow(clippy::disallowed_types)]
            triples: {
                #[allow(clippy::disallowed_types)]
                let arc: Arc<[(u64, u32, u32)]> = triples.into_boxed_slice().into();
                arc
            },
            patch_evts: patch_evts.into_boxed_slice(),
            batch_event: [0u32; 256],
            batch_event_len: 0,
            session,
            clock_clamp: None,
            body_prefetch: true,
            coalesce: 1,
            aliased_frames: self_aliased_frames,
        };
        // R8: bake the construction session into the patchable prefixes —
        // the old lazy poll-patching did this on first release; without a
        // reset() the blob would otherwise serve zeroed sessions (caught by
        // the A-1 split-watermark test).
        t.patch_sessions();
        t
    }

    /// R6: control main-side frame-body prefetching in poll() (see field
    /// doc). Default ON (single-core TITAN behavior preserved bit-for-bit).
    #[inline]
    pub fn set_body_prefetch(&mut self, on: bool) {
        self.body_prefetch = on;
    }

    /// R8: RX coalescing control (see the `coalesce` field doc). `k = 1` is
    /// the exact pre-R8 pacing; `k > 1` releases up to `k` vt-groups per
    /// poll, NAPI-style, with `now_ns()` reporting the latest released
    /// group's virtual time. Conformance/golden/differential paths never
    /// touch this.
    #[inline]
    pub fn set_poll_coalesce(&mut self, k: usize) {
        self.coalesce = k.max(1);
    }

    #[inline]
    pub fn reset(&mut self, session: [u8; 10]) {
        let first_vt = self
            .schedule
            .events
            .first()
            .map(|e| e.release_vt)
            .unwrap_or(0);
        self.event_idx = 0;
        self.virtual_clock = first_vt;
        self.session = session;
        self.clock_clamp = None;
        // R8: patch every patchable frame's 10B session prefix NOW, once —
        // poll() used to do this per released frame inside the measurement
        // window (a 10B copy + branch per frame). reset() runs outside every
        // timed window in the burst arms, and its ~60-80µs blob rewrite is
        // 0.25% of the sustained arm's in-window reset cadence. The frames
        // poll() serves are byte-identical to the lazily-patched ones (same
        // bytes written, same positions, before any consumer can read them
        // — poll slices frames only after this loop completes).
        self.patch_sessions();
    }

    /// R8 phase-3b: write the current session into every patchable frame's 10B
    /// prefix (see `reset`). Also run at construction, so a transport that
    /// is polled without any reset() serves the construction session's
    /// bytes exactly as the old lazy poll-patching did.
    fn patch_sessions(&mut self) {
        let session = self.session;
        let frames = self.frames.as_mut_ptr();
        // R8 phase-6: the prefetchW experiment was reverted — Zen3 measured
        // it at or slightly behind the plain loop (AMD's ET0 hint buys
        // nothing there), and the measured-best configuration is what ships.
        // SAFETY: every offset in `patch_evts` was captured at construction
        // from a rendered (len >= 20) frame's own directory entry —
        // off..off+10 is in-bounds of `frames`; the list is immutable after
        // construction. (Order is event order; the full bake is
        // order-independent.)
        unsafe {
            for &(_, off) in self.patch_evts.iter() {
                let p = frames.add(off as usize);
                std::ptr::copy_nonoverlapping(session.as_ptr(), p, 10);
            }
        }
    }

    /// R9: the transport's current schedule-event cursor — the RX thread
    /// records each publication's exclusive end event index so the prepatch
    /// can map a freed publication to the events whose frames it carried.
    #[inline]
    pub fn current_event_idx(&self) -> usize {
        self.event_idx
    }

    /// R9: whether construction aliased any duplicate-feed deliveries onto
    /// a primary's blob region (diagnostics; the event-indexed prepatch is
    /// aliasing-compatible by construction).
    pub fn blob_aliasing(&self) -> bool {
        self.aliased_frames > 0
    }

    /// R9 diagnostics: the number of aliased duplicate deliveries.
    pub fn aliased_frame_count(&self) -> u32 {
        self.aliased_frames
    }

    /// R9: patch patchable frames whose FIRST-RENDER event index is below
    /// `upto_evt`, starting the walk at patch-list index `from_idx`, with
    /// an explicit `session` (the NEXT pass's — the transport's own
    /// `session` field still holds the current pass's until the advance).
    /// Returns the new patch-list index (the prepatch cursor). The walk
    /// touches AT MOST `budget` sites — R9c PACING: an unbounded step
    /// bursts ~250 RFOs in one go (5.5k unique sites over ~22 publications
    /// per pass on the mini tape), a stall comparable to the whole
    /// publication period on the fast runners — measured as the armed
    /// run's main-side wait_ms tripling (322 -> 813ms on the 8573C). A
    /// bounded step spreads the bake; the frontier keeps advancing and the
    /// advance-point tail covers whatever remains.
    ///
    /// WHY EVENT-INDEXED: the old offset-indexed walk assumed blob offsets
    /// grow monotonically with the schedule — true pre-R9, broken by blob
    /// aliasing, and the source of the frontier's fragility (an over-shoot
    /// patches frames of an UNCONSUMED publication; the next pass's render
    /// then reads a stale session and the consumer's cold path diverges —
    /// the +39-count flake class). The consumed-EVENT frontier is monotone
    /// BY CONSTRUCTION: the consumer frees publications strictly in turn
    /// order, and each turn's events were released in schedule order, so
    /// "every event below the frontier has been fully consumed" needs no
    /// offset inference at all — and holds under any blob layout.
    ///
    /// SAFETY CONTRACT: only frames whose ENTIRE publication has been
    /// consumed (buffer freed) may be prepatched — the consumer never
    /// re-reads a freed publication, span bodies live at frame[20..] and
    /// the patch writes frame[0..10], and the next pass's entry session
    /// words are computed at render time (after the advance), so the
    /// prepatch can never be observed mid-flight.
    pub(crate) fn patch_range(
        &mut self,
        session: &[u8; 10],
        from_idx: usize,
        upto_evt: usize,
        budget: usize,
    ) -> usize {
        let frames = self.frames.as_mut_ptr();
        let mut idx = from_idx;
        let end = (from_idx + budget).min(self.patch_evts.len());
        // SAFETY: same per-offset contract as patch_sessions; the walk is
        // bounded by the patch list's own length.
        unsafe {
            while idx < end {
                let (evt, off) = self.patch_evts[idx];
                if evt as usize >= upto_evt {
                    break;
                }
                let p = frames.add(off as usize);
                std::ptr::copy_nonoverlapping(session.as_ptr(), p, 10);
                idx += 1;
            }
        }
        idx
    }

    /// R8 phase-3b: `reset` without re-patching the already-prepatched
    /// prefix — the RX's prepatch cursor advanced through the consumed
    /// events during the pass, so only the tail ([from_idx..]) needs the
    /// synchronous bake at the advance point.
    pub fn reset_prepatched(&mut self, session: [u8; 10], from_idx: usize) {
        let first_vt = self
            .schedule
            .events
            .first()
            .map(|e| e.release_vt)
            .unwrap_or(0);
        self.event_idx = 0;
        self.virtual_clock = first_vt;
        self.session = session;
        self.clock_clamp = None;
        let session = self.session;
        let frames = self.frames.as_mut_ptr();
        // SAFETY: same per-offset contract as patch_sessions.
        unsafe {
            for &(_, off) in self.patch_evts.iter().skip(from_idx) {
                let p = frames.add(off as usize);
                std::ptr::copy_nonoverlapping(session.as_ptr(), p, 10);
            }
        }
    }

    #[inline]
    pub fn set_clock_clamp(&mut self, clamp: Option<u64>) {
        self.clock_clamp = clamp;
    }

    /// P3: always-inline + hoisted len/capacity, cold clamp path.
    /// P9a: per released frame: 2 indexed loads + 10B session patch + batch push.
    /// No cursor seeks, no length re-walk, no payload memcpy in-window.
    /// R8: (a) the release boundary comes from the precomputed `group_end`
    /// chain — poll() advances the virtual clock to the entering group's
    /// vt and releases the maximal prefix at or below it, which is exactly
    /// the pre-R8 per-frame comparison loop's release set (proved: the scan
    /// always breaks at the first event above `vclock`, and every member of
    /// `group_end[i]`'s run is `<= vts[i] <= vclock`), so the per-frame
    /// clock compare disappears from the release loop; (b) with
    /// `set_poll_coalesce(k > 1)` the chain advances `k` groups per call —
    /// NAPI-style receipt batching, `now_ns()` = latest released group's vt.
    /// F-1 (HFT_RXWARM): the number of frames a full pass emits (the
    /// non-tombstone events of the construction-fixed schedule) — the RX
    /// pipeline's warm array's exact capacity. Computed once at pipeline
    /// construction, outside every measured window.
    pub fn rendered_frame_count(&self) -> usize {
        self.meta.iter().filter(|m| m.len != 0).count()
    }

    /// F-2 (HFT_RXBUILD / CHECKLIST F-2 / ROADMAP1 §6-I1): the
    /// publish-by-reference MASTER — built ONCE at construction (outside
    /// every measured window), event-ordered over the pass's emitted
    /// frames (tombstones skipped — the master records the published
    /// subsequence, so the consumer's walk needs no empty-slot checks).
    /// Every frame of every pass is byte-identical except the 10 session
    /// bytes; the master's `bytes`/`blocks`/`memo`/`first_seq`/`feed`
    /// fields are construction-frozen and its `sess_lo`/`sess_hi`/`elig`
    /// fields are the only per-pass state (patched at the session
    /// boundaries by `pipeline::master_patch_range` — the prepatch's
    /// consumed-event frontier maps a freed publication to its master
    /// slice exactly as it maps to the blob's patch sites).
    ///
    /// Returns `(master, per-entry event index, per-entry patchable)`.
    /// The event-index table is the patch frontier's mapping; the
    /// patchable flag mirrors the constructor's session-split rule (the
    /// same pure function of `session_split` and the event kind — HB/EOS
    /// frames and second-session packets never change).
    ///
    /// SAFETY-of-lifetime (the EntryBuf contract, extended): the entries'
    /// slices are re-built at `'static` from raw parts pointing into this
    /// transport's blob and triple store — both outlive the pipeline (the
    /// RX thread owns this transport until the pipeline's Drop joins it),
    /// and the master is never dereferenced after the pipeline drops.
    /// ALLOC_DELTA=0: fixed-capacity, allocated here once.
    #[allow(clippy::disallowed_types, clippy::type_complexity)] // construction-time Vecs only
    pub(crate) fn build_frame_master(
        &self,
        compute_elig: bool,
    ) -> (
        Box<[nf_protocol::packet::FrameEntry<'static>]>,
        Box<[u32]>,
        Box<[bool]>,
    ) {
        use nf_protocol::packet::{FrameEntry, FrameMemo};
        let n = self.rendered_frame_count();
        let mut master: Vec<FrameEntry<'static>> = Vec::with_capacity(n);
        let mut evts: Vec<u32> = Vec::with_capacity(n);
        let mut patchable: Vec<bool> = Vec::with_capacity(n);
        // The construction bake (patch_sessions at construction) guarantees
        // the blob's frame lines carry the construction session — the
        // master's initial sess words read from the same authoritative
        // bytes the classic build reads at poll time.
        let tmpl_lo = u64::from_le_bytes([
            self.session[0],
            self.session[1],
            self.session[2],
            self.session[3],
            self.session[4],
            self.session[5],
            self.session[6],
            self.session[7],
        ]);
        let tmpl_hi = u64::from_le_bytes([
            self.session[2],
            self.session[3],
            self.session[4],
            self.session[5],
            self.session[6],
            self.session[7],
            self.session[8],
            self.session[9],
        ]);
        let tp = self.triples.as_ptr();
        for (evt, m) in self.meta.iter().enumerate() {
            if m.len == 0 {
                continue; // tombstone: never emitted, never in the master
            }
            let base = m.offset as usize;
            // SAFETY: `offset`/`len` are construction-valid (the same
            // contract poll_impl's frame slice relies on); the reads below
            // touch only [base, base + len).
            let fptr = unsafe { self.frames.as_ptr().add(base) };
            // SAFETY: as poll_impl's session reads — len >= HEADER_LEN for
            // every pushed frame; unaligned u64 reads are defined.
            let (sess_lo, sess_hi) = unsafe {
                (
                    (fptr as *const u64).read_unaligned(),
                    (fptr.add(2) as *const u64).read_unaligned(),
                )
            };
            // SAFETY: the slice targets (blob + triple store) outlive the
            // pipeline (the RX thread is joined in Drop) and are never
            // dereferenced after the pipeline drops — the master carries
            // the EntryBuf contract verbatim.
            let bytes: &'static [u8] =
                unsafe { std::slice::from_raw_parts(fptr, m.len as usize) };
            let blocks: &'static [(u64, u32, u32)] = if m.blk_count == 0 {
                &[]
            } else {
                unsafe {
                    std::slice::from_raw_parts(
                        tp.add(m.blk_base as usize),
                        m.blk_count as usize,
                    )
                }
            };
            // R12c: the elig byte — the exact poll-time formula (the parity
            // suite pins the master's fields to the classic build's).
            let elig_ok = (compute_elig
                && m.blk_count != 0
                && m.valid == m.blk_count
                && sess_lo == tmpl_lo
                && sess_hi == tmpl_hi) as u8;
            let ev = &self.schedule.events[evt];
            let patch = match self.schedule.session_split {
                Some((split_m, _)) => match ev.kind {
                    SchedKind::Packet { first_msg, .. } => first_msg < split_m,
                    SchedKind::Heartbeat { .. } | SchedKind::EndOfSession { .. } => false,
                },
                None => true,
            };
            master.push(FrameEntry {
                bytes,
                feed: m.feed,
                blocks,
                memo: (m.blk_count != 0).then_some(FrameMemo { valid_count: m.valid }),
                first_seq: m.first_seq,
                sess_lo,
                sess_hi,
                elig: (m.feed & 3) | (elig_ok << 7),
            });
            evts.push(evt as u32);
            patchable.push(patch);
        }
        debug_assert_eq!(master.len(), evts.len());
        debug_assert_eq!(master.len(), patchable.len());
        (
            master.into_boxed_slice(),
            evts.into_boxed_slice(),
            patchable.into_boxed_slice(),
        )
    }

    #[inline(always)]
    pub fn poll_clamped(&mut self, batch: &mut FrameBatch, max_vt: Option<u64>) -> usize {
        batch.clear();
        // F-1/F-2: the classic instantiation (MODE 0). The warm/rxbuild
        // context is compile-time dead at MODE 0 — the dummy below never
        // touches memory.
        let mut dummy = WarmEmitCtx {
            out: &mut [],
            warm: &mut [],
            idx: 0,
            fixes: 0,
            uncovered: 0,
            sess_lo_tmpl: 0,
            sess_hi_tmpl: 0,
            compute_elig: false,
        };
        self.poll_impl::<0>(batch, &mut dummy, max_vt)
    }

    /// F-1 (HFT_RXWARM): the warm twin of [`Self::poll_clamped`] — the
    /// SAME pacing skeleton (single-sourced in `poll_impl`; the pipeline
    /// parity suite pins the two instantiations to identical observables),
    /// with the per-frame emit replaced by the warm start: the entry is
    /// derived IN REGISTERS from the walk's own live facts (the frame
    /// meta + the frame line's session words — both loaded by the walk
    /// anyway), COMPARED against the warm array (the check IS the
    /// correctness — the rxdesc check-and-fix law), fixed in place on
    /// divergence, and the VERIFIED entry is stored into `ctx.out` — the
    /// mailbox store that replaces the classic slot push plus the
    /// accumulate-loop build (the scratch slot round trip is the removed
    /// cost; zero added store traffic). The legacy batch_event side table
    /// is not maintained (the pipelined consumer never reads it —
    /// batch_blocks/batch_memo are the single-threaded harness's paths).
    ///
    /// SAFETY-of-lifetime (the EntryBuf contract, pipeline.rs): the
    /// entries' slices are re-built at `'static` from raw parts pointing
    /// into this transport's blob and triple store — both outlive the
    /// pipeline (the RX thread owns this transport until the pipeline's
    /// Drop joins it) and the entries are never dereferenced after the
    /// consumer frees the buffer.
    #[inline]
    pub(crate) fn poll_warm(&mut self, batch: &mut FrameBatch, ctx: &mut WarmEmitCtx<'_>) -> usize {
        batch.clear();
        self.poll_impl::<1>(batch, ctx, None)
    }

    /// F-2 (HFT_RXBUILD): the publish-by-reference twin — the SAME pacing
    /// skeleton (single-sourced in `poll_impl`; the parity suite pins the
    /// instantiations to identical observables), with the per-frame work
    /// reduced to COUNTING: the entries live in the construction-built
    /// master array (see `build_frame_master`) and the publication carries
    /// only the master slice bounds (`ctx.idx` is the pass-local frame
    /// index — the walk advances it per emitted frame, exactly the warm
    /// counter's law). No frame line is read (the session reads and the
    /// blob slice disappear with the entry build — the RX per-frame cost
    /// is the pacing walk alone); the DLP prefetch stays (the fold's
    /// first lines).
    #[inline]
    pub(crate) fn poll_rxbuild(&mut self, batch: &mut FrameBatch, ctx: &mut WarmEmitCtx<'_>) -> usize {
        batch.clear();
        self.poll_impl::<2>(batch, ctx, None)
    }

    /// The shared poll skeleton (F-1/F-2): the pacing preamble, the event
    /// walk, the tombstone skip and the DLP prefetch block are
    /// SINGLE-SOURCED for all instantiations; only the per-frame EMIT
    /// differs (MODE 0 classic: the FrameBatch slot push + the legacy
    /// batch_event map; MODE 1 warm: the derive + check-and-fix +
    /// verified-entry store; MODE 2 rxbuild: count-only — the master's
    /// slice bounds are all the publication carries). The const parameter
    /// makes every branch compile-time — the classic instantiation's
    /// codegen is the pre-F-1 codegen.
    #[inline(always)]
    fn poll_impl<const MODE: u8>(
        &mut self,
        batch: &mut FrameBatch,
        w: &mut WarmEmitCtx<'_>,
        max_vt: Option<u64>,
    ) -> usize {
        let events_len = self.meta.len();
        if self.event_idx >= events_len {
            return 0;
        }

        let first_vt = self.vts[self.event_idx];
        // HOT: max_vt=None + clock_clamp=None (steady replay) — clamp is cold.
        let limit = match max_vt.or(self.clock_clamp) {
            Some(clamp) => {
                std::hint::cold_path();
                std::cmp::min(first_vt, clamp)
            }
            None => first_vt,
        };

        if limit > self.virtual_clock {
            self.virtual_clock = limit;
        }
        let vclock = self.virtual_clock;

        // R8: advance the group chain — `coalesce` groups (default 1). The
        // chain's `mx` is the max group-start vt released; the clock never
        // exceeds an explicit clamp (chain stops at a group start above the
        // limit, matching the pre-R8 jump-to-min semantics).
        let coalesce = self.coalesce;
        let mut e = self.event_idx;
        let mut mx = 0u64;
        let mut groups = 0usize;
        let limit_full = max_vt.or(self.clock_clamp).unwrap_or(u64::MAX);
        while groups < coalesce && e < events_len {
            let v = self.vts[e];
            if v > limit_full {
                break;
            }
            if v > mx {
                mx = v;
            }
            e = self.group_end[e] as usize;
            groups += 1;
        }
        if mx > vclock {
            self.virtual_clock = mx;
        }
        let vclock = self.virtual_clock;

        let cap = if MODE == 1 {
            w.out.len().min(FrameBatch::capacity())
        } else {
            FrameBatch::capacity()
        };
        // R8: the per-frame clock compare and the legacy batch_event map are
        // needed only in the exact-pacing mode (coalesce == 1, the
        // conformance/golden/differential configuration). In the coalesced
        // throughput mode the release boundary IS the group chain's `e`
        // (proved: every member of the chain's runs has vt <= mx <= vclock,
        // and the schedules the throughput arms render have non-decreasing
        // vts), so the loop bounds by `e` directly.
        let exact_pacing = self.coalesce == 1;
        let limit_evt = if exact_pacing { events_len } else { e };
        // F-1: the warm emit's frame counter (dead in the classic
        // instantiation; the classic bounds by batch.len()).
        let mut emitted = 0usize;
        let tp = self.triples.as_ptr();

        while self.event_idx < limit_evt && (if MODE != 0 { emitted } else { batch.len() }) < cap {
            let evt = self.event_idx;
            if exact_pacing && self.vts[evt] > vclock {
                break;
            }
            let m = self.meta[evt];
            self.event_idx = evt + 1;
            if m.len == 0 {
                continue; // tombstone: advances the cursor, never emitted
            }
            let base = m.offset as usize;
            let end = base + m.len as usize;
            // SAFETY: `offset`/`len` were written at construction from the
            // blob's own append cursor (off = blob.len() before
            // extend_from_slice, len = the rendered length), so base..end is
            // in-bounds of `frames` by construction; the debug assert keeps
            // the invariant honest under mutation-heavy test builds.
            debug_assert!(end <= self.frames.len());
            if MODE == 2 {
                // F-2: count-only — the entries live in the master (built
                // once at construction, patched at the session boundaries);
                // the publication carries the slice bounds. `w.idx` is the
                // pass-local frame index (the warm counter's law).
                w.idx += 1;
                emitted += 1;
            } else {
                let frame = unsafe { self.frames.get_unchecked_mut(base..end) };
                // R8: session compare words, read from the frame line this
                // thread already holds (the reset-time bake guarantees the
                // bytes). In pipelined mode the consumer's steady scan then
                // never touches the cross-core frame lines at all.
                let fptr = frame.as_ptr();
                // SAFETY: len >= HEADER_LEN (>= 20) for every pushed frame;
                // unaligned u64 reads are defined.
                let (sess_lo, sess_hi) = unsafe {
                    (
                        (fptr as *const u64).read_unaligned(),
                        (fptr.add(2) as *const u64).read_unaligned(),
                    )
                };
                if MODE == 1 {
                // F-1: derive the entry in registers from the walk's live
                // facts — the exact formula of the pipeline's classic
                // accumulate-loop build (the parity suite pins the two
                // instantiations together). SAFETY: as poll_warm's doc —
                // the target bytes outlive the pipeline and are never read
                // after the consumer frees the buffer.
                let bytes: &'static [u8] =
                    unsafe { std::slice::from_raw_parts(fptr, m.len as usize) };
                let blocks: &'static [(u64, u32, u32)] = if m.blk_count == 0 {
                    &[]
                } else {
                    // SAFETY: slot fields are construction-valid (the same
                    // contract as the classic build in pipeline.rs).
                    unsafe {
                        std::slice::from_raw_parts(
                            tp.add(m.blk_base as usize),
                            m.blk_count as usize,
                        )
                    }
                };
                // R12c: the elig byte's steady-ok bit (see FrameEntry::elig).
                let elig_ok = (w.compute_elig
                    && m.blk_count != 0
                    && m.valid == m.blk_count
                    && sess_lo == w.sess_lo_tmpl
                    && sess_hi == w.sess_hi_tmpl) as u8;
                let e = nf_protocol::packet::FrameEntry {
                    bytes,
                    feed: m.feed,
                    blocks,
                    memo: (m.blk_count != 0)
                        .then_some(nf_protocol::packet::FrameMemo { valid_count: m.valid }),
                    first_seq: m.first_seq,
                    sess_lo,
                    sess_hi,
                    elig: (m.feed & 3) | (elig_ok << 7),
                };
                // Check-and-fix (the rxdesc law): the compare IS the
                // correctness — the remembered entry is only trusted
                // because the walk's live derivation just proved it.
                if w.idx < w.warm.len() {
                    let j = w.idx;
                    if warm_entry_neq(&w.warm[j], &e) {
                        w.warm[j] = e;
                        w.fixes += 1;
                    }
                    w.out[emitted] = w.warm[j];
                } else {
                    w.out[emitted] = e;
                    w.uncovered += 1;
                }
                w.idx += 1;
                emitted += 1;
                } else {
                // R8: the session prefix was patched at reset() time — poll's
                // release loop is a pure slice + push (the 10B copy and its
                // branch are gone from the hot path).
                let slot_idx = batch.len();
                batch.push_indexed(
                    fptr,
                    m.len,
                    m.feed,
                    m.blk_base,
                    m.blk_count,
                    m.valid,
                    m.first_seq,
                    sess_lo,
                    sess_hi,
                );
                // Map batch position back to its event for the legacy
                // batch_blocks()/batch_memo() side tables (exact-pacing
                // callers only; the coalesced throughput arms consume the
                // inline slot index).
                if exact_pacing {
                    self.batch_event[slot_idx] = evt as u32;
                }
                }
            }
            // R4/R8: DLP warm-up — the blob is append-ordered, so the NEXT
            // event's frame starts exactly at base + len: its first line
            // (header + first body bytes — the session compare reads
            // [0..20] and the verification consumers stream from there) is
            // prefetchable from the CURRENT meta with zero extra loads.
            // Depth-2 covers the batch boundary via ONE directory load (the
            // hardware prefetcher carries the sequential meta/slot streams
            // and the scan no longer touches the triple store). The old
            // 4-deep lookahead reloaded metas 5x per frame; the coalesced
            // release gives every in-batch frame hundreds of cycles of lead
            // already.
            #[cfg(target_arch = "x86_64")]
            {
                let next = base + m.len as usize;
                // SAFETY: prefetcht0 never faults and accepts any readable
                // address; `next` is within the blob (or one past its end
                // for the final frame — an unmapped prefetch is dropped by
                // the hardware, and blob pages are over-allocated by the
                // allocator's page granularity in practice; the last event
                // is never a released frame's lookahead target in the
                // steady stream).
                unsafe {
                    if self.body_prefetch {
                        std::arch::x86_64::_mm_prefetch(
                            self.frames.as_ptr().add(next) as *const i8,
                            std::arch::x86_64::_MM_HINT_T0,
                        );
                    }
                }
                if evt + 1 < events_len {
                    let m2 = self.meta[evt + 1];
                    if m2.len > 0 && self.body_prefetch {
                        let next2 = next + m2.len as usize;
                        // SAFETY: same prefetch contract as above.
                        unsafe {
                            std::arch::x86_64::_mm_prefetch(
                                self.frames.as_ptr().add(next2) as *const i8,
                                std::arch::x86_64::_MM_HINT_T0,
                            );
                        }
                    }
                }
            }
        }

        self.batch_event_len = if MODE == 0 { batch.len() } else { 0 };
        if MODE != 0 {
            emitted
        } else {
            batch.len()
        }
    }

    /// Q1 indexed ingest: block triples `(seq, start, end)` for the frame at
    /// batch position `batch_pos` (positions from the most recent `poll()` on
    /// this transport). Empty for HB/EOS/tombstone frames and OOB positions —
    /// callers fall back to classic `ingest` on empty (same observables).
    /// Triples are relative to the frame bytes and session-patch-proof.
    #[inline(always)]
    pub fn batch_blocks(&self, batch_pos: usize) -> &[(u64, u32, u32)] {
        if batch_pos >= self.batch_event_len {
            std::hint::cold_path();
            return &[];
        }
        let ev = self.batch_event[batch_pos] as usize;
        if ev >= self.meta.len() {
            std::hint::cold_path();
            return &[];
        }
        let m = &self.meta[ev];
        let base = m.blk_base as usize;
        let end = base + m.blk_count as usize;
        if end > self.triples.len() {
            std::hint::cold_path();
            return &[];
        }
        &self.triples[base..end]
    }

    /// R8: the frames of the most recent `poll()` as `FrameEntry` values —
    /// bytes + feed + inline Q1/R2 index with ZERO side-table indirection
    /// (the slot itself carries blk_base/blk_count/valid). Feeds the
    /// sequencer's `ingest_batch` apply loop. The iterator borrows the
    /// transport immutably; the caller must finish it before the next poll
    /// (the borrow checker enforces this).
    #[inline(always)]
    pub fn batch_entries<'b>(
        &'b self,
        batch: &'b FrameBatch,
    ) -> impl Iterator<Item = nf_protocol::packet::FrameEntry<'b>> + 'b {
        batch.frames().iter().map(move |f| {
            let (blocks, memo) = self.frame_blocks_memo(f);
            nf_protocol::packet::FrameEntry {
                bytes: f.bytes(),
                feed: f.feed,
                blocks,
                memo,
                first_seq: f.first_seq,
                sess_lo: f.sess_lo,
                sess_hi: f.sess_hi,
                // R12c: the single-threaded path feeds the SCALAR steady
                // scan (no vector ladder), which ignores elig; the ok bit
                // stays cleared so a hypothetical ladder consumer would
                // safely fall back to the scalar ladder.
                elig: f.feed & 3,
            }
        })
    }

    /// R8: slot-direct per-frame index — one call replacing the
    /// `batch_blocks(pos)` + `batch_memo(pos)` side-table pair (which walked
    /// batch_event[pos] → meta[ev] → triples twice, with bounds checks at
    /// every hop). Reads the Q1/R2 index the slot itself carries.
    #[inline(always)]
    pub fn frame_blocks_memo(
        &self,
        f: &crate::FrameView,
    ) -> (&[(u64, u32, u32)], Option<FrameMemo>) {
        let (blk_base, blk_count, valid) = (f.blk_base, f.blk_count, f.valid);
        if blk_count == 0 {
            return (&[], None);
        }
        // SAFETY: batch slots are written only inside this crate —
        // `push_indexed` copies construction-valid blk_base/blk_count
        // (bounded by the triples store built at transport construction),
        // and `push_raw`/`FrameBatch::new` write 0/0, which the guard above
        // routes to the empty slice.
        unsafe {
            let tp = self.triples.as_ptr().add(blk_base as usize);
            (
                std::slice::from_raw_parts(tp, blk_count as usize),
                Some(FrameMemo { valid_count: valid }),
            )
        }
    }

    /// R2: validation-verdict memo for the frame at batch position `batch_pos`
    /// (positions from the most recent `poll()` on this transport). `None` for
    /// HB/EOS/tombstone frames, OOB positions, and frames with no block index —
    /// callers then run in-window validation (identical observables either way).
    /// The verdict is computed at construction from these exact immutable bytes
    /// (bodies untouched by session patch), so it holds for every reset().
    #[inline(always)]
    pub fn batch_memo(&self, batch_pos: usize) -> Option<FrameMemo> {
        if batch_pos >= self.batch_event_len {
            std::hint::cold_path();
            return None;
        }
        let ev = self.batch_event[batch_pos] as usize;
        if ev >= self.meta.len() {
            std::hint::cold_path();
            return None;
        }
        let m = &self.meta[ev];
        if m.blk_count == 0 {
            std::hint::cold_path();
            return None;
        }
        let base = m.blk_base as usize;
        let end = base + m.blk_count as usize;
        if end > self.triples.len() {
            std::hint::cold_path();
            return None;
        }
        Some(FrameMemo {
            valid_count: m.valid,
        })
    }
}

/// P3: always-inline — fuses session/header/payload copy into poll (saves call/frame).
#[inline(always)]
pub fn render_event_standalone(
    gt: &[u8],
    ev: &SchedEvent,
    sched: &ReplaySchedule,
    session: [u8; 10],
    slot: &mut [u8],
    cursor: &mut Cursor,
) -> Option<usize> {
    let effective_session = match sched.session_split {
        Some((split_m, next_sess)) => match ev.kind {
            SchedKind::Packet { first_msg, .. } if first_msg >= split_m => next_sess,
            SchedKind::Heartbeat { .. } | SchedKind::EndOfSession { .. } => next_sess,
            _ => session,
        },
        None => session,
    };

    if slot.len() < HEADER_LEN {
        return None;
    }
    slot[0..10].copy_from_slice(&effective_session);
    match ev.kind {
        SchedKind::Heartbeat { next_seq } => {
            slot[10..18].copy_from_slice(&next_seq.to_be_bytes());
            slot[18..20].copy_from_slice(&HEARTBEAT_COUNT.to_be_bytes());
            Some(HEADER_LEN)
        }
        SchedKind::EndOfSession { next_seq } => {
            slot[10..18].copy_from_slice(&next_seq.to_be_bytes());
            slot[18..20].copy_from_slice(&EOS_COUNT.to_be_bytes());
            Some(HEADER_LEN)
        }
        SchedKind::Packet {
            first_seq,
            first_msg,
            count,
        } => {
            slot[10..18].copy_from_slice(&first_seq.to_be_bytes());
            slot[18..20].copy_from_slice(&count.to_be_bytes());

            let start_pos = cursor.seek_msg(gt, first_msg)?;

            let mut cur_pos = start_pos;
            for _ in 0..count {
                if cur_pos + 2 > gt.len() {
                    return None;
                }
                let len = u16::from_be_bytes([gt[cur_pos], gt[cur_pos + 1]]) as usize;
                cur_pos += 2 + len;
            }

            let payload_len = cur_pos - start_pos;
            let total_len = HEADER_LEN + payload_len;
            if total_len > slot.len() || cur_pos > gt.len() {
                return None;
            }

            slot[HEADER_LEN..total_len].copy_from_slice(&gt[start_pos..cur_pos]);
            cursor.byte_offset = cur_pos;
            cursor.msg_index = first_msg + count as u64;

            Some(total_len)
        }
    }
}

impl Transport for ReplayTransport {
    #[inline(always)]
    fn poll(&mut self, batch: &mut FrameBatch) -> usize {
        self.poll_clamped(batch, None)
    }

    #[inline(always)]
    fn now_ns(&self) -> u64 {
        self.virtual_clock
    }
}

#[cfg(test)]
#[allow(clippy::all)]
mod tests {
    use super::*;

    /// Ground truth: [len|msg] chain of 12B System Event messages.
    fn gt_with(count: u64) -> Vec<u8> {
        #[allow(clippy::disallowed_types)]
        let mut gt = Vec::new();
        for i in 0..count {
            let mut msg = [b'S', 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, b'O'];
            msg[1..9].copy_from_slice(&(i + 1).to_be_bytes());
            gt.extend_from_slice(&(msg.len() as u16).to_be_bytes());
            gt.extend_from_slice(&msg);
        }
        gt
    }

    /// Ground truth whose 3rd message (index 2) has an unknown type byte.
    fn gt_with_bad_third() -> Vec<u8> {
        let mut gt = gt_with(5);
        // skip 2 messages (2 * (2 + 12) bytes), then patch the type byte
        let off = 2 * 14 + 2;
        gt[off] = 0xFE;
        gt
    }

    fn sched_packet(first_seq: u64, first_msg: u64, count: u16) -> ReplaySchedule {
        ReplaySchedule {
            events: vec![SchedEvent {
                release_vt: 0,
                feed: 0,
                kind: SchedKind::Packet {
                    first_seq,
                    first_msg,
                    count,
                },
            }],
            session_split: None,
        }
    }

    /// R2 verdict memo: all-valid frame memoizes valid_count == blk_count.
    #[test]
    fn t_r2_memo_all_valid() {
        let gt = gt_with(5);
        let sched = sched_packet(1, 0, 5);
        let mut t = ReplayTransport::new(&gt, sched, *b"TESTSESS01");
        let mut batch = FrameBatch::new();
        assert_eq!(t.poll(&mut batch), 1);
        let blocks = t.batch_blocks(0);
        assert_eq!(blocks.len(), 5);
        let memo = t.batch_memo(0).expect("memo present");
        assert_eq!(memo.valid_count, 5);
    }

    /// R2 verdict memo: exact leading prefix — invalid 3rd message => 2.
    #[test]
    fn t_r2_memo_exact_prefix() {
        let gt = gt_with_bad_third();
        let sched = sched_packet(1, 0, 5);
        let mut t = ReplayTransport::new(&gt, sched, *b"TESTSESS01");
        let mut batch = FrameBatch::new();
        assert_eq!(t.poll(&mut batch), 1);
        let blocks = t.batch_blocks(0);
        assert_eq!(blocks.len(), 5);
        let memo = t.batch_memo(0).expect("memo present");
        assert_eq!(memo.valid_count, 2);
        // Cross-check the memo against in-window validation of the same bytes
        // (delivered via the batch FrameView): blocks[0..2] valid, block 2 fails.
        let frame = batch.frames()[0].bytes();
        for b in &blocks[0..2] {
            assert!(itch5::validate(&frame[b.1 as usize..b.2 as usize]).is_ok());
        }
        assert_eq!(
            itch5::validate(&frame[blocks[2].1 as usize..blocks[2].2 as usize]),
            Err(nf_protocol::itch5::ItchError::UnknownType { t: 0xFE })
        );
    }

    /// R2: memo survives reset() with a patched session (bodies untouched).
    #[test]
    fn t_r2_memo_stable_across_reset() {
        let gt = gt_with(4);
        let sched = sched_packet(1, 0, 4);
        let mut t = ReplayTransport::new(&gt, sched, *b"TESTSESS01");
        let mut batch = FrameBatch::new();
        assert_eq!(t.poll(&mut batch), 1);
        let before = t.batch_memo(0).expect("memo present");
        t.reset(*b"OTHERSESS1");
        assert_eq!(t.poll(&mut batch), 1);
        let after = t.batch_memo(0).expect("memo present");
        assert_eq!(before, after);
    }

    /// R9: blob aliasing — a dual-feed schedule delivering the same message
    /// range on both feeds renders byte-identical frames, so the duplicate
    /// delivery must alias onto the primary's blob region (blob shrinks,
    /// `aliased_frame_count` reports it) while poll() serves byte-identical
    /// frames for BOTH deliveries, and a reset() re-bakes the one shared
    /// patch site so both deliveries carry the new session.
    #[test]
    fn t_r9_aliasing_dual_feed_identical_bytes_and_smaller_blob() {
        let gt = gt_with(8);
        // Feed A delivers msgs 0..4 (seq 1..5); feed B the SAME range.
        let sched = ReplaySchedule {
            events: vec![
                SchedEvent {
                    release_vt: 0,
                    feed: 0,
                    kind: SchedKind::Packet {
                        first_seq: 1,
                        first_msg: 0,
                        count: 4,
                    },
                },
                SchedEvent {
                    release_vt: 0,
                    feed: 1,
                    kind: SchedKind::Packet {
                        first_seq: 1,
                        first_msg: 0,
                        count: 4,
                    },
                },
            ],
            session_split: None,
        };
        let mut t = ReplayTransport::new(&gt, sched, *b"TESTSESS01");
        assert_eq!(t.aliased_frame_count(), 1, "dup delivery must alias");
        assert!(t.blob_aliasing());
        let mut batch = FrameBatch::new();
        assert_eq!(t.poll(&mut batch), 2);
        let f0 = batch.frames()[0].bytes().to_vec();
        let f1 = batch.frames()[1].bytes().to_vec();
        assert_eq!(f0, f1, "aliased deliveries serve identical bytes");
        // The blob is a private field; the observable proxy is that the two
        // FrameViews share a base pointer (same region, no second copy).
        assert_eq!(
            batch.frames()[0].bytes().as_ptr(),
            batch.frames()[1].bytes().as_ptr(),
            "aliased deliveries share the blob region"
        );
        // Both deliveries re-bake with the new session (one shared site).
        t.reset(*b"OTHERSESS1");
        let mut batch2 = FrameBatch::new();
        assert_eq!(t.poll(&mut batch2), 2);
        assert_eq!(batch2.frames()[0].bytes(), batch2.frames()[1].bytes());
        assert_eq!(batch2.frames()[0].bytes()[..10], *b"OTHERSESS1");
        // And the memo/triples of both deliveries remain equal.
        let b0 = t.batch_blocks(0);
        let b1 = t.batch_blocks(1);
        assert_eq!(b0.len(), b1.len());
        assert_eq!(b0, b1, "aliased deliveries share the triple range");
    }

    /// R9: aliasing must NOT trigger for deliveries of DIFFERENT message
    /// ranges (same first_seq, different first_msg) — the bytes differ, so
    /// both render independently.
    #[test]
    fn t_r9_aliasing_not_for_distinct_ranges() {
        let gt = gt_with(8);
        let sched = ReplaySchedule {
            events: vec![
                SchedEvent {
                    release_vt: 0,
                    feed: 0,
                    kind: SchedKind::Packet {
                        first_seq: 1,
                        first_msg: 0,
                        count: 4,
                    },
                },
                SchedEvent {
                    release_vt: 0,
                    feed: 1,
                    kind: SchedKind::Packet {
                        first_seq: 5,
                        first_msg: 4,
                        count: 4,
                    },
                },
            ],
            session_split: None,
        };
        let mut t = ReplayTransport::new(&gt, sched, *b"TESTSESS01");
        assert_eq!(
            t.aliased_frame_count(),
            0,
            "distinct message ranges must not alias"
        );
        assert!(!t.blob_aliasing());
        let mut batch = FrameBatch::new();
        assert_eq!(t.poll(&mut batch), 2);
        assert_ne!(
            batch.frames()[0].bytes().as_ptr(),
            batch.frames()[1].bytes().as_ptr()
        );
    }

    /// R2: HB/EOS carry no memo (None) and no blocks.
    #[test]
    fn t_r2_memo_none_for_hb() {
        let gt = gt_with(2);
        let sched = ReplaySchedule {
            events: vec![
                SchedEvent {
                    release_vt: 0,
                    feed: 0,
                    kind: SchedKind::Heartbeat { next_seq: 3 },
                },
                SchedEvent {
                    release_vt: 1,
                    feed: 0,
                    kind: SchedKind::EndOfSession { next_seq: 3 },
                },
            ],
            session_split: None,
        };
        let mut t = ReplayTransport::new(&gt, sched, *b"TESTSESS01");
        let mut batch = FrameBatch::new();
        assert_eq!(t.poll(&mut batch), 1); // HB
        assert!(t.batch_memo(0).is_none());
        assert_eq!(t.poll(&mut batch), 1); // EOS
        assert!(t.batch_memo(0).is_none());
    }

    /// R2: OOB batch positions memoize None (defensive fallback).
    #[test]
    fn t_r2_memo_none_oob() {
        let gt = gt_with(2);
        let sched = sched_packet(1, 0, 2);
        let mut t = ReplayTransport::new(&gt, sched, *b"TESTSESS01");
        let mut batch = FrameBatch::new();
        assert_eq!(t.poll(&mut batch), 1);
        assert!(t.batch_memo(7).is_none());
    }

    /// R2 default Transport trait impl: batch_memo() is None.
    struct NoIndexTransport;
    impl Transport for NoIndexTransport {
        fn poll(&mut self, _batch: &mut FrameBatch) -> usize {
            0
        }
        fn now_ns(&self) -> u64 {
            0
        }
    }

    #[test]
    fn t_r2_memo_default_trait_none() {
        let t = NoIndexTransport;
        assert!(t.batch_memo(0).is_none());
        assert!(t.batch_blocks(0).is_empty());
    }

    /// R8: the inline slot index (batch_entries) must agree field-for-field
    /// with the legacy side-table API (batch_blocks/batch_memo) on every
    /// position of every poll — the two views of the same per-frame index
    /// can never diverge, or the batch apply loop would see different data
    /// than the classic per-frame path.
    #[test]
    fn t_r8_batch_entries_match_side_tables() {
        let gt = gt_with(40);
        let sched = sched_packet(1, 0, 40);
        let mut t = ReplayTransport::new(&gt, sched, *b"TESTSESS01");
        let mut batch = FrameBatch::new();
        while t.poll(&mut batch) > 0 {
            let entries: Vec<_> = t.batch_entries(&batch).collect();
            assert_eq!(entries.len(), batch.len());
            for (pos, e) in entries.iter().enumerate() {
                assert_eq!(e.blocks, t.batch_blocks(pos), "slot/side-table blocks diverge");
                assert_eq!(e.memo, t.batch_memo(pos), "slot/side-table memo diverge");
                assert_eq!(e.feed, batch.frames()[pos].feed);
                assert_eq!(e.bytes.len(), batch.frames()[pos].len as usize);
                assert_eq!(e.bytes.as_ptr(), batch.frames()[pos].bytes().as_ptr());
            }
        }
    }
}

impl ReplayTransport {
    /// R8: the shared block-triple store — the RX-pipelined consumer builds
    /// `FrameEntry::blocks` slices from it cross-thread (see pipeline.rs).
    /// Immutable after construction.
    #[inline]
    #[allow(clippy::disallowed_types)]
    pub fn shared_triples(&self) -> Arc<[(u64, u32, u32)]> {
        Arc::clone(&self.triples)
    }
}
