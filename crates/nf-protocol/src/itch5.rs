//! ITCH 5.0 payload layer. Hot path uses ONLY LENGTH + validate() (OP-1).
//! Overlays are cold-path borrowed views; zero allocation anywhere.

/// Total wire length per type byte; 0 = unknown. Sentinel safe: minimum
/// legal message is 12B.
pub const LENGTH: [u8; 256] = {
    let mut t = [0u8; 256];
    t[b'S' as usize] = 12; // System Event
    t[b'R' as usize] = 39; // Stock Directory
    t[b'H' as usize] = 25; // Stock Trading Action
    t[b'Y' as usize] = 20; // Reg SHO Restriction
    t[b'L' as usize] = 26; // Market Participant Position
    t[b'V' as usize] = 35; // MWCB Decline Level
    t[b'W' as usize] = 12; // MWCB Status
    t[b'K' as usize] = 28; // IPO Quoting Period
    t[b'J' as usize] = 35; // LULD Auction Collar
    t[b'h' as usize] = 21; // Operational Halt (lowercase)
    t[b'A' as usize] = 36; // Add Order (no MPID)
    t[b'F' as usize] = 40; // Add Order (MPID)
    t[b'E' as usize] = 31; // Order Executed
    t[b'C' as usize] = 36; // Order Executed w/ Price
    t[b'X' as usize] = 23; // Order Cancel
    t[b'D' as usize] = 19; // Order Delete
    t[b'U' as usize] = 35; // Order Replace
    t[b'P' as usize] = 44; // Trade (non-cross)
    t[b'Q' as usize] = 40; // Cross Trade
    t[b'B' as usize] = 19; // Broken Trade
    t[b'I' as usize] = 50; // NOII (max ITCH length)
    t[b'N' as usize] = 20; // Retail Price Improvement
    t
};

#[inline]
pub fn msg_len(type_byte: u8) -> Option<u8> {
    match LENGTH[type_byte as usize] {
        0 => None,
        l => Some(l),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItchError {
    Empty,
    UnknownType { t: u8 },
    LengthMismatch { expected: u8, got: usize },
}

/// O(1): one load, two compares. The ONLY ITCH call on the hot path.
#[inline]
pub fn validate(msg: &[u8]) -> Result<(), ItchError> {
    if msg.is_empty() {
        return Err(ItchError::Empty);
    }
    let expected = LENGTH[msg[0] as usize];
    if expected == 0 {
        return Err(ItchError::UnknownType { t: msg[0] });
    }
    if msg.len() != expected as usize {
        return Err(ItchError::LengthMismatch {
            expected,
            got: msg.len(),
        });
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommonHeader {
    pub locate: u16,
    pub tracking: u16,
    pub timestamp_ns: u64,
}

/// Infallible for validated messages (all known types >= 12B).
#[inline]
pub fn header(msg: &[u8]) -> CommonHeader {
    debug_assert!(msg.len() >= 11);
    let locate = u16::from_be_bytes([msg[1], msg[2]]);
    let tracking = u16::from_be_bytes([msg[3], msg[4]]);
    let timestamp_ns = ((msg[5] as u64) << 40)
        | ((msg[6] as u64) << 32)
        | ((msg[7] as u64) << 24)
        | ((msg[8] as u64) << 16)
        | ((msg[9] as u64) << 8)
        | (msg[10] as u64);
    CommonHeader {
        locate,
        tracking,
        timestamp_ns,
    }
}

// ── Overlays (cold path / borrowed views) ──────────────────────

pub struct SystemEvent<'a> {
    msg: &'a [u8],
}

impl<'a> SystemEvent<'a> {
    #[inline]
    pub fn parse(msg: &'a [u8]) -> Result<Self, ItchError> {
        if msg.len() != 12 || msg.first() != Some(&b'S') {
            validate(msg)?;
            return Err(ItchError::LengthMismatch {
                expected: 12,
                got: msg.len(),
            });
        }
        Ok(Self { msg })
    }

    #[inline]
    pub fn header(&self) -> CommonHeader {
        header(self.msg)
    }

    #[inline]
    pub fn event_code(&self) -> u8 {
        self.msg[11]
    }
}

pub struct StockDirectory<'a> {
    msg: &'a [u8],
}

impl<'a> StockDirectory<'a> {
    #[inline]
    pub fn parse(msg: &'a [u8]) -> Result<Self, ItchError> {
        if msg.len() != 39 || msg.first() != Some(&b'R') {
            validate(msg)?;
            return Err(ItchError::LengthMismatch {
                expected: 39,
                got: msg.len(),
            });
        }
        Ok(Self { msg })
    }

    #[inline]
    pub fn header(&self) -> CommonHeader {
        header(self.msg)
    }

    #[inline]
    pub fn stock(&self) -> &'a [u8] {
        &self.msg[11..19]
    }

    #[inline]
    pub fn market_category(&self) -> u8 {
        self.msg[19]
    }

    #[inline]
    pub fn financial_status(&self) -> u8 {
        self.msg[20]
    }

    #[inline]
    pub fn round_lot_size(&self) -> u32 {
        u32::from_be_bytes([self.msg[21], self.msg[22], self.msg[23], self.msg[24]])
    }

    #[inline]
    pub fn round_lots_only(&self) -> u8 {
        self.msg[25]
    }

    #[inline]
    pub fn issue_classification(&self) -> u8 {
        self.msg[26]
    }

    #[inline]
    pub fn issue_sub_type(&self) -> &'a [u8] {
        &self.msg[27..29]
    }

    #[inline]
    pub fn authenticity(&self) -> u8 {
        self.msg[29]
    }

    #[inline]
    pub fn short_sale_threshold(&self) -> u8 {
        self.msg[30]
    }

    #[inline]
    pub fn ipo_flag(&self) -> u8 {
        self.msg[31]
    }

    #[inline]
    pub fn luld_tier(&self) -> u8 {
        self.msg[32]
    }

    #[inline]
    pub fn etp_flag(&self) -> u8 {
        self.msg[33]
    }

    #[inline]
    pub fn etp_leverage(&self) -> u32 {
        u32::from_be_bytes([self.msg[34], self.msg[35], self.msg[36], self.msg[37]])
    }

    #[inline]
    pub fn inverse_indicator(&self) -> u8 {
        self.msg[38]
    }
}

pub struct Add<'a> {
    msg: &'a [u8],
}

impl<'a> Add<'a> {
    #[inline]
    pub fn parse(msg: &'a [u8]) -> Result<Self, ItchError> {
        if msg.len() != 36 || msg.first() != Some(&b'A') {
            validate(msg)?;
            return Err(ItchError::LengthMismatch {
                expected: 36,
                got: msg.len(),
            });
        }
        Ok(Self { msg })
    }

    #[inline]
    pub fn header(&self) -> CommonHeader {
        header(self.msg)
    }

    #[inline]
    pub fn order_ref(&self) -> u64 {
        u64::from_be_bytes([
            self.msg[11],
            self.msg[12],
            self.msg[13],
            self.msg[14],
            self.msg[15],
            self.msg[16],
            self.msg[17],
            self.msg[18],
        ])
    }

    #[inline]
    pub fn side(&self) -> u8 {
        self.msg[19]
    }

    #[inline]
    pub fn shares(&self) -> u32 {
        u32::from_be_bytes([self.msg[20], self.msg[21], self.msg[22], self.msg[23]])
    }

    #[inline]
    pub fn stock(&self) -> &'a [u8] {
        &self.msg[24..32]
    }

    #[inline]
    pub fn price_raw(&self) -> u32 {
        u32::from_be_bytes([self.msg[32], self.msg[33], self.msg[34], self.msg[35]])
    }
}

pub struct AddAttributed<'a> {
    msg: &'a [u8],
}

impl<'a> AddAttributed<'a> {
    #[inline]
    pub fn parse(msg: &'a [u8]) -> Result<Self, ItchError> {
        if msg.len() != 40 || msg.first() != Some(&b'F') {
            validate(msg)?;
            return Err(ItchError::LengthMismatch {
                expected: 40,
                got: msg.len(),
            });
        }
        Ok(Self { msg })
    }

    #[inline]
    pub fn header(&self) -> CommonHeader {
        header(self.msg)
    }

    #[inline]
    pub fn order_ref(&self) -> u64 {
        u64::from_be_bytes([
            self.msg[11],
            self.msg[12],
            self.msg[13],
            self.msg[14],
            self.msg[15],
            self.msg[16],
            self.msg[17],
            self.msg[18],
        ])
    }

    #[inline]
    pub fn side(&self) -> u8 {
        self.msg[19]
    }

    #[inline]
    pub fn shares(&self) -> u32 {
        u32::from_be_bytes([self.msg[20], self.msg[21], self.msg[22], self.msg[23]])
    }

    #[inline]
    pub fn stock(&self) -> &'a [u8] {
        &self.msg[24..32]
    }

    #[inline]
    pub fn price_raw(&self) -> u32 {
        u32::from_be_bytes([self.msg[32], self.msg[33], self.msg[34], self.msg[35]])
    }

    #[inline]
    pub fn mpid(&self) -> &'a [u8] {
        &self.msg[36..40]
    }
}

pub struct Executed<'a> {
    msg: &'a [u8],
}

impl<'a> Executed<'a> {
    #[inline]
    pub fn parse(msg: &'a [u8]) -> Result<Self, ItchError> {
        if msg.len() != 31 || msg.first() != Some(&b'E') {
            validate(msg)?;
            return Err(ItchError::LengthMismatch {
                expected: 31,
                got: msg.len(),
            });
        }
        Ok(Self { msg })
    }

    #[inline]
    pub fn header(&self) -> CommonHeader {
        header(self.msg)
    }

    #[inline]
    pub fn order_ref(&self) -> u64 {
        u64::from_be_bytes([
            self.msg[11],
            self.msg[12],
            self.msg[13],
            self.msg[14],
            self.msg[15],
            self.msg[16],
            self.msg[17],
            self.msg[18],
        ])
    }

    #[inline]
    pub fn executed_shares(&self) -> u32 {
        u32::from_be_bytes([self.msg[19], self.msg[20], self.msg[21], self.msg[22]])
    }

    #[inline]
    pub fn match_number(&self) -> u64 {
        u64::from_be_bytes([
            self.msg[23],
            self.msg[24],
            self.msg[25],
            self.msg[26],
            self.msg[27],
            self.msg[28],
            self.msg[29],
            self.msg[30],
        ])
    }
}

pub struct ExecutedWithPrice<'a> {
    msg: &'a [u8],
}

impl<'a> ExecutedWithPrice<'a> {
    #[inline]
    pub fn parse(msg: &'a [u8]) -> Result<Self, ItchError> {
        if msg.len() != 36 || msg.first() != Some(&b'C') {
            validate(msg)?;
            return Err(ItchError::LengthMismatch {
                expected: 36,
                got: msg.len(),
            });
        }
        Ok(Self { msg })
    }

    #[inline]
    pub fn header(&self) -> CommonHeader {
        header(self.msg)
    }

    #[inline]
    pub fn order_ref(&self) -> u64 {
        u64::from_be_bytes([
            self.msg[11],
            self.msg[12],
            self.msg[13],
            self.msg[14],
            self.msg[15],
            self.msg[16],
            self.msg[17],
            self.msg[18],
        ])
    }

    #[inline]
    pub fn executed_shares(&self) -> u32 {
        u32::from_be_bytes([self.msg[19], self.msg[20], self.msg[21], self.msg[22]])
    }

    #[inline]
    pub fn match_number(&self) -> u64 {
        u64::from_be_bytes([
            self.msg[23],
            self.msg[24],
            self.msg[25],
            self.msg[26],
            self.msg[27],
            self.msg[28],
            self.msg[29],
            self.msg[30],
        ])
    }

    #[inline]
    pub fn printable(&self) -> u8 {
        self.msg[31]
    }

    #[inline]
    pub fn price_raw(&self) -> u32 {
        u32::from_be_bytes([self.msg[32], self.msg[33], self.msg[34], self.msg[35]])
    }
}

pub struct Cancel<'a> {
    msg: &'a [u8],
}

impl<'a> Cancel<'a> {
    #[inline]
    pub fn parse(msg: &'a [u8]) -> Result<Self, ItchError> {
        if msg.len() != 23 || msg.first() != Some(&b'X') {
            validate(msg)?;
            return Err(ItchError::LengthMismatch {
                expected: 23,
                got: msg.len(),
            });
        }
        Ok(Self { msg })
    }

    #[inline]
    pub fn header(&self) -> CommonHeader {
        header(self.msg)
    }

    #[inline]
    pub fn order_ref(&self) -> u64 {
        u64::from_be_bytes([
            self.msg[11],
            self.msg[12],
            self.msg[13],
            self.msg[14],
            self.msg[15],
            self.msg[16],
            self.msg[17],
            self.msg[18],
        ])
    }

    #[inline]
    pub fn canceled_shares(&self) -> u32 {
        u32::from_be_bytes([self.msg[19], self.msg[20], self.msg[21], self.msg[22]])
    }
}

pub struct Delete<'a> {
    msg: &'a [u8],
}

impl<'a> Delete<'a> {
    #[inline]
    pub fn parse(msg: &'a [u8]) -> Result<Self, ItchError> {
        if msg.len() != 19 || msg.first() != Some(&b'D') {
            validate(msg)?;
            return Err(ItchError::LengthMismatch {
                expected: 19,
                got: msg.len(),
            });
        }
        Ok(Self { msg })
    }

    #[inline]
    pub fn header(&self) -> CommonHeader {
        header(self.msg)
    }

    #[inline]
    pub fn order_ref(&self) -> u64 {
        u64::from_be_bytes([
            self.msg[11],
            self.msg[12],
            self.msg[13],
            self.msg[14],
            self.msg[15],
            self.msg[16],
            self.msg[17],
            self.msg[18],
        ])
    }
}

pub struct Replace<'a> {
    msg: &'a [u8],
}

impl<'a> Replace<'a> {
    #[inline]
    pub fn parse(msg: &'a [u8]) -> Result<Self, ItchError> {
        if msg.len() != 35 || msg.first() != Some(&b'U') {
            validate(msg)?;
            return Err(ItchError::LengthMismatch {
                expected: 35,
                got: msg.len(),
            });
        }
        Ok(Self { msg })
    }

    #[inline]
    pub fn header(&self) -> CommonHeader {
        header(self.msg)
    }

    #[inline]
    pub fn orig_order_ref(&self) -> u64 {
        u64::from_be_bytes([
            self.msg[11],
            self.msg[12],
            self.msg[13],
            self.msg[14],
            self.msg[15],
            self.msg[16],
            self.msg[17],
            self.msg[18],
        ])
    }

    #[inline]
    pub fn new_order_ref(&self) -> u64 {
        u64::from_be_bytes([
            self.msg[19],
            self.msg[20],
            self.msg[21],
            self.msg[22],
            self.msg[23],
            self.msg[24],
            self.msg[25],
            self.msg[26],
        ])
    }

    #[inline]
    pub fn shares(&self) -> u32 {
        u32::from_be_bytes([self.msg[27], self.msg[28], self.msg[29], self.msg[30]])
    }

    #[inline]
    pub fn price_raw(&self) -> u32 {
        u32::from_be_bytes([self.msg[31], self.msg[32], self.msg[33], self.msg[34]])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TV_IT1: [u8; 12] = [
        0x53, 0x00, 0x00, 0x00, 0x00, 0x0A, 0x2D, 0xF4, 0x92, 0x1D, 0x67, 0x4F,
    ];

    const TV_IT2: [u8; 39] = [
        0x52, 0x00, 0x01, 0x00, 0x00, 0x0A, 0x66, 0xA0, 0xE0, 0xDC, 0x44, 0x41, 0x20, 0x20, 0x20,
        0x20, 0x20, 0x20, 0x20, 0x4E, 0x20, 0x00, 0x00, 0x00, 0x64, 0x4E, 0x43, 0x5A, 0x20, 0x50,
        0x4E, 0x20, 0x31, 0x4E, 0x00, 0x00, 0x00, 0x00, 0x4E,
    ];

    #[test]
    fn t1_tv_it1_system_event() {
        validate(&TV_IT1).expect("validate TV-IT1");
        let ev = SystemEvent::parse(&TV_IT1).expect("parse SystemEvent");
        let hdr = ev.header();
        assert_eq!(hdr.locate, 0);
        assert_eq!(hdr.tracking, 0);
        assert_eq!(hdr.timestamp_ns, 11_192_493_022_567);
        assert_eq!(ev.event_code(), b'O');
    }

    #[test]
    fn t2_tv_it2_stock_directory() {
        validate(&TV_IT2).expect("validate TV-IT2");
        let dir = StockDirectory::parse(&TV_IT2).expect("parse StockDirectory");
        let hdr = dir.header();
        assert_eq!(hdr.locate, 1);
        assert_eq!(hdr.tracking, 0);
        assert_eq!(hdr.timestamp_ns, 11_435_902_032_964);
        assert_eq!(dir.stock(), b"A       ");
        assert_eq!(dir.market_category(), b'N');
        assert_eq!(dir.financial_status(), b' ');
        assert_eq!(dir.round_lot_size(), 100);
        assert_eq!(dir.round_lots_only(), b'N');
        assert_eq!(dir.issue_classification(), b'C');
        assert_eq!(dir.issue_sub_type(), b"Z ");
        assert_eq!(dir.authenticity(), b'P');
        assert_eq!(dir.short_sale_threshold(), b'N');
        assert_eq!(dir.ipo_flag(), b' ');
        assert_eq!(dir.luld_tier(), b'1');
        assert_eq!(dir.etp_flag(), b'N');
        assert_eq!(dir.etp_leverage(), 0);
        assert_eq!(dir.inverse_indicator(), b'N');
    }

    #[test]
    fn t3_case_trap() {
        assert_eq!(LENGTH[b'H' as usize], 25);
        assert_eq!(LENGTH[b'h' as usize], 21);

        let mut h_msg = vec![0u8; 25];
        h_msg[0] = b'H';
        assert!(validate(&h_msg).is_ok());

        // wrong len for H
        h_msg.truncate(21);
        assert_eq!(
            validate(&h_msg),
            Err(ItchError::LengthMismatch {
                expected: 25,
                got: 21
            })
        );

        let mut lower_h = vec![0u8; 21];
        lower_h[0] = b'h';
        assert!(validate(&lower_h).is_ok());

        // wrong len for h
        lower_h.resize(25, 0);
        assert_eq!(
            validate(&lower_h),
            Err(ItchError::LengthMismatch {
                expected: 21,
                got: 25
            })
        );
    }

    #[test]
    fn t4_synthetic_add_order() {
        let mut msg = [0u8; 36];
        msg[0] = b'A';
        msg[1..3].copy_from_slice(&10u16.to_be_bytes()); // locate
        msg[3..5].copy_from_slice(&0u16.to_be_bytes()); // tracking
        msg[5..11].copy_from_slice(&[0, 0, 1, 2, 3, 4]); // timestamp
        msg[11..19].copy_from_slice(&42u64.to_be_bytes()); // order ref
        msg[19] = b'B'; // side
        msg[20..24].copy_from_slice(&100u32.to_be_bytes()); // shares
        msg[24..32].copy_from_slice(b"AAPL    "); // stock
        msg[32..36].copy_from_slice(&1_234_500u32.to_be_bytes()); // price 123.4500

        validate(&msg).expect("validate Add");
        let add = Add::parse(&msg).expect("parse Add");
        assert_eq!(add.header().locate, 10);
        assert_eq!(add.order_ref(), 42);
        assert_eq!(add.side(), b'B');
        assert_eq!(add.shares(), 100);
        assert_eq!(add.stock(), b"AAPL    ");
        assert_eq!(add.price_raw(), 1_234_500);
    }

    #[test]
    fn t5_table_totality() {
        for b in 0..=255u8 {
            let direct = LENGTH[b as usize];
            let helper = msg_len(b);
            if direct == 0 {
                assert_eq!(helper, None);
            } else {
                assert_eq!(helper, Some(direct));
            }
        }
    }

    #[test]
    fn t6_error_paths() {
        assert_eq!(validate(&[]), Err(ItchError::Empty));
        assert_eq!(validate(&[0x00]), Err(ItchError::UnknownType { t: 0x00 }));
        assert_eq!(validate(&[0xFF]), Err(ItchError::UnknownType { t: 0xFF }));
        assert_eq!(
            validate(&[b'S', 0, 0]),
            Err(ItchError::LengthMismatch {
                expected: 12,
                got: 3
            })
        );
    }

    #[test]
    fn t7_extended_overlays() {
        // 'F' AddAttributed (40B)
        let mut f_msg = [0u8; 40];
        f_msg[0] = b'F';
        f_msg[11..19].copy_from_slice(&101u64.to_be_bytes());
        f_msg[19] = b'S';
        f_msg[20..24].copy_from_slice(&500u32.to_be_bytes());
        f_msg[24..32].copy_from_slice(b"MSFT    ");
        f_msg[32..36].copy_from_slice(&250_0000u32.to_be_bytes());
        f_msg[36..40].copy_from_slice(b"GSCO");
        let f = AddAttributed::parse(&f_msg).expect("parse AddAttributed");
        assert_eq!(f.order_ref(), 101);
        assert_eq!(f.side(), b'S');
        assert_eq!(f.shares(), 500);
        assert_eq!(f.stock(), b"MSFT    ");
        assert_eq!(f.price_raw(), 250_0000);
        assert_eq!(f.mpid(), b"GSCO");

        // 'E' Executed (31B)
        let mut e_msg = [0u8; 31];
        e_msg[0] = b'E';
        e_msg[11..19].copy_from_slice(&101u64.to_be_bytes());
        e_msg[19..23].copy_from_slice(&200u32.to_be_bytes());
        e_msg[23..31].copy_from_slice(&999999u64.to_be_bytes());
        let e = Executed::parse(&e_msg).expect("parse Executed");
        assert_eq!(e.order_ref(), 101);
        assert_eq!(e.executed_shares(), 200);
        assert_eq!(e.match_number(), 999999);

        // 'C' ExecutedWithPrice (36B)
        let mut c_msg = [0u8; 36];
        c_msg[0] = b'C';
        c_msg[11..19].copy_from_slice(&101u64.to_be_bytes());
        c_msg[19..23].copy_from_slice(&200u32.to_be_bytes());
        c_msg[23..31].copy_from_slice(&999999u64.to_be_bytes());
        c_msg[31] = b'Y';
        c_msg[32..36].copy_from_slice(&251_0000u32.to_be_bytes());
        let c = ExecutedWithPrice::parse(&c_msg).expect("parse ExecutedWithPrice");
        assert_eq!(c.order_ref(), 101);
        assert_eq!(c.executed_shares(), 200);
        assert_eq!(c.match_number(), 999999);
        assert_eq!(c.printable(), b'Y');
        assert_eq!(c.price_raw(), 251_0000);

        // 'X' Cancel (23B)
        let mut x_msg = [0u8; 23];
        x_msg[0] = b'X';
        x_msg[11..19].copy_from_slice(&101u64.to_be_bytes());
        x_msg[19..23].copy_from_slice(&100u32.to_be_bytes());
        let x = Cancel::parse(&x_msg).expect("parse Cancel");
        assert_eq!(x.order_ref(), 101);
        assert_eq!(x.canceled_shares(), 100);

        // 'D' Delete (19B)
        let mut d_msg = [0u8; 19];
        d_msg[0] = b'D';
        d_msg[11..19].copy_from_slice(&101u64.to_be_bytes());
        let d = Delete::parse(&d_msg).expect("parse Delete");
        assert_eq!(d.order_ref(), 101);

        // 'U' Replace (35B)
        let mut u_msg = [0u8; 35];
        u_msg[0] = b'U';
        u_msg[11..19].copy_from_slice(&101u64.to_be_bytes());
        u_msg[19..27].copy_from_slice(&102u64.to_be_bytes());
        u_msg[27..31].copy_from_slice(&300u32.to_be_bytes());
        u_msg[31..35].copy_from_slice(&252_0000u32.to_be_bytes());
        let u = Replace::parse(&u_msg).expect("parse Replace");
        assert_eq!(u.orig_order_ref(), 101);
        assert_eq!(u.new_order_ref(), 102);
        assert_eq!(u.shares(), 300);
        assert_eq!(u.price_raw(), 252_0000);
    }
}
