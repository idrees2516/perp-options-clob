//! The whole-system wiring proof: one script that walks **every**
//! component of the venue through one shared engine, with the legacy
//! FIX rails carrying the desks' traffic end to end.
//!
//! ```text
//!   instruments ──┐
//!   (perp +      │
//!    American    ▼
//!    options)  FIX gateway ─► engine (CLOB matching) ─► margin/fees
//!      ▲            │   ▲                                   │
//!      │  RFQ desk  │   └── ExecutionReports ◄──────────────┘
//!      └────────────┘         (both counterparties)
//!                                 │
//!         American exercise ◄─────┤
//!         settlement book ◄──────┤
//!         proof-of-reserves ◄────┘
//! ```
//!
//! What the script proves, in order:
//!
//! 1. **Instruments** — a perp and an American option chain register
//!    against the shared engine; the oracle marks them live.
//! 2. **CLOB over FIX** — a dealer rests an option limit order through
//!    `NewOrderSingle`; a taker crosses it; BOTH desks receive
//!    `ExecutionReport(ExecType=F)` frames; the option premium moves
//!    through the venue's fee path.
//! 3. **Options legs creation over FIX** — the taker requests a
//!    two-legged structure (a call spread: buy the $80k wing, sell the
//!    $85k wing) as `QuoteRequest` with a `NoLegs` repeating group;
//!    the engine turns it into a multi-leg RFQ (G-11).
//! 4. **Dealer quote over FIX** — the dealer answers with per-leg
//!    `LegPrice`s and a committed working size (`LegOrderQty`);
//!    the venue acks with `QuoteStatusReport` (accepted, firm).
//! 5. **Execution** — the taker lifts a PARTIAL against the quote via
//!    `NewOrderSingle(117=QuoteID, 38=<qty>)`; the take lands on every
//!    leg at once; both desks see fills for both wings; the RFQ stays
//!    open for the remainder and the quote stays firm (G-11/G-15).
//! 6. **American exercise** — the long wing bought over the RFQ is
//!    tendered for early exercise; the TWAP window settles intrinsic
//!    value through the pro-rata assignment path.
//! 7. **Settlement + PoR** — the book commitment captures the resting
//!    state; the liability tree proves the taker's balance with an
//!    inclusion proof.
//! 8. **Determinism** — the entire script, replayed frame-for-frame
//!    on a fresh gateway, lands on the identical account fingerprint.

use poc_core::{
    AmericanParams, ExerciseStyle, Instrument, OptionKind, OptionMarket, OptionVariant, PerpMarket,
    TimestampMs,
};
use poc_engine::{Command, Engine, EngineConfig, Event};
use poc_fix::{tag, FixGateway, FixMessage, MsgType};
use poc_rfq::{QuoteStatus, RfqStatus};
use poc_settlement::{
    por::{verify_liability, LiabilityEntry, LiabilityTree},
    BookCommitment,
};

const T0: TimestampMs = 10_000_000;
/// $2M per desk, quote minor (2dp).
const RICH: u128 = 200_000_000;
/// Spot seed: $100k, quote minor.
const SPOT: u128 = 10_000_000;

/// The lower wing: an ITM American BTC call, strike $80k, 30 days out,
/// 1-minute TWAP settlement, 5bp exercise fee.
fn wing_80k() -> OptionMarket {
    OptionMarket {
        symbol: "BTC-AMER-80000-C".into(),
        base_symbol: "BTC".into(),
        kind: OptionKind::Call,
        strike_quote_minor: 8_000_000,
        expiry_ts_ms: T0 + 30 * 86_400_000,
        variant: OptionVariant::Dated,
        exercise_style: ExerciseStyle::American,
        american: AmericanParams {
            settlement_twap_ms: 60_000,
            exercise_fee_bps: 5,
        },
        ..OptionMarket::default()
    }
}

/// The upper wing: the same shape at strike $85k — sold by the taker
/// in the spread structure.
fn wing_85k() -> OptionMarket {
    OptionMarket {
        symbol: "BTC-AMER-85000-C".into(),
        strike_quote_minor: 8_500_000,
        ..wing_80k()
    }
}

fn full_engine() -> Engine {
    // Zero the incentive drip so cash conservation is exact across the
    // whole script.
    let cfg = EngineConfig {
        reward_per_interval_quote_minor: 0,
        ..EngineConfig::default()
    };
    let mut e = Engine::new(cfg);
    e.register_instrument(Instrument::Perp(PerpMarket::default()));
    e.register_instrument(Instrument::Option(wing_80k()));
    e.register_instrument(Instrument::Option(wing_85k()));
    e
}

fn feed_spot(e: &mut Engine, ts: TimestampMs, price: u128) {
    for provider in ["pyth", "chainlink"] {
        e.process(Command::OracleUpdate {
            base_symbol: "BTC".into(),
            provider: provider.into(),
            ts,
            price_quote_minor: price,
        });
    }
}

/// The state shared by both run-lanes of the script.
struct Venue {
    gw: FixGateway,
    taker: u64,
    dealer: u64,
    /// Cash + venue pools right after deposits, before any trading: the
    /// conservation baseline.
    baseline: i128,
    /// Subaccount cash fingerprint: (sub, cash) pairs, sorted.
    fingerprint: Vec<(u64, i128)>,
}

fn fingerprint(e: &Engine) -> Vec<(u64, i128)> {
    let mut fp: Vec<(u64, i128)> = e
        .accounts_iter()
        .map(|(&s, a)| (s, a.cash_quote_minor))
        .collect();
    fp.sort();
    fp
}

fn tracked_total(e: &Engine) -> i128 {
    let (ins, _rew, house, buy) = e.venue_pools();
    e.accounts_iter()
        .map(|(_, a)| a.cash_quote_minor)
        .fold(0_i128, i128::saturating_add)
        .saturating_add(ins)
        .saturating_add(i128::try_from(house).unwrap_or(i128::MAX))
        .saturating_add(i128::try_from(buy).unwrap_or(i128::MAX))
}

impl Venue {
    /// Build the shared engine, wire two desks onto it, log them in.
    fn new() -> Self {
        let mut e = full_engine();
        feed_spot(&mut e, T0, SPOT);
        e.process(Command::Tick { now: T0 });
        for sub in [1_u64, 2] {
            e.process(Command::Deposit {
                subaccount: sub,
                amount_quote_minor: RICH,
            });
        }
        let mut gw = FixGateway::new(e);
        gw.register_comp_id("TAKER-DESK", 1);
        gw.register_comp_id("DEALER-DESK", 2);
        let taker = gw.connect("TAKER-DESK").expect("taker session");
        let dealer = gw.connect("DEALER-DESK").expect("dealer session");
        let mut v = Venue {
            gw,
            taker,
            dealer,
            baseline: 0,
            fingerprint: Vec::new(),
        };
        v.baseline = tracked_total(v.engine());
        v.logon(v.taker, 1);
        v.logon(v.dealer, 1);
        v
    }

    fn logon(&mut self, sid: u64, seq: u32) {
        let frame = inbound(
            seq,
            MsgType::Logon,
            &[(tag::ENCRYPT_METHOD, "0"), (tag::HEART_BT_INT, "30")],
        );
        let out = self.gw.on_frame(sid, &frame, T0).expect("logon");
        assert_eq!(out.len(), 1, "logon must ack exactly once");
        assert_eq!(
            decoded(&out[0]).msg_type(),
            Some(MsgType::Logon),
            "logon ack"
        );
    }

    /// Send one application frame; return every outbound message,
    /// decoded, in order.
    fn send(
        &mut self,
        sid: u64,
        seq: u32,
        mtype: MsgType,
        fields: &[(u32, &str)],
    ) -> Vec<FixMessage> {
        let frame = inbound(seq, mtype, fields);
        let out = self
            .gw
            .on_frame(sid, &frame, T0 + 1_000)
            .expect("frame accepted");
        out.iter().map(|b| decoded(b)).collect()
    }

    fn engine(&self) -> &Engine {
        self.gw.engine()
    }

    fn engine_mut(&mut self) -> &mut Engine {
        self.gw.engine_mut()
    }
}

fn inbound(seq: u32, mtype: MsgType, fields: &[(u32, &str)]) -> Vec<u8> {
    let mut m = FixMessage::new();
    m.push(tag::MSG_SEQ_NUM, seq.to_string());
    m.push(tag::SENDER_COMP_ID, "DESK");
    m.push(tag::TARGET_COMP_ID, "VENUE");
    m.push(tag::SENDING_TIME, "20260101-00:00:00.000");
    m.push(tag::MSG_TYPE, mtype.code());
    for (t, v) in fields {
        m.push(*t, *v);
    }
    m.encode()
}

fn decoded(frame: &[u8]) -> FixMessage {
    FixMessage::decode(frame).expect("outbound frame decodes")
}

/// The complete wiring script. Runs identically twice (lane A for
/// assertions, lane B for determinism).
fn run_script(v: &mut Venue) {
    // ------------------------------------------------------------------
    // 2. CLOB over FIX: the dealer rests an option limit, the taker
    //    crosses it. The matching engine does the work; both desks see
    //    fills.
    // ------------------------------------------------------------------
    let premium_ticks = 48_000_u64.to_string(); // $24k/BTC premium
    let rest = v.send(
        v.dealer,
        2,
        MsgType::NewOrderSingle,
        &[
            (tag::CL_ORD_ID, "D-REST-1"),
            (tag::SYMBOL, "BTC-AMER-80000-C"),
            (tag::SIDE, "2"), // sell
            (tag::ORDER_QTY, "40"),
            (tag::ORD_TYPE, "2"),
            (tag::PRICE, &premium_ticks),
        ],
    );
    assert!(
        rest.iter()
            .any(|m| m.msg_type() == Some(MsgType::ExecutionReport)),
        "resting order acks with an ExecutionReport"
    );

    let cross = v.send(
        v.taker,
        2,
        MsgType::NewOrderSingle,
        &[
            (tag::CL_ORD_ID, "T-CROSS-1"),
            (tag::SYMBOL, "BTC-AMER-80000-C"),
            (tag::SIDE, "1"), // buy
            (tag::ORDER_QTY, "40"),
            (tag::ORD_TYPE, "2"),
            (tag::PRICE, &premium_ticks),
        ],
    );
    // Both desks must see the trade.
    assert!(
        cross
            .iter()
            .any(|m| m.get(tag::EXEC_TYPE) == Some("F") && m.get_u64(tag::LAST_QTY) == Some(40)),
        "taker sees its fill: {cross:?}"
    );

    // The venue moved the option: taker +40 lots, dealer -40 lots.
    assert_eq!(
        v.engine().account(1).map(|a| a.lots_of("BTC-AMER-80000-C")),
        Some(40)
    );
    assert_eq!(
        v.engine().account(2).map(|a| a.lots_of("BTC-AMER-80000-C")),
        Some(-40)
    );
    // The premium-unpaid convention (Deribit-style): the option's
    // premium rides in margin marks, NOT in execution cash — execution
    // moves exactly the fee leg of the trade (taker fee $16, maker
    // fee $4 on 40 lots at a $24k/BTC premium).
    let cash1 = v.engine().account(1).map(|a| a.cash_quote_minor).unwrap();
    assert_eq!(cash1, RICH as i128 - 1_600, "taker paid exactly its fee");
    let cash2 = v.engine().account(2).map(|a| a.cash_quote_minor).unwrap();
    assert_eq!(cash2, RICH as i128 - 400, "dealer paid exactly its fee");

    // ------------------------------------------------------------------
    // 3. Options legs creation over FIX: a call spread as one package.
    //    Buy 10 of the $80k wing, sell 10 of the $85k wing.
    // ------------------------------------------------------------------
    let rq = v.send(
        v.taker,
        3,
        MsgType::QuoteRequest,
        &[
            (tag::QUOTE_REQ_ID, "SPREAD-1"),
            (tag::NO_LEGS, "2"),
            // Leg 1: buy the lower wing.
            (tag::LEG_SYMBOL, "BTC-AMER-80000-C"),
            (tag::LEG_SIDE, "1"),
            (tag::LEG_QTY, "10"),
            // Leg 2: sell the upper wing.
            (tag::LEG_SYMBOL, "BTC-AMER-85000-C"),
            (tag::LEG_SIDE, "2"),
            (tag::LEG_QTY, "10"),
        ],
    );
    let _ = rq; // QuoteRequest emits nothing: quotes are the response.

    // The engine created RFQ #1 with both legs, taker-owned.
    let rfq = v.engine().rfq_view().rfq(1).cloned().expect("rfq 1 exists");
    assert_eq!(rfq.taker, 1);
    assert_eq!(rfq.legs.len(), 2);
    assert_eq!(rfq.legs[0].symbol, "BTC-AMER-80000-C");
    assert_eq!(rfq.legs[1].symbol, "BTC-AMER-85000-C");
    assert_eq!(rfq.remaining_lots, vec![10, 10]);
    assert_eq!(rfq.status, RfqStatus::Open);

    // ------------------------------------------------------------------
    // 4. The dealer quotes the structure: per-leg prices, committed
    //    working size on every leg.
    // ------------------------------------------------------------------
    let quote = v.send(
        v.dealer,
        3,
        MsgType::Quote,
        &[
            (tag::QUOTE_REQ_ID, "SPREAD-1"),
            (tag::QUOTE_ID, "SPREAD-Q1"),
            (tag::NO_LEGS, "2"),
            (tag::LEG_PRICE, "48000"),
            (tag::LEG_PRICE, "27000"),
            (tag::LEG_ORDER_QTY, "10"),
            (tag::LEG_ORDER_QTY, "10"),
        ],
    );
    // The dealer's ack: live, firm, tradable.
    assert_eq!(quote.len(), 1, "exactly one quote ack: {quote:?}");
    let ack = &quote[0];
    assert_eq!(ack.msg_type(), Some(MsgType::QuoteStatusReport));
    assert_eq!(ack.get(tag::QUOTE_REQ_ID), Some("SPREAD-1"));
    assert_eq!(ack.get(tag::QUOTE_ID), Some("SPREAD-Q1"));
    assert_eq!(ack.get(tag::QUOTE_TYPE), Some("1")); // tradable
    let q = v.engine().rfq_view().quote(1).cloned().expect("quote 1");
    assert_eq!(q.maker, 2);
    assert_eq!(q.leg_prices_ticks, vec![48_000, 27_000]);
    assert_eq!(q.qty_lots, vec![10, 10]);
    assert_eq!(q.status, QuoteStatus::Live);

    // ------------------------------------------------------------------
    // 5. Execution: a PARTIAL lift of the spread — 5 lots per wing.
    // ------------------------------------------------------------------
    let hit = v.send(
        v.taker,
        4,
        MsgType::NewOrderSingle,
        &[
            (tag::CL_ORD_ID, "T-SPREAD-HIT"),
            (tag::QUOTE_ID, "SPREAD-Q1"),
            (tag::ORDER_QTY, "5"),
        ],
    );
    // The taker sees BOTH wings fill (the outbound stream carries the
    // dealer's copies too — filter to the taker's desk by TargetCompID).
    let taker_fills: Vec<&str> = hit
        .iter()
        .filter(|m| {
            m.get(tag::EXEC_TYPE) == Some("F") && m.get(tag::TARGET_COMP_ID) == Some("TAKER-DESK")
        })
        .map(|m| m.get(tag::SYMBOL).unwrap_or_default())
        .collect();
    assert_eq!(
        taker_fills,
        vec!["BTC-AMER-80000-C", "BTC-AMER-85000-C"],
        "both wings fill the taker: {hit:?}"
    );
    // ...and the quoting dealer sees its side of both wings: a fill you
    // cannot see is a fill you cannot hedge.
    let dealer_fills: Vec<&str> = hit
        .iter()
        .filter(|m| {
            m.get(tag::EXEC_TYPE) == Some("F") && m.get(tag::TARGET_COMP_ID) == Some("DEALER-DESK")
        })
        .map(|m| m.get(tag::SYMBOL).unwrap_or_default())
        .collect();
    assert_eq!(
        dealer_fills,
        vec!["BTC-AMER-80000-C", "BTC-AMER-85000-C"],
        "both wings fill the dealer: {hit:?}"
    );
    // The taker's sides in its reports: Buy the 80k wing, Sell the 85k
    // wing — exactly the structure requested.
    let taker_sides: Vec<&str> = hit
        .iter()
        .filter(|m| {
            m.get(tag::EXEC_TYPE) == Some("F") && m.get(tag::TARGET_COMP_ID) == Some("TAKER-DESK")
        })
        .map(|m| m.get(tag::SIDE).unwrap_or_default())
        .collect();
    assert_eq!(
        taker_sides,
        vec!["1", "2"],
        "taker sides match the structure"
    );

    // Positions after the partial spread lift:
    //   taker: 40 + 5 = 45 on the 80k wing, -5 on the 85k wing
    //   dealer: -40 - 5 = -45 on the 80k wing, +5 on the 85k wing
    assert_eq!(
        v.engine().account(1).map(|a| a.lots_of("BTC-AMER-80000-C")),
        Some(45)
    );
    assert_eq!(
        v.engine().account(1).map(|a| a.lots_of("BTC-AMER-85000-C")),
        Some(-5)
    );
    assert_eq!(
        v.engine().account(2).map(|a| a.lots_of("BTC-AMER-80000-C")),
        Some(-45)
    );
    assert_eq!(
        v.engine().account(2).map(|a| a.lots_of("BTC-AMER-85000-C")),
        Some(5)
    );

    // The RFQ stays open with half its size; the quote stays firm for
    // its remainder (G-11's discipline).
    let rfq = v.engine().rfq_view().rfq(1).cloned().expect("rfq alive");
    assert_eq!(rfq.status, RfqStatus::Open);
    assert_eq!(rfq.remaining_lots, vec![5, 5]);
    let q = v
        .engine()
        .rfq_view()
        .quote(1)
        .cloned()
        .expect("quote alive");
    assert_eq!(q.status, QuoteStatus::Live);
    assert_eq!(q.remaining_lots(), vec![5, 5]);

    // ------------------------------------------------------------------
    // 6. American exercise: tender 5 of the long wing. The TWAP window
    //    settles intrinsic value against the short side.
    // ------------------------------------------------------------------
    let cash_before = tracked_total(v.engine());
    let evs = v.engine_mut().process(Command::Exercise {
        subaccount: 1,
        symbol: "BTC-AMER-80000-C".into(),
        lots: 5,
        now: T0 + 2_000,
    });
    assert!(
        matches!(evs.as_slice(), [Event::ExerciseQueued { .. }]),
        "exercise queues: {evs:?}"
    );
    // Fill the 1-minute TWAP window at the same $100k spot.
    for dt in [10_000_u64, 30_000, 50_000, 60_000] {
        feed_spot(v.engine_mut(), T0 + dt, SPOT);
    }
    let settle = v.engine_mut().process(Command::Tick { now: T0 + 65_000 });
    assert!(
        settle
            .iter()
            .any(|ev| matches!(ev, Event::OptionExercised(_))),
        "the TWAP window settles the exercise: {settle:?}"
    );
    // The exercised lots left the taker's book.
    assert_eq!(
        v.engine().account(1).map(|a| a.lots_of("BTC-AMER-80000-C")),
        Some(40)
    );
    // Conservation across the exercise: cash + pools are a closed loop.
    let cash_after = tracked_total(v.engine());
    assert_eq!(
        cash_before, cash_after,
        "exercise conserves the tracked total"
    );

    // ------------------------------------------------------------------
    // 7. Settlement + proof-of-reserves over the final state.
    // ------------------------------------------------------------------
    // A market maker never leaves the book empty: the dealer rests a
    // fresh quote so the commitment captures live resting state.
    v.engine_mut().process(Command::Place {
        request: poc_engine::OrderRequest::limit(
            2,
            "BTC-AMER-80000-C",
            poc_core::Side::Ask,
            45_000,
            25,
        ),
        now: T0 + 66_000,
    });
    let commitment = BookCommitment::capture(v.engine());
    assert!(!commitment.entries.is_empty(), "resting state committed");

    let rows = v.engine().por_liabilities(T0 + 65_000);
    let entries: Vec<LiabilityEntry> = rows
        .into_iter()
        .map(|r| LiabilityEntry {
            subaccount: r.subaccount,
            quote_cash_minor: r.quote_cash_minor,
            collateral: r.collateral,
            vault_claims_quote_minor: r.vault_claims_quote_minor,
        })
        .collect();
    let tree = LiabilityTree::build(entries, 7).expect("liability tree builds");
    let root = tree.root();
    let (taker_entry, taker_proof) = tree.proof(1).expect("taker has a liability proof");
    assert!(verify_liability(&taker_entry, &taker_proof, &root, 7));

    // The fingerprint for lane B's determinism check.
    v.fingerprint = fingerprint(v.engine());
}

// ---------------------------------------------------------------------------
// The script, driven twice: lane A asserts, lane B proves determinism.
// ---------------------------------------------------------------------------

#[test]
fn full_stack_wiring_end_to_end() {
    // Lane A: the asserted run.
    let mut a = Venue::new();
    run_script(&mut a);

    // ------------------------------------------------------------------
    // 8. Determinism: replay the identical frame sequence on a fresh
    //    gateway; the final account fingerprint must match bit for bit.
    // ------------------------------------------------------------------
    let mut b = Venue::new();
    run_script(&mut b);
    assert_eq!(a.fingerprint, b.fingerprint, "wiring is deterministic");

    // And the whole-script conservation law: measured against the
    // post-deposit baseline, nothing was printed out of thin air —
    // every fee dollar lands in a venue pool (house/insurance/buyback).
    let total = tracked_total(a.engine());
    assert_eq!(total, a.baseline, "no cash created or destroyed");
    // Sanity: the desks really did pay for the flow (fees left the
    // accounts and landed in the venue pools).
    assert!(
        a.engine()
            .account(1)
            .map(|acc| acc.cash_quote_minor)
            .unwrap()
            < RICH as i128,
        "taker net cash reflects fees"
    );
}
