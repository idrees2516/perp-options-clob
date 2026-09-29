//! Compact binary codec for [`Command`] (G-24's on-disk format).
//!
//! Design (the classic deterministic-log layout, e.g. Kafka / LevelDB
//! record batches):
//!
//! * **Varints** for every integer (LEB128) — small ids and timestamps
//!   cost one to three bytes.
//! * **Length-prefixed strings and vectors** (varint lengths).
//! * **Tagged variants**: one byte per enum, documented per encoder.
//! * **Framing** lives in [`crate::wal`]; this module is pure
//!   encode/decode with no I/O.
//!
//! The codec is total: every byte string either decodes to exactly one
//! command and consumes the whole buffer, or fails. Partial decodes are
//! impossible by construction, which is what makes torn-tail recovery
//! safe.

use poc_core::{
    AmericanParams, EverlastingParams, ExerciseStyle, FundingParams, Instrument, OptionKind,
    OptionMarginParams, OptionMarket, OptionVariant, OrderType, PerpMarket, SelfTradePrevention,
    Side, Symbol, TimeInForce, TimestampMs,
};
use poc_engine::{Command, OrderRequest, RfqLegCommand};

/// Encode an instrument (the genesis-frame payload).
#[must_use]
pub fn encode_instrument(instr: &Instrument) -> Vec<u8> {
    let mut e = Encoder::new();
    match instr {
        Instrument::Perp(m) => {
            e.varint(0);
            encode_perp(&mut e, m);
        }
        Instrument::Option(m) => {
            e.varint(1);
            encode_option(&mut e, m);
        }
    }
    e.into_vec()
}

fn encode_perp(e: &mut Encoder, m: &PerpMarket) {
    e.str(&m.symbol);
    e.str(&m.base_symbol);
    e.varint(u128::from(m.quote_decimals));
    e.varint(u128::from(m.base_decimals));
    e.varint(m.tick_size_quote_minor);
    e.varint(m.lot_size_base_minor);
    e.varint(u128::from(m.initial_margin_ratio_bps));
    e.varint(u128::from(m.maintenance_margin_ratio_bps));
    e.varint(u128::from(m.price_band_bps));
    e.varint(u128::from(m.max_order_lots));
    e.varint(u128::from(m.funding.interval_ms));
    e.svarint(m.funding.interest_rate_bps_per_interval);
    e.varint(u128::from(m.funding.premium_clamp_bps));
    e.varint(u128::from(m.funding.rate_cap_bps));
}

fn encode_option(e: &mut Encoder, m: &OptionMarket) {
    e.str(&m.symbol);
    e.str(&m.base_symbol);
    e.varint(match m.kind {
        OptionKind::Call => 0,
        OptionKind::Put => 1,
    });
    e.varint(m.strike_quote_minor);
    e.varint(u128::from(m.expiry_ts_ms));
    e.varint(match m.variant {
        OptionVariant::Dated => 0,
        OptionVariant::Everlasting => 1,
    });
    e.varint(u128::from(m.everlasting.interval_ms));
    e.varint(u128::from(m.everlasting.maturity_multiple));
    e.varint(u128::from(m.quote_decimals));
    e.varint(u128::from(m.base_decimals));
    e.varint(m.tick_size_quote_minor);
    e.varint(m.lot_size_base_minor);
    e.varint(u128::from(m.price_band_bps));
    e.varint(u128::from(m.max_order_lots));
    e.varint(u128::from(m.margin.short_option_min_bps));
    e.varint(u128::from(m.margin.liquidation_fee_bps));
    e.varint(match m.exercise_style {
        ExerciseStyle::European => 0,
        ExerciseStyle::American => 1,
    });
    e.varint(u128::from(m.american.settlement_twap_ms));
    e.varint(u128::from(m.american.exercise_fee_bps));
}

/// Decode an instrument (must consume the whole buffer).
pub fn decode_instrument(data: &[u8]) -> Result<Instrument, DecodeError> {
    let mut d = Decoder::new(data);
    let tag = d.varint()?;
    let instr = match tag {
        0 => Instrument::Perp(decode_perp(&mut d)?),
        1 => Instrument::Option(decode_option(&mut d)?),
        other => return Err(DecodeError::UnknownTag(other as u8)),
    };
    if !d.is_empty() {
        return Err(DecodeError::Malformed);
    }
    Ok(instr)
}

fn decode_perp(d: &mut Decoder<'_>) -> Result<PerpMarket, DecodeError> {
    let symbol = d.str()?;
    let base_symbol = d.str()?;
    let quote_decimals = u32::try_from(d.varint()?).unwrap_or(0);
    let base_decimals = u32::try_from(d.varint()?).unwrap_or(0);
    let tick_size_quote_minor = d.varint()?;
    let lot_size_base_minor = d.varint()?;
    let initial_margin_ratio_bps = d.u64()?;
    let maintenance_margin_ratio_bps = d.u64()?;
    let price_band_bps = d.u64()?;
    let max_order_lots = d.u64()?;
    let interval_ms = d.u64()?;
    let interest_rate_bps_per_interval = d.i64()?;
    let premium_clamp_bps = d.u64()?;
    let rate_cap_bps = d.u64()?;
    Ok(PerpMarket {
        symbol,
        base_symbol,
        quote_decimals,
        base_decimals,
        tick_size_quote_minor,
        lot_size_base_minor,
        initial_margin_ratio_bps,
        maintenance_margin_ratio_bps,
        price_band_bps,
        max_order_lots,
        funding: FundingParams {
            interval_ms,
            interest_rate_bps_per_interval,
            premium_clamp_bps,
            rate_cap_bps,
        },
    })
}

fn decode_option(d: &mut Decoder<'_>) -> Result<OptionMarket, DecodeError> {
    let symbol = d.str()?;
    let base_symbol = d.str()?;
    let kind = match d.varint()? {
        0 => OptionKind::Call,
        1 => OptionKind::Put,
        other => return Err(DecodeError::UnknownTag(other as u8)),
    };
    let strike_quote_minor = d.varint()?;
    let expiry_ts_ms = d.u64()?;
    let variant = match d.varint()? {
        0 => OptionVariant::Dated,
        1 => OptionVariant::Everlasting,
        other => return Err(DecodeError::UnknownTag(other as u8)),
    };
    let interval_ms = d.u64()?;
    let maturity_multiple = u32::try_from(d.varint()?).unwrap_or(1);
    let quote_decimals = u32::try_from(d.varint()?).unwrap_or(0);
    let base_decimals = u32::try_from(d.varint()?).unwrap_or(0);
    let tick_size_quote_minor = d.varint()?;
    let lot_size_base_minor = d.varint()?;
    let price_band_bps = d.u64()?;
    let max_order_lots = d.u64()?;
    let short_option_min_bps = d.u64()?;
    let liquidation_fee_bps = d.u64()?;
    let exercise_style = match d.varint()? {
        0 => ExerciseStyle::European,
        1 => ExerciseStyle::American,
        other => return Err(DecodeError::UnknownTag(other as u8)),
    };
    let settlement_twap_ms = d.u64()?;
    let exercise_fee_bps = d.u64()?;
    Ok(OptionMarket {
        symbol,
        base_symbol,
        kind,
        strike_quote_minor,
        expiry_ts_ms,
        variant,
        everlasting: EverlastingParams {
            interval_ms,
            maturity_multiple,
        },
        quote_decimals,
        base_decimals,
        tick_size_quote_minor,
        lot_size_base_minor,
        price_band_bps,
        max_order_lots,
        margin: OptionMarginParams {
            short_option_min_bps,
            liquidation_fee_bps,
        },
        exercise_style,
        american: AmericanParams {
            settlement_twap_ms,
            exercise_fee_bps,
        },
    })
}

// ----------------------------------------------------------------------
// Primitives
// ----------------------------------------------------------------------

/// Append-only encode cursor.
#[derive(Default)]
pub struct Encoder {
    buf: Vec<u8>,
}

impl Encoder {
    /// New encoder.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Encode a varint.
    pub fn varint(&mut self, mut v: u128) {
        loop {
            let mut b = (v & 0x7F) as u8;
            v >>= 7;
            if v != 0 {
                b |= 0x80;
            }
            self.buf.push(b);
            if v == 0 {
                break;
            }
        }
    }

    /// Encode a signed varint (zigzag).
    pub fn svarint(&mut self, v: i64) {
        let zig = ((v << 1) ^ (v >> 63)) as u64;
        self.varint(u128::from(zig));
    }

    /// Encode a boolean as one byte.
    pub fn bool(&mut self, b: bool) {
        self.buf.push(u8::from(b));
    }

    /// Encode a length-prefixed byte string.
    pub fn bytes(&mut self, b: &[u8]) {
        self.varint(u128::try_from(b.len()).unwrap_or(u128::MAX));
        self.buf.extend_from_slice(b);
    }

    /// Encode a string.
    pub fn str(&mut self, s: &str) {
        self.bytes(s.as_bytes());
    }

    /// Encode an optional varint (0 = None, else n+1).
    pub fn opt_varint(&mut self, v: Option<u64>) {
        match v {
            None => self.varint(0),
            Some(x) => self.varint(u128::from(x) + 1),
        }
    }

    /// Encode an optional u128.
    pub fn opt_u128(&mut self, v: Option<u128>) {
        match v {
            None => self.varint(0),
            Some(x) => self.varint(x + 1),
        }
    }

    /// Encode a vec of varints.
    pub fn varint_vec(&mut self, xs: &[u64]) {
        self.varint(u128::try_from(xs.len()).unwrap_or(u128::MAX));
        for &x in xs {
            self.varint(u128::from(x));
        }
    }

    /// Finish, returning the payload.
    #[must_use]
    pub fn into_vec(self) -> Vec<u8> {
        self.buf
    }
}

/// Decode cursor over a byte slice.
pub struct Decoder<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Decoder<'a> {
    /// New decoder at offset 0.
    #[must_use]
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn take(&mut self) -> Result<u8, DecodeError> {
        let b = *self.data.get(self.pos).ok_or(DecodeError::Truncated)?;
        self.pos += 1;
        Ok(b)
    }

    /// Decode a varint.
    pub fn varint(&mut self) -> Result<u128, DecodeError> {
        let mut out = 0_u128;
        let mut shift = 0_u32;
        loop {
            let b = self.take()?;
            out |= u128::from(b & 0x7F) << shift;
            if b & 0x80 == 0 {
                return Ok(out);
            }
            shift += 7;
            if shift >= 128 {
                return Err(DecodeError::Malformed);
            }
        }
    }

    /// Decode a u64 varint.
    pub fn u64(&mut self) -> Result<u64, DecodeError> {
        let v = self.varint()?;
        u64::try_from(v).map_err(|_| DecodeError::Malformed)
    }

    /// Decode a signed varint.
    pub fn i64(&mut self) -> Result<i64, DecodeError> {
        let zig = u64::try_from(self.varint()?).map_err(|_| DecodeError::Malformed)?;
        Ok(((zig >> 1) as i64) ^ -((zig & 1) as i64))
    }

    /// Decode a boolean.
    pub fn bool(&mut self) -> Result<bool, DecodeError> {
        Ok(self.take()? != 0)
    }

    /// Decode a length-prefixed byte string.
    pub fn bytes(&mut self) -> Result<Vec<u8>, DecodeError> {
        let len = self.varint()?;
        let len = usize::try_from(len).map_err(|_| DecodeError::Malformed)?;
        if self
            .pos
            .checked_add(len)
            .map_or(true, |end| end > self.data.len())
        {
            return Err(DecodeError::Truncated);
        }
        let out = self.data[self.pos..self.pos + len].to_vec();
        self.pos += len;
        Ok(out)
    }

    /// Decode a string.
    pub fn str(&mut self) -> Result<String, DecodeError> {
        String::from_utf8(self.bytes()?).map_err(|_| DecodeError::Malformed)
    }

    /// Decode an optional u64.
    pub fn opt_u64(&mut self) -> Result<Option<u64>, DecodeError> {
        match self.varint()? {
            0 => Ok(None),
            n => u64::try_from(n - 1)
                .map(Some)
                .map_err(|_| DecodeError::Malformed),
        }
    }

    /// Decode an optional u128.
    pub fn opt_u128(&mut self) -> Result<Option<u128>, DecodeError> {
        match self.varint()? {
            0 => Ok(None),
            n => Ok(Some(n - 1)),
        }
    }

    /// Decode a vec of u64 varints.
    pub fn u64_vec(&mut self) -> Result<Vec<u64>, DecodeError> {
        let len = self.varint()?;
        let len = usize::try_from(len).map_err(|_| DecodeError::Malformed)?;
        let mut out = Vec::with_capacity(len.min(4096));
        for _ in 0..len {
            out.push(self.u64()?);
        }
        Ok(out)
    }

    /// Whether the whole buffer was consumed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pos >= self.data.len()
    }

    /// Bytes consumed so far.
    #[must_use]
    pub fn consumed(&self) -> usize {
        self.pos
    }
}

/// Why a decode failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeError {
    /// Buffer ended mid-field.
    Truncated,
    /// Semantically invalid (bad varint, non-UTF8, wrong tag).
    Malformed,
    /// Unknown command or enum tag.
    UnknownTag(u8),
}

// ----------------------------------------------------------------------
// Enums
// ----------------------------------------------------------------------

fn encode_side(e: &mut Encoder, s: Side) {
    e.varint(match s {
        Side::Bid => 0,
        Side::Ask => 1,
    });
}

fn decode_side(d: &mut Decoder<'_>) -> Result<Side, DecodeError> {
    match d.varint()? {
        0 => Ok(Side::Bid),
        1 => Ok(Side::Ask),
        other => Err(DecodeError::UnknownTag(other as u8)),
    }
}

fn encode_tif(e: &mut Encoder, t: TimeInForce) {
    match t {
        TimeInForce::Gtc => e.varint(0),
        TimeInForce::Ioc => e.varint(1),
        TimeInForce::Fok => e.varint(2),
        TimeInForce::Gtd(ts) => {
            e.varint(3);
            e.varint(u128::from(ts));
        }
    }
}

fn decode_tif(d: &mut Decoder<'_>) -> Result<TimeInForce, DecodeError> {
    match d.varint()? {
        0 => Ok(TimeInForce::Gtc),
        1 => Ok(TimeInForce::Ioc),
        2 => Ok(TimeInForce::Fok),
        3 => Ok(TimeInForce::Gtd(
            TimestampMs::try_from(d.varint()?).unwrap_or(0),
        )),
        other => Err(DecodeError::UnknownTag(other as u8)),
    }
}

fn encode_stp(e: &mut Encoder, s: SelfTradePrevention) {
    e.varint(match s {
        SelfTradePrevention::CancelNewest => 0,
        SelfTradePrevention::CancelOldest => 1,
        SelfTradePrevention::CancelBoth => 2,
        SelfTradePrevention::DecrementAndCancel => 3,
    });
}

fn decode_stp(d: &mut Decoder<'_>) -> Result<SelfTradePrevention, DecodeError> {
    match d.varint()? {
        0 => Ok(SelfTradePrevention::CancelNewest),
        1 => Ok(SelfTradePrevention::CancelOldest),
        2 => Ok(SelfTradePrevention::CancelBoth),
        3 => Ok(SelfTradePrevention::DecrementAndCancel),
        other => Err(DecodeError::UnknownTag(other as u8)),
    }
}

fn encode_order_type(e: &mut Encoder, t: OrderType) {
    match t {
        OrderType::Limit => e.varint(0),
        OrderType::Market => e.varint(1),
        OrderType::StopMarket { trigger_price } => {
            e.varint(2);
            e.varint(u128::from(trigger_price));
        }
        OrderType::StopLimit {
            trigger_price,
            limit_price,
        } => {
            e.varint(3);
            e.varint(u128::from(trigger_price));
            e.varint(u128::from(limit_price));
        }
        OrderType::TrailingStopMarket { offset_ticks } => {
            e.varint(4);
            e.varint(u128::from(offset_ticks));
        }
        OrderType::TrailingStopLimit {
            offset_ticks,
            limit_ticks,
        } => {
            e.varint(5);
            e.varint(u128::from(offset_ticks));
            e.varint(u128::from(limit_ticks));
        }
    }
}

fn decode_order_type(d: &mut Decoder<'_>) -> Result<OrderType, DecodeError> {
    match d.varint()? {
        0 => Ok(OrderType::Limit),
        1 => Ok(OrderType::Market),
        2 => Ok(OrderType::StopMarket {
            trigger_price: d.u64()?,
        }),
        3 => Ok(OrderType::StopLimit {
            trigger_price: d.u64()?,
            limit_price: d.u64()?,
        }),
        4 => Ok(OrderType::TrailingStopMarket {
            offset_ticks: d.u64()?,
        }),
        5 => Ok(OrderType::TrailingStopLimit {
            offset_ticks: d.u64()?,
            limit_ticks: d.u64()?,
        }),
        other => Err(DecodeError::UnknownTag(other as u8)),
    }
}

fn encode_request(e: &mut Encoder, r: &OrderRequest) {
    e.varint(u128::from(r.subaccount));
    e.str(&r.symbol);
    encode_side(e, r.side);
    encode_order_type(e, r.order_type);
    e.opt_varint(r.price_ticks);
    e.varint(u128::from(r.qty_lots));
    encode_tif(e, r.tif);
    e.bool(r.post_only);
    e.bool(r.reduce_only);
    encode_stp(e, r.stp);
    e.opt_varint(r.display_lots);
    e.opt_varint(r.oco_group);
    e.varint(u128::from(r.client_ts));
}

fn decode_request(d: &mut Decoder<'_>) -> Result<OrderRequest, DecodeError> {
    let subaccount = d.u64()?;
    let symbol = d.str()?;
    let side = decode_side(d)?;
    let order_type = decode_order_type(d)?;
    let price_ticks = d.opt_u64()?;
    let qty_lots = d.u64()?;
    let tif = decode_tif(d)?;
    let post_only = d.bool()?;
    let reduce_only = d.bool()?;
    let stp = decode_stp(d)?;
    let display_lots = d.opt_u64()?;
    let oco_group = d.opt_u64()?;
    let client_ts = d.u64()?;
    Ok(OrderRequest {
        subaccount,
        symbol,
        side,
        order_type,
        price_ticks,
        qty_lots,
        tif,
        post_only,
        reduce_only,
        stp,
        display_lots,
        oco_group,
        client_ts,
    })
}

fn encode_rfq_leg(e: &mut Encoder, l: &RfqLegCommand) {
    e.str(&l.symbol);
    encode_side(e, l.side);
    e.varint(u128::from(l.qty_lots));
}

fn decode_rfq_leg(d: &mut Decoder<'_>) -> Result<RfqLegCommand, DecodeError> {
    let symbol = d.str()?;
    let side = decode_side(d)?;
    let qty_lots = d.u64()?;
    Ok(RfqLegCommand {
        symbol,
        side,
        qty_lots,
    })
}

// ----------------------------------------------------------------------
// Command
// ----------------------------------------------------------------------

/// Encode a command to bytes.
#[must_use]
pub fn encode_command(cmd: &Command) -> Vec<u8> {
    let mut e = Encoder::new();
    match cmd {
        Command::Deposit {
            subaccount,
            amount_quote_minor,
        } => {
            e.varint(1);
            e.varint(u128::from(*subaccount));
            e.varint(*amount_quote_minor);
        }
        Command::Withdraw {
            subaccount,
            amount_quote_minor,
        } => {
            e.varint(2);
            e.varint(u128::from(*subaccount));
            e.varint(*amount_quote_minor);
        }
        Command::Place { request, now } => {
            e.varint(3);
            encode_request(&mut e, request);
            e.varint(u128::from(*now));
        }
        Command::Cancel {
            subaccount,
            order_id,
            now,
        } => {
            e.varint(4);
            e.varint(u128::from(*subaccount));
            e.varint(u128::from(*order_id));
            e.varint(u128::from(*now));
        }
        Command::CancelAll {
            subaccount,
            symbol,
            now,
        } => {
            e.varint(5);
            e.varint(u128::from(*subaccount));
            if let Some(s) = symbol {
                e.bool(true);
                e.str(s);
            } else {
                e.bool(false);
            }
            e.varint(u128::from(*now));
        }
        Command::OracleUpdate {
            base_symbol,
            provider,
            ts,
            price_quote_minor,
        } => {
            e.varint(6);
            e.str(base_symbol);
            e.str(provider);
            e.varint(u128::from(*ts));
            e.varint(*price_quote_minor);
        }
        Command::Tick { now } => {
            e.varint(7);
            e.varint(u128::from(*now));
        }
        Command::RfqCreate {
            taker,
            legs,
            counterparties,
            min_total_cost_quote_minor,
            max_total_cost_quote_minor,
            ttl_ms,
            now,
        } => {
            e.varint(8);
            e.varint(u128::from(*taker));
            e.varint(u128::try_from(legs.len()).unwrap_or(u128::MAX));
            for leg in legs {
                encode_rfq_leg(&mut e, leg);
            }
            e.varint(u128::try_from(counterparties.len()).unwrap_or(u128::MAX));
            for &c in counterparties {
                e.varint(u128::from(c));
            }
            e.opt_u128(*min_total_cost_quote_minor);
            e.opt_u128(*max_total_cost_quote_minor);
            e.varint(u128::from(*ttl_ms));
            e.varint(u128::from(*now));
        }
        Command::RfqQuote {
            maker,
            rfq_id,
            leg_prices_ticks,
            ttl_ms,
            now,
        } => {
            e.varint(9);
            e.varint(u128::from(*maker));
            e.varint(u128::from(*rfq_id));
            e.varint_vec(leg_prices_ticks);
            e.varint(u128::from(*ttl_ms));
            e.varint(u128::from(*now));
        }
        Command::RfqExecute {
            taker,
            rfq_id,
            quote_id,
            now,
        } => {
            e.varint(10);
            e.varint(u128::from(*taker));
            e.varint(u128::from(*rfq_id));
            e.varint(u128::from(*quote_id));
            e.varint(u128::from(*now));
        }
        Command::RfqCancel {
            subaccount,
            rfq_id,
            quote_id,
            now,
        } => {
            e.varint(11);
            e.varint(u128::from(*subaccount));
            e.opt_varint(*rfq_id);
            e.opt_varint(*quote_id);
            e.varint(u128::from(*now));
        }
        Command::BlockTrade {
            taker,
            maker,
            legs,
            now,
        } => {
            e.varint(12);
            e.varint(u128::from(*taker));
            e.varint(u128::from(*maker));
            e.varint(u128::try_from(legs.len()).unwrap_or(u128::MAX));
            for (symbol, side, qty, price) in legs {
                e.str(symbol);
                encode_side(&mut e, *side);
                e.varint(u128::from(*qty));
                e.varint(u128::from(*price));
            }
            e.varint(u128::from(*now));
        }
        Command::Transfer {
            from,
            to,
            amount_quote_minor,
            now,
        } => {
            e.varint(13);
            e.varint(u128::from(*from));
            e.varint(u128::from(*to));
            e.varint(*amount_quote_minor);
            e.varint(u128::from(*now));
        }
        Command::SetMmp {
            subaccount,
            base_symbol,
            interval_ms,
            frozen_time_ms,
            amount_limit_lots,
            delta_limit_lots,
            now,
        } => {
            e.varint(14);
            e.varint(u128::from(*subaccount));
            e.str(base_symbol);
            e.varint(u128::from(*interval_ms));
            e.varint(u128::from(*frozen_time_ms));
            e.varint(u128::from(*amount_limit_lots));
            e.varint(u128::from(*delta_limit_lots));
            e.varint(u128::from(*now));
        }
        Command::Exercise {
            subaccount,
            symbol,
            lots,
            now,
        } => {
            e.varint(60);
            e.varint(u128::from(*subaccount));
            e.str(symbol);
            e.varint(u128::from(*lots));
            e.varint(u128::from(*now));
        }
        Command::SetCod {
            subaccount,
            enabled,
            now,
        } => {
            e.varint(15);
            e.varint(u128::from(*subaccount));
            e.bool(*enabled);
            e.varint(u128::from(*now));
        }
        Command::SessionDropped { subaccount, now } => {
            e.varint(16);
            e.varint(u128::from(*subaccount));
            e.varint(u128::from(*now));
        }
        Command::PlaceBatch { requests, now } => {
            e.varint(17);
            e.varint(u128::try_from(requests.len()).unwrap_or(u128::MAX));
            for r in requests {
                encode_request(&mut e, r);
            }
            e.varint(u128::from(*now));
        }
        Command::CancelBatch {
            subaccount,
            order_ids,
            now,
        } => {
            e.varint(18);
            e.varint(u128::from(*subaccount));
            e.varint_vec(order_ids);
            e.varint(u128::from(*now));
        }
        Command::Amend {
            subaccount,
            order_id,
            new_price_ticks,
            new_open_lots,
            now,
        } => {
            e.varint(19);
            e.varint(u128::from(*subaccount));
            e.varint(u128::from(*order_id));
            e.opt_varint(*new_price_ticks);
            e.opt_varint(*new_open_lots);
            e.varint(u128::from(*now));
        }
        Command::BeginAuction {
            symbol,
            uncross_at,
            now,
        } => {
            e.varint(20);
            e.str(symbol);
            e.varint(u128::from(*uncross_at));
            e.varint(u128::from(*now));
        }
        Command::DepositCollateral {
            subaccount,
            currency,
            amount_minor,
            now,
        } => {
            e.varint(21);
            e.varint(u128::from(*subaccount));
            e.str(currency);
            e.varint(*amount_minor);
            e.varint(u128::from(*now));
        }
        Command::WithdrawCollateral {
            subaccount,
            currency,
            amount_minor,
            now,
        } => {
            e.varint(22);
            e.varint(u128::from(*subaccount));
            e.str(currency);
            e.varint(*amount_minor);
            e.varint(u128::from(*now));
        }
        Command::ConvertCollateral {
            subaccount,
            from,
            to,
            from_amount_minor,
            now,
        } => {
            e.varint(23);
            e.varint(u128::from(*subaccount));
            e.str(from);
            e.str(to);
            e.varint(*from_amount_minor);
            e.varint(u128::from(*now));
        }
        Command::PlaceOco { first, second, now } => {
            e.varint(24);
            encode_request(&mut e, first);
            encode_request(&mut e, second);
            e.varint(u128::from(*now));
        }
        Command::PlaceTwap {
            subaccount,
            symbol,
            side,
            total_lots,
            slices,
            slice_interval_ms,
            limit_ticks,
            now,
        } => {
            e.varint(25);
            e.varint(u128::from(*subaccount));
            e.str(symbol);
            encode_side(&mut e, *side);
            e.varint(u128::from(*total_lots));
            e.varint(u128::from(*slices));
            e.varint(u128::from(*slice_interval_ms));
            e.opt_varint(*limit_ticks);
            e.varint(u128::from(*now));
        }
        Command::CancelTwap {
            subaccount,
            parent_id,
            now,
        } => {
            e.varint(26);
            e.varint(u128::from(*subaccount));
            e.varint(u128::from(*parent_id));
            e.varint(u128::from(*now));
        }
        Command::VaultCreate {
            revenue_share_bps,
            now,
        } => {
            e.varint(27);
            e.varint(u128::from(*revenue_share_bps));
            e.varint(u128::from(*now));
        }
        Command::VaultSubscribe {
            vault_id,
            subaccount,
            amount_quote_minor,
            now,
        } => {
            e.varint(28);
            e.varint(u128::from(*vault_id));
            e.varint(u128::from(*subaccount));
            e.varint(*amount_quote_minor);
            e.varint(u128::from(*now));
        }
        Command::VaultRedeem {
            vault_id,
            subaccount,
            shares,
            now,
        } => {
            e.varint(29);
            e.varint(u128::from(*vault_id));
            e.varint(u128::from(*subaccount));
            e.varint(*shares);
            e.varint(u128::from(*now));
        }
        Command::MmTierEnroll { subaccount, now } => {
            e.varint(30);
            e.varint(u128::from(*subaccount));
            e.varint(u128::from(*now));
        }
    }
    e.into_vec()
}

/// Decode a command from bytes (must consume the whole buffer).
pub fn decode_command(data: &[u8]) -> Result<Command, DecodeError> {
    let mut d = Decoder::new(data);
    let tag = d.varint()?;
    let cmd = match tag {
        1 => Command::Deposit {
            subaccount: d.u64()?,
            amount_quote_minor: d.varint()?,
        },
        2 => Command::Withdraw {
            subaccount: d.u64()?,
            amount_quote_minor: d.varint()?,
        },
        3 => {
            let request = decode_request(&mut d)?;
            let now = d.u64()?;
            Command::Place { request, now }
        }
        4 => Command::Cancel {
            subaccount: d.u64()?,
            order_id: d.u64()?,
            now: d.u64()?,
        },
        5 => {
            let subaccount = d.u64()?;
            let symbol = if d.bool()? { Some(d.str()?) } else { None };
            let now = d.u64()?;
            Command::CancelAll {
                subaccount,
                symbol,
                now,
            }
        }
        6 => Command::OracleUpdate {
            base_symbol: d.str()?,
            provider: d.str()?,
            ts: d.u64()?,
            price_quote_minor: d.varint()?,
        },
        7 => Command::Tick { now: d.u64()? },
        8 => {
            let taker = d.u64()?;
            let leg_len = d.u64()?;
            let mut legs = Vec::new();
            for _ in 0..leg_len {
                legs.push(decode_rfq_leg(&mut d)?);
            }
            let cp_len = d.u64()?;
            let mut counterparties = Vec::new();
            for _ in 0..cp_len {
                counterparties.push(d.u64()?);
            }
            let min_total_cost_quote_minor = d.opt_u128()?;
            let max_total_cost_quote_minor = d.opt_u128()?;
            let ttl_ms = d.u64()?;
            let now = d.u64()?;
            Command::RfqCreate {
                taker,
                legs,
                counterparties,
                min_total_cost_quote_minor,
                max_total_cost_quote_minor,
                ttl_ms,
                now,
            }
        }
        9 => Command::RfqQuote {
            maker: d.u64()?,
            rfq_id: d.u64()?,
            leg_prices_ticks: d.u64_vec()?,
            ttl_ms: d.u64()?,
            now: d.u64()?,
        },
        10 => Command::RfqExecute {
            taker: d.u64()?,
            rfq_id: d.u64()?,
            quote_id: d.u64()?,
            now: d.u64()?,
        },
        11 => Command::RfqCancel {
            subaccount: d.u64()?,
            rfq_id: d.opt_u64()?,
            quote_id: d.opt_u64()?,
            now: d.u64()?,
        },
        12 => {
            let taker = d.u64()?;
            let maker = d.u64()?;
            let len = d.u64()?;
            let mut legs = Vec::new();
            for _ in 0..len {
                let symbol = d.str()?;
                let side = decode_side(&mut d)?;
                let qty = d.u64()?;
                let price = d.u64()?;
                legs.push((symbol, side, qty, price));
            }
            let now = d.u64()?;
            Command::BlockTrade {
                taker,
                maker,
                legs,
                now,
            }
        }
        13 => Command::Transfer {
            from: d.u64()?,
            to: d.u64()?,
            amount_quote_minor: d.varint()?,
            now: d.u64()?,
        },
        14 => Command::SetMmp {
            subaccount: d.u64()?,
            base_symbol: d.str()?,
            interval_ms: d.u64()?,
            frozen_time_ms: d.u64()?,
            amount_limit_lots: d.u64()?,
            delta_limit_lots: d.u64()?,
            now: d.u64()?,
        },
        15 => Command::SetCod {
            subaccount: d.u64()?,
            enabled: d.bool()?,
            now: d.u64()?,
        },
        16 => Command::SessionDropped {
            subaccount: d.u64()?,
            now: d.u64()?,
        },
        17 => {
            let len = d.u64()?;
            let mut requests = Vec::new();
            for _ in 0..len {
                requests.push(decode_request(&mut d)?);
            }
            let now = d.u64()?;
            Command::PlaceBatch { requests, now }
        }
        18 => Command::CancelBatch {
            subaccount: d.u64()?,
            order_ids: d.u64_vec()?,
            now: d.u64()?,
        },
        19 => Command::Amend {
            subaccount: d.u64()?,
            order_id: d.u64()?,
            new_price_ticks: d.opt_u64()?,
            new_open_lots: d.opt_u64()?,
            now: d.u64()?,
        },
        20 => Command::BeginAuction {
            symbol: d.str()?,
            uncross_at: d.u64()?,
            now: d.u64()?,
        },
        21 => Command::DepositCollateral {
            subaccount: d.u64()?,
            currency: d.str()?,
            amount_minor: d.varint()?,
            now: d.u64()?,
        },
        22 => Command::WithdrawCollateral {
            subaccount: d.u64()?,
            currency: d.str()?,
            amount_minor: d.varint()?,
            now: d.u64()?,
        },
        24 => {
            let first = decode_request(&mut d)?;
            let second = decode_request(&mut d)?;
            let now = d.u64()?;
            Command::PlaceOco { first, second, now }
        }
        25 => {
            let subaccount = d.u64()?;
            let symbol = d.str()?;
            let side = decode_side(&mut d)?;
            let total_lots = d.u64()?;
            let slices = d.u64()?;
            let slice_interval_ms = d.u64()?;
            let limit_ticks = d.opt_u64()?;
            let now = d.u64()?;
            Command::PlaceTwap {
                subaccount,
                symbol,
                side,
                total_lots,
                slices,
                slice_interval_ms,
                limit_ticks,
                now,
            }
        }
        26 => {
            let subaccount = d.u64()?;
            let parent_id = d.u64()?;
            let now = d.u64()?;
            Command::CancelTwap {
                subaccount,
                parent_id,
                now,
            }
        }
        27 => {
            let revenue_share_bps = d.u64()?;
            let now = d.u64()?;
            Command::VaultCreate {
                revenue_share_bps,
                now,
            }
        }
        28 => {
            let vault_id = d.u64()?;
            let subaccount = d.u64()?;
            let amount_quote_minor = d.varint()?;
            let now = d.u64()?;
            Command::VaultSubscribe {
                vault_id,
                subaccount,
                amount_quote_minor,
                now,
            }
        }
        29 => {
            let vault_id = d.u64()?;
            let subaccount = d.u64()?;
            let shares = d.varint()?;
            let now = d.u64()?;
            Command::VaultRedeem {
                vault_id,
                subaccount,
                shares,
                now,
            }
        }
        30 => Command::MmTierEnroll {
            subaccount: d.u64()?,
            now: d.u64()?,
        },
        60 => Command::Exercise {
            subaccount: d.u64()?,
            symbol: Symbol::from(d.str()?),
            lots: u64::try_from(d.varint()?).unwrap_or(0),
            now: d.u64()?,
        },
        23 => Command::ConvertCollateral {
            subaccount: d.u64()?,
            from: d.str()?,
            to: d.str()?,
            from_amount_minor: d.varint()?,
            now: d.u64()?,
        },
        other => return Err(DecodeError::UnknownTag(other as u8)),
    };
    if !d.is_empty() {
        return Err(DecodeError::Malformed);
    }
    Ok(cmd)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varint_round_trip_boundaries() {
        for v in [0_u128, 1, 127, 128, 300, u128::from(u64::MAX), u128::MAX] {
            let mut e = Encoder::new();
            e.varint(v);
            let buf = e.into_vec();
            let mut d = Decoder::new(&buf);
            assert_eq!(d.varint().unwrap(), v);
            assert!(d.is_empty());
        }
    }

    #[test]
    fn varint_truncated_detected() {
        let mut e = Encoder::new();
        e.varint(u128::from(u64::MAX));
        let buf = e.into_vec();
        for cut in 0..buf.len() {
            let mut d = Decoder::new(&buf[..cut]);
            assert!(d.varint().is_err());
        }
    }

    #[test]
    fn every_command_round_trips() {
        let stop = OrderType::StopLimit {
            trigger_price: 79_000,
            limit_price: 78_900,
        };
        let trailing = OrderType::TrailingStopMarket { offset_ticks: 50 };
        let mut req = OrderRequest {
            subaccount: 7,
            symbol: "BTC-EVER-80000-C".into(),
            side: Side::Ask,
            order_type: trailing,
            price_ticks: Some(300),
            qty_lots: 12,
            tif: TimeInForce::Gtd(99_999),
            post_only: true,
            reduce_only: false,
            stp: SelfTradePrevention::DecrementAndCancel,
            display_lots: Some(4),
            oco_group: Some(9),
            client_ts: 123_456,
        };
        let commands = vec![
            Command::Deposit {
                subaccount: 1,
                amount_quote_minor: u128::MAX,
            },
            Command::Withdraw {
                subaccount: 2,
                amount_quote_minor: 55,
            },
            Command::Place {
                request: req.clone(),
                now: 1,
            },
            Command::Cancel {
                subaccount: 1,
                order_id: 9,
                now: 2,
            },
            Command::CancelAll {
                subaccount: 1,
                symbol: Some("BTC-PERP".into()),
                now: 3,
            },
            Command::CancelAll {
                subaccount: 1,
                symbol: None,
                now: 4,
            },
            Command::OracleUpdate {
                base_symbol: "BTC".into(),
                provider: "pyth".into(),
                ts: 5,
                price_quote_minor: 8_000_000,
            },
            Command::Tick { now: 6 },
            Command::RfqCreate {
                taker: 1,
                legs: vec![RfqLegCommand {
                    symbol: "BTC-PERP".into(),
                    side: Side::Bid,
                    qty_lots: 5,
                }],
                counterparties: vec![2, 3],
                min_total_cost_quote_minor: Some(10),
                max_total_cost_quote_minor: None,
                ttl_ms: 600_000,
                now: 7,
            },
            Command::RfqQuote {
                maker: 2,
                rfq_id: 1,
                leg_prices_ticks: vec![79_950, 79_900],
                ttl_ms: 30_000,
                now: 8,
            },
            Command::RfqExecute {
                taker: 1,
                rfq_id: 1,
                quote_id: 1,
                now: 9,
            },
            Command::RfqCancel {
                subaccount: 1,
                rfq_id: None,
                quote_id: Some(2),
                now: 10,
            },
            Command::BlockTrade {
                taker: 1,
                maker: 2,
                legs: vec![("BTC-PERP".into(), Side::Bid, 3, 79_950)],
                now: 11,
            },
            Command::Transfer {
                from: 1,
                to: 2,
                amount_quote_minor: 999,
                now: 12,
            },
            Command::SetMmp {
                subaccount: 3,
                base_symbol: "BTC".into(),
                interval_ms: 1_000,
                frozen_time_ms: 0,
                amount_limit_lots: 100,
                delta_limit_lots: 50,
                now: 13,
            },
            Command::SetCod {
                subaccount: 3,
                enabled: true,
                now: 14,
            },
            Command::SessionDropped {
                subaccount: 3,
                now: 15,
            },
            Command::PlaceBatch {
                requests: vec![req.clone()],
                now: 16,
            },
            Command::CancelBatch {
                subaccount: 1,
                order_ids: vec![1, 2, 3],
                now: 17,
            },
            Command::Amend {
                subaccount: 1,
                order_id: 2,
                new_price_ticks: Some(80_000),
                new_open_lots: None,
                now: 18,
            },
            Command::BeginAuction {
                symbol: "BTC-PERP".into(),
                uncross_at: 20_000,
                now: 19,
            },
            Command::DepositCollateral {
                subaccount: 1,
                currency: "BTC".into(),
                amount_minor: 1_000_000,
                now: 20,
            },
            Command::WithdrawCollateral {
                subaccount: 1,
                currency: "BTC".into(),
                amount_minor: 1,
                now: 21,
            },
            Command::ConvertCollateral {
                subaccount: 1,
                from: "BTC".into(),
                to: "USD".into(),
                from_amount_minor: 500,
                now: 22,
            },
        ];
        for cmd in &commands {
            let buf = encode_command(cmd);
            assert!(
                decode_command(&buf).as_ref() == Ok(cmd),
                "round trip {cmd:?}"
            );
        }
        // Mutate the request to cover the second order-type family.
        req.order_type = stop;
        let cmd = Command::Place {
            request: req,
            now: 0,
        };
        let buf = encode_command(&cmd);
        assert_eq!(decode_command(&buf).as_ref(), Ok(&cmd));
    }

    #[test]
    fn trailing_bytes_rejected() {
        let mut e = Encoder::new();
        e.varint(7);
        e.varint(42);
        let mut buf = e.into_vec();
        buf.push(0);
        assert!(decode_command(&buf).is_err());
    }

    #[test]
    fn unknown_tag_rejected() {
        let mut e = Encoder::new();
        e.varint(99);
        assert!(matches!(
            decode_command(&e.into_vec()),
            Err(DecodeError::UnknownTag(99))
        ));
    }
}
