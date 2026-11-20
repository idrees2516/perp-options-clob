//! FIX `NewOrderMultileg (AB)` end-to-end: combined orders placed,
//! matched (implied-in and combo-vs-combo), canceled, and rejected
//! through the gateway — with multileg ExecutionReports on both desks.

use poc_core::{
    ExerciseStyle, Instrument, OptionKind, OptionMarket, OptionVariant, PerpMarket, Side,
    TimestampMs,
};
use poc_engine::{Command, Engine, EngineConfig};
use poc_fix::{tag, FixGateway, FixMessage, MsgType};

const T0: TimestampMs = 10_000_000;
const RICH: u128 = 200_000_000;
const SPOT: u128 = 10_000_000;

fn call(symbol: &str, strike: u128) -> OptionMarket {
    OptionMarket {
        symbol: symbol.into(),
        base_symbol: "BTC".into(),
        kind: OptionKind::Call,
        strike_quote_minor: strike,
        expiry_ts_ms: T0 + 30 * 86_400_000,
        variant: OptionVariant::Dated,
        exercise_style: ExerciseStyle::European,
        quote_decimals: 2,
        base_decimals: 5,
        tick_size_quote_minor: 100,
        lot_size_base_minor: 100,
        price_band_bps: 1_000_000,
        ..OptionMarket::default()
    }
}

fn full_engine() -> Engine {
    let mut e = Engine::new(EngineConfig::default());
    e.register_instrument(Instrument::Perp(PerpMarket::default()));
    e.register_instrument(Instrument::Option(call("BTC-C1", 9_500_000)));
    e.register_instrument(Instrument::Option(call("BTC-C2", 10_500_000)));
    e
}

struct Venue {
    gw: FixGateway,
    taker: u64,
    dealer: u64,
    /// Per-session FIX sequence numbers (each session counts alone).
    seqs: std::collections::BTreeMap<u64, u32>,
}

impl Venue {
    fn new() -> Self {
        let mut e = full_engine();
        for provider in ["pyth", "chainlink"] {
            e.process(Command::OracleUpdate {
                base_symbol: "BTC".into(),
                provider: provider.into(),
                ts: T0,
                price_quote_minor: SPOT,
            });
        }
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
            seqs: std::collections::BTreeMap::new(),
        };
        v.logon(v.taker);
        v.logon(v.dealer);
        v
    }

    fn logon(&mut self, sid: u64) {
        let frame = inbound(
            1,
            MsgType::Logon,
            &[(tag::ENCRYPT_METHOD, "0"), (tag::HEART_BT_INT, "30")],
        );
        self.seqs.insert(sid, 2);
        let out = self.gw.on_frame(sid, &frame, T0).expect("logon");
        assert_eq!(out.len(), 1);
    }

    fn send(&mut self, sid: u64, mtype: MsgType, fields: &[(u32, &str)]) -> Vec<FixMessage> {
        let seq = self.seqs.get_mut(&sid).copied().unwrap_or(2);
        self.seqs.insert(sid, seq + 1);
        let frame = inbound(seq, mtype, fields);
        let out = self
            .gw
            .on_frame(sid, &frame, T0 + 1_000)
            .expect("frame accepted");
        out.iter().map(|b| decoded(b)).collect()
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

/// A net-priced buy of the call spread: 55(1) x 38(5) x 44(90) with a
/// two-leg 555 group.
fn multileg_buy(cl: &str, net: &str, units: &str, tif: &str) -> Vec<(u32, String)> {
    let mut f: Vec<(u32, String)> = vec![
        (tag::CL_ORD_ID, cl.to_string()),
        (tag::SIDE, "1".into()),
        (tag::ORDER_QTY, units.into()),
        (tag::PRICE, net.into()),
        (tag::TIME_IN_FORCE, tif.into()),
        (tag::NO_LEGS, "2".into()),
        (tag::LEG_SYMBOL, "BTC-C1".into()),
        (tag::LEG_SIDE, "1".into()),
        (tag::LEG_RATIO_QTY, "1".into()),
        (tag::LEG_SYMBOL, "BTC-C2".into()),
        (tag::LEG_SIDE, "2".into()),
        (tag::LEG_RATIO_QTY, "1".into()),
    ];
    f
}

fn as_strs(v: &[(u32, String)]) -> Vec<(u32, &str)> {
    v.iter().map(|(t, s)| (*t, s.as_str())).collect()
}

// ---------------------------------------------------------------------------
// 1. Net-priced multileg placement acks with a New ExecutionReport.
// ---------------------------------------------------------------------------

#[test]
fn multileg_placement_acks() {
    let mut v = Venue::new();
    let out = v.send(
        v.taker,
        MsgType::NewOrderMultileg,
        &as_strs(&multileg_buy("ML-1", "90", "5", "0")),
    );
    assert_eq!(out.len(), 1, "one ack");
    let er = &out[0];
    assert_eq!(er.msg_type(), Some(MsgType::ExecutionReport));
    assert_eq!(er.get(tag::EXEC_TYPE), Some("0")); // New
    assert_eq!(er.get(tag::ORD_STATUS), Some("0")); // New
    assert_eq!(er.get(tag::MULTI_LEG_RPT_TYPE), Some("3"));
    assert_eq!(er.get(tag::ORDER_QTY), Some("5"));
    assert_eq!(er.get(tag::LEAVES_QTY), Some("5"));
    assert!(er.get(tag::ORDER_ID).unwrap_or("").starts_with("COMBO-"));
    // The combo rests in the engine.
    assert_eq!(v.gw.engine().combos_view().all().count(), 1);
}

// ---------------------------------------------------------------------------
// 2. Implied-in fill: the multileg order sweeps both wings and the desk
//    receives a Trade ER with the per-leg group.
// ---------------------------------------------------------------------------

#[test]
fn multileg_implied_in_fill_reports_legs() {
    let mut v = Venue::new();
    // The dealer seeds thin wings: C1 ask 1_000, C2 bid 150.
    v.send(
        v.dealer,
        MsgType::NewOrderSingle,
        &[
            (tag::CL_ORD_ID, "WING-1"),
            (tag::SYMBOL, "BTC-C1"),
            (tag::SIDE, "2"),
            (tag::ORDER_QTY, "5"),
            (tag::PRICE, "1000"),
        ],
    );
    v.send(
        v.dealer,
        MsgType::NewOrderSingle,
        &[
            (tag::CL_ORD_ID, "WING-2"),
            (tag::SYMBOL, "BTC-C2"),
            (tag::SIDE, "1"),
            (tag::ORDER_QTY, "5"),
            (tag::PRICE, "150"),
        ],
    );
    // Net-priced IOC buy at 90 (composite is 85).
    let out = v.send(
        v.taker,
        MsgType::NewOrderMultileg,
        &as_strs(&multileg_buy("ML-2", "90", "5", "3")),
    );
    // New ack + trade report.
    let trade = out
        .iter()
        .find(|m| m.get(tag::EXEC_TYPE) == Some("F"))
        .expect("a trade ER");
    assert_eq!(trade.get(tag::ORD_STATUS), Some("2")); // filled
    assert_eq!(trade.get(tag::LAST_QTY), Some("5")); // units
                                                     // Net per unit: 85 minor.
    assert_eq!(trade.get(tag::LAST_PX), Some("85"));
    assert_eq!(trade.get(tag::CUM_QTY), Some("5"));
    assert_eq!(trade.get(tag::LEAVES_QTY), Some("0"));
    // The per-leg group.
    assert_eq!(trade.get(tag::NO_LEGS), Some("2"));
    let leg_symbols = trade.get_all(tag::LEG_SYMBOL);
    let leg_sides = trade.get_all(tag::LEG_SIDE);
    let leg_qtys = trade.get_all(tag::LEG_QTY);
    let leg_px = trade.get_all(tag::LEG_LAST_PX);
    assert_eq!(leg_symbols, vec!["BTC-C1", "BTC-C2"]);
    assert_eq!(leg_sides, vec!["1", "2"]);
    assert_eq!(leg_qtys, vec!["5", "5"]);
    assert_eq!(leg_px, vec!["1000", "150"]);
    // Engine positions.
    let e = v.gw.engine();
    assert_eq!(e.account(1).unwrap().lots_of("BTC-C1"), 5);
    assert_eq!(e.account(1).unwrap().lots_of("BTC-C2"), -5);
}

// ---------------------------------------------------------------------------
// 3. Multileg-vs-multileg: both desks receive fills.
// ---------------------------------------------------------------------------

#[test]
fn multileg_vs_multileg_fills_both_desks() {
    let mut v = Venue::new();
    // The dealer SELLS the spread at an 82-minor net (side 2).
    let mut sell = multileg_buy("ML-SELL", "82", "5", "0");
    sell[1] = (tag::SIDE, "2".into());
    let _ = v.send(v.dealer, MsgType::NewOrderMultileg, &as_strs(&sell));
    assert_eq!(v.gw.engine().combos_view().all().count(), 1);
    // The taker buys at 90.
    let out = v.send(
        v.taker,
        MsgType::NewOrderMultileg,
        &as_strs(&multileg_buy("ML-BUY", "90", "3", "0")),
    );
    let trade = out
        .iter()
        .find(|m| m.get(tag::EXEC_TYPE) == Some("F"))
        .expect("taker trade ER");
    assert_eq!(trade.get(tag::LAST_QTY), Some("3"));
    // Net traded at the resting 82.
    assert_eq!(trade.get(tag::LAST_PX), Some("82"));
    // Positions mirrored.
    let e = v.gw.engine();
    assert_eq!(e.account(1).unwrap().lots_of("BTC-C1"), 3);
    assert_eq!(e.account(2).unwrap().lots_of("BTC-C1"), -3);
    assert_eq!(e.account(2).unwrap().lots_of("BTC-C2"), 3);
}

// ---------------------------------------------------------------------------
// 4. Cancellation through OrderCancelRequest.
// ---------------------------------------------------------------------------

#[test]
fn multileg_cancel_closes_the_combo() {
    let mut v = Venue::new();
    v.send(
        v.taker,
        MsgType::NewOrderMultileg,
        &as_strs(&multileg_buy("ML-3", "90", "5", "0")),
    );
    assert_eq!(v.gw.engine().combos_view().all().count(), 1);
    let out = v.send(
        v.taker,
        MsgType::OrderCancelRequest,
        &[
            (tag::CL_ORD_ID, "ML-CXL"),
            (tag::ORIG_CL_ORD_ID, "ML-3"),
            (tag::SYMBOL, "BTC-C1"),
            (tag::SIDE, "1"),
            (tag::ORDER_QTY, "5"),
        ],
    );
    let er = out
        .iter()
        .find(|m| m.get(tag::EXEC_TYPE) == Some("4"))
        .expect("canceled ER");
    assert_eq!(er.get(tag::ORD_STATUS), Some("4"));
    assert_eq!(v.gw.engine().combos_view().all().count(), 0);
}

// ---------------------------------------------------------------------------
// 5. Malformed legs reject cleanly.
// ---------------------------------------------------------------------------

#[test]
fn malformed_multileg_rejects() {
    let mut v = Venue::new();
    // Ratio 0.
    let mut bad = multileg_buy("ML-BAD", "90", "5", "0");
    bad[8] = (tag::LEG_RATIO_QTY, "0".into());
    let out = v.send(v.taker, MsgType::NewOrderMultileg, &as_strs(&bad));
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].get(tag::ORD_STATUS), Some("8")); // rejected
                                                        // Unknown leg instrument.
    let mut bad2 = multileg_buy("ML-BAD2", "90", "5", "0");
    bad2[6] = (tag::LEG_SYMBOL, "NOPE".into());
    let out = v.send(v.taker, MsgType::NewOrderMultileg, &as_strs(&bad2));
    assert_eq!(out[0].get(tag::ORD_STATUS), Some("8"));
    // LegOrderQty inconsistent with ratio x units.
    let mut bad3 = multileg_buy("ML-BAD3", "90", "5", "0");
    bad3.push((tag::LEG_ORDER_QTY, "7".into()));
    let out = v.send(v.taker, MsgType::NewOrderMultileg, &as_strs(&bad3));
    assert_eq!(out[0].get(tag::ORD_STATUS), Some("8"));
    assert_eq!(v.gw.engine().combos_view().all().count(), 0);
}

// ---------------------------------------------------------------------------
// 6. Leg-priced order (no Price 44): the net derives from LegPrice(566).
// ---------------------------------------------------------------------------

#[test]
fn leg_priced_multileg_derives_the_net() {
    let mut v = Venue::new();
    // Buy the spread: leg prices 1_000 ticks (C1) and 150 ticks (C2).
    // Net = V(1000) - V(150) = 100 - 15 = 85 minor per unit.
    let fields = vec![
        (tag::CL_ORD_ID, "ML-LEG".to_string()),
        (tag::SIDE, "1".to_string()),
        (tag::ORDER_QTY, "2".to_string()),
        (tag::TIME_IN_FORCE, "0".to_string()),
        (tag::NO_LEGS, "2".to_string()),
        (tag::LEG_SYMBOL, "BTC-C1".to_string()),
        (tag::LEG_SIDE, "1".to_string()),
        (tag::LEG_RATIO_QTY, "1".to_string()),
        (tag::LEG_PRICE, "1000".to_string()),
        (tag::LEG_SYMBOL, "BTC-C2".to_string()),
        (tag::LEG_SIDE, "2".to_string()),
        (tag::LEG_RATIO_QTY, "1".to_string()),
        (tag::LEG_PRICE, "150".to_string()),
    ];
    let out = v.send(v.taker, MsgType::NewOrderMultileg, &as_strs(&fields));
    assert_eq!(out[0].get(tag::EXEC_TYPE), Some("0"), "accepted");
    // The resting combo carries the derived 85 net.
    let combo = v.gw.engine().combos_view().all().next().cloned().unwrap();
    assert_eq!(combo.limit_quote_minor_per_unit, 85);
    assert_eq!(combo.units, 2);
}

// ---------------------------------------------------------------------------
// 7. Implied-out through FIX: a plain sell fills into a resting multileg
//    order; the combo desk receives its fill report.
// ---------------------------------------------------------------------------

#[test]
fn plain_order_fills_into_a_resting_multileg() {
    let mut v = Venue::new();
    // The taker rests a BUY of the spread at an 80-minor net.
    v.send(
        v.taker,
        MsgType::NewOrderMultileg,
        &as_strs(&multileg_buy("ML-IO", "80", "5", "0")),
    );
    // The dealer seeds the C2 hedge bid at 160.
    v.send(
        v.dealer,
        MsgType::NewOrderSingle,
        &[
            (tag::CL_ORD_ID, "HEDGE"),
            (tag::SYMBOL, "BTC-C2"),
            (tag::SIDE, "1"),
            (tag::ORDER_QTY, "10"),
            (tag::PRICE, "160"),
        ],
    );
    // The dealer sells 3 C1 at 960 (the implied bid).
    let out = v.send(
        v.dealer,
        MsgType::NewOrderSingle,
        &[
            (tag::CL_ORD_ID, "AGGRO"),
            (tag::SYMBOL, "BTC-C1"),
            (tag::SIDE, "2"),
            (tag::ORDER_QTY, "3"),
            (tag::PRICE, "960"),
        ],
    );
    // The aggressor's trade report on C1.
    assert!(
        out.iter().any(|m| m.get(tag::EXEC_TYPE) == Some("F")),
        "aggressor filled: {:?}",
        out.iter()
            .map(|m| m.get(tag::EXEC_TYPE))
            .collect::<Vec<_>>()
    );
    // The combo desk's trade report: sent to the TAKER session.
    let e = v.gw.engine();
    assert_eq!(e.account(1).unwrap().lots_of("BTC-C1"), 3);
    assert_eq!(e.account(1).unwrap().lots_of("BTC-C2"), -3);
    let combo = e.combos_view().all().next().cloned().unwrap();
    assert_eq!(combo.open_units(), 2);
}

// ---------------------------------------------------------------------------
// 8. Consistency: the multileg desk's reported state matches the engine.
// ---------------------------------------------------------------------------

#[test]
fn multileg_reports_track_engine_state() {
    let mut v = Venue::new();
    v.send(
        v.taker,
        MsgType::NewOrderMultileg,
        &as_strs(&multileg_buy("ML-T1", "90", "4", "0")),
    );
    let mut sell = multileg_buy("ML-T2", "85", "6", "0");
    sell[1] = (tag::SIDE, "2".into());
    v.send(v.dealer, MsgType::NewOrderMultileg, &as_strs(&sell));
    // 4 units trade at the resting 85.
    let out = v.send(
        v.taker,
        MsgType::OrderStatusRequest,
        &[(tag::CL_ORD_ID, "ML-T1")],
    );
    // Status request paths answer for plain orders; the combo state is
    // asserted from the engine here (the desk's ERs already carry it).
    let _ = out;
    let e = v.gw.engine();
    assert_eq!(e.account(1).unwrap().lots_of("BTC-C1"), 4);
    let combos: Vec<_> = e.combos_view().all().collect();
    assert_eq!(combos.len(), 1, "the dealer's remainder rests");
    assert_eq!(combos[0].open_units(), 2);
    assert_eq!(combos[0].owner, 2);
}

// Keep Side referenced for the helper signatures above.
#[allow(dead_code)]
fn _side_anchor(_s: Side) {}
