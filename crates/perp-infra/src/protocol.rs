//! Wire protocol of the TCP server: one JSON object per line in each direction.
//!
//! # Requests
//!
//! Every request is a JSON object with a string field `op`. Unknown fields,
//! missing fields, wrong types and out-of-range values are rejected. All
//! numbers are JSON **integers in the engine's raw units** (see
//! [`perp_domain::fixed`]): prices, quantities and money amounts are raw
//! integers, never decimals. Money is a signed 128-bit integer; clients that
//! parse JSON numbers as IEEE doubles will lose precision above 2^53 and
//! should use a big-integer parser.
//!
//! | `op` | fields |
//! |------|--------|
//! | `auth` | `token` (string) |
//! | `ping` | – |
//! | `deposit` | `account`, `amount` |
//! | `withdraw` | `account`, `amount` |
//! | `fund_insurance` | `amount` |
//! | `place` | `market`, `account`, `side` (`buy`/`sell`), `type` (`limit`/`market`), `qty`, `client_id`, optional `reduce_only` (default `false`), optional `stp` (`cancel_newest` (default), `cancel_oldest`, `cancel_both`); limit orders also `price` and `tif` (`gtc`/`ioc`/`fok`/`post_only`); market orders also `max_slippage_bps` |
//! | `cancel` | `account`, `order_id` |
//! | `cancel_all` | `account`, optional `market` |
//! | `replace` | `account`, `order_id`, optional `price`, optional `qty` |
//! | `oracle` | `market`, `index_price`, `source_ts` |
//! | `tick` | – |
//! | `book` | `market`, optional `depth` (default 10, at most 100) |
//! | `account` | `account` |
//! | `summary` | – |
//!
//! `configure` and `add_market` are deliberately not available over TCP.
//!
//! # Responses
//!
//! Success: `{"ok":true, ...}`. Failure: `{"ok":false,"error":"<code>"}` with
//! an optional `"message"`. Engine rejections use [`perp_domain::error::Error::code`]
//! as the code. Responses never echo request contents.
//!
//! Transport-level codes (from the server): `auth_failed`, `rate_limited`,
//! `line_too_long`, `invalid_utf8`, `too_many_connections`, `timeout`,
//! `unavailable`, `storage_error`, `response_too_large`. Request-level codes:
//! `bad_json`, `bad_request`, `unknown_op`, `already_authenticated`.

use perp_domain::fixed::{Money, Price, Qty};
use perp_domain::types::{
    AccountId, CancelReason, EventBody, MarketId, OrderKind, Output, PlaceOrder, Side, StpMode, Tif,
};
use perp_domain::views::{AccountView, BookView, LevelView, PositionView, Summary};

use crate::json::{self, Json, JsonError, Limits, ObjWriter};

/// Default number of book levels per side returned by `book`.
pub const DEFAULT_BOOK_DEPTH: usize = 10;
/// Largest `depth` accepted by `book`.
pub const MAX_BOOK_DEPTH: usize = 100;
/// Default cap on the size of a response line, in bytes.
pub const DEFAULT_RESPONSE_CAP: usize = 256 * 1024;

/// A decoded request line.
#[derive(Clone, PartialEq, Eq)]
pub enum Request {
    /// Authenticate the connection.
    Auth {
        /// The shared secret presented by the client.
        token: String,
    },
    /// Liveness check; answered without touching the engine.
    Ping,
    /// A state-changing event for the engine.
    Command(EventBody),
    /// Query the top of a book.
    Book {
        /// Market.
        market: MarketId,
        /// Levels per side.
        depth: usize,
    },
    /// Query an account.
    Account {
        /// Account id.
        id: AccountId,
    },
    /// Query engine-wide counters.
    Summary,
}

impl std::fmt::Debug for Request {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Request::Auth { .. } => f.write_str("Auth { token: <redacted> }"),
            Request::Ping => f.write_str("Ping"),
            Request::Command(body) => f.debug_tuple("Command").field(body).finish(),
            Request::Book { market, depth } => f
                .debug_struct("Book")
                .field("market", market)
                .field("depth", depth)
                .finish(),
            Request::Account { id } => f.debug_struct("Account").field("id", id).finish(),
            Request::Summary => f.write_str("Summary"),
        }
    }
}

/// A request that could not be decoded.
///
/// `message` is built from static text and field names defined in this module;
/// it never contains request contents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtoError {
    /// Short machine-readable code.
    pub code: &'static str,
    /// Human-readable explanation.
    pub message: String,
}

impl std::fmt::Display for ProtoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for ProtoError {}

fn bad(message: impl Into<String>) -> ProtoError {
    ProtoError {
        code: "bad_request",
        message: message.into(),
    }
}

impl From<JsonError> for ProtoError {
    fn from(e: JsonError) -> ProtoError {
        ProtoError {
            code: "bad_json",
            message: e.to_string(),
        }
    }
}

/// View over the members of a request object with typed accessors.
struct Fields<'a> {
    members: &'a [(String, Json)],
}

impl<'a> Fields<'a> {
    fn get(&self, key: &str) -> Option<&'a Json> {
        self.members.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    /// Reject any member that is not in `allowed`.
    fn only(&self, allowed: &[&str]) -> Result<(), ProtoError> {
        if self
            .members
            .iter()
            .all(|(k, _)| allowed.contains(&k.as_str()))
        {
            Ok(())
        } else {
            Err(bad("unknown field"))
        }
    }

    fn required(&self, key: &str) -> Result<&'a Json, ProtoError> {
        self.get(key)
            .ok_or_else(|| bad(format!("missing field '{key}'")))
    }

    fn str(&self, key: &str) -> Result<&'a str, ProtoError> {
        self.required(key)?
            .as_str()
            .ok_or_else(|| bad(format!("field '{key}' must be a string")))
    }

    fn int(&self, key: &str) -> Result<i128, ProtoError> {
        int_of(self.required(key)?, key)
    }

    fn opt_int(&self, key: &str) -> Result<Option<i128>, ProtoError> {
        self.get(key).map(|v| int_of(v, key)).transpose()
    }

    fn u64(&self, key: &str) -> Result<u64, ProtoError> {
        to_u64(self.int(key)?, key)
    }

    fn opt_u64(&self, key: &str) -> Result<Option<u64>, ProtoError> {
        self.opt_int(key)?.map(|v| to_u64(v, key)).transpose()
    }

    fn u32(&self, key: &str) -> Result<u32, ProtoError> {
        to_u32(self.int(key)?, key)
    }

    fn opt_u32(&self, key: &str) -> Result<Option<u32>, ProtoError> {
        self.opt_int(key)?.map(|v| to_u32(v, key)).transpose()
    }

    fn bool_or(&self, key: &str, default: bool) -> Result<bool, ProtoError> {
        match self.get(key) {
            None => Ok(default),
            Some(Json::Bool(b)) => Ok(*b),
            Some(_) => Err(bad(format!("field '{key}' must be a boolean"))),
        }
    }
}

fn int_of(v: &Json, key: &str) -> Result<i128, ProtoError> {
    v.as_int()
        .ok_or_else(|| bad(format!("field '{key}' must be an integer")))
}

fn out_of_range(key: &str) -> ProtoError {
    bad(format!("field '{key}' out of range"))
}

fn to_u64(v: i128, key: &str) -> Result<u64, ProtoError> {
    u64::try_from(v).map_err(|_| out_of_range(key))
}

fn to_u32(v: i128, key: &str) -> Result<u32, ProtoError> {
    u32::try_from(v).map_err(|_| out_of_range(key))
}

fn price(raw: u64) -> Result<Price, ProtoError> {
    Price::new(raw).map_err(|e| ProtoError {
        code: e.code(),
        message: "price out of range".to_string(),
    })
}

fn qty(raw: u64) -> Result<Qty, ProtoError> {
    Qty::new(raw).map_err(|e| ProtoError {
        code: e.code(),
        message: "quantity out of range".to_string(),
    })
}

/// Decode one request line.
///
/// `max_line_bytes` bounds the size of the JSON text; the caller should
/// already have enforced it while reading, this is a second line of defence.
pub fn parse_request(line: &str, max_line_bytes: usize) -> Result<Request, ProtoError> {
    let doc = json::parse_with(line, &Limits::with_max_bytes(max_line_bytes))?;
    let members = match &doc {
        Json::Obj(m) => m,
        _ => return Err(bad("request must be a JSON object")),
    };
    let f = Fields { members };
    let op = f.str("op")?;
    match op {
        "auth" => {
            f.only(&["op", "token"])?;
            Ok(Request::Auth {
                token: f.str("token")?.to_string(),
            })
        }
        "ping" => {
            f.only(&["op"])?;
            Ok(Request::Ping)
        }
        "summary" => {
            f.only(&["op"])?;
            Ok(Request::Summary)
        }
        "tick" => {
            f.only(&["op"])?;
            Ok(Request::Command(EventBody::Tick))
        }
        "deposit" => {
            f.only(&["op", "account", "amount"])?;
            Ok(Request::Command(EventBody::Deposit {
                account: f.u64("account")?,
                amount: Money(f.int("amount")?),
            }))
        }
        "withdraw" => {
            f.only(&["op", "account", "amount"])?;
            Ok(Request::Command(EventBody::Withdraw {
                account: f.u64("account")?,
                amount: Money(f.int("amount")?),
            }))
        }
        "fund_insurance" => {
            f.only(&["op", "amount"])?;
            Ok(Request::Command(EventBody::FundInsurance {
                amount: Money(f.int("amount")?),
            }))
        }
        "place" => parse_place(&f),
        "cancel" => {
            f.only(&["op", "account", "order_id"])?;
            Ok(Request::Command(EventBody::Cancel {
                account: f.u64("account")?,
                order_id: f.u64("order_id")?,
            }))
        }
        "cancel_all" => {
            f.only(&["op", "account", "market"])?;
            Ok(Request::Command(EventBody::CancelAll {
                account: f.u64("account")?,
                market: f.opt_u32("market")?,
            }))
        }
        "replace" => {
            f.only(&["op", "account", "order_id", "price", "qty"])?;
            let new_price = f.opt_u64("price")?.map(price).transpose()?;
            let new_qty = f.opt_u64("qty")?.map(qty).transpose()?;
            Ok(Request::Command(EventBody::Replace {
                account: f.u64("account")?,
                order_id: f.u64("order_id")?,
                price: new_price,
                qty: new_qty,
            }))
        }
        "oracle" => {
            f.only(&["op", "market", "index_price", "source_ts"])?;
            Ok(Request::Command(EventBody::Oracle {
                market: f.u32("market")?,
                index_price: price(f.u64("index_price")?)?,
                source_ts: f.u64("source_ts")?,
            }))
        }
        "book" => {
            f.only(&["op", "market", "depth"])?;
            let depth = match f.opt_u64("depth")? {
                None => DEFAULT_BOOK_DEPTH,
                Some(d) => usize::try_from(d)
                    .ok()
                    .filter(|d| (1..=MAX_BOOK_DEPTH).contains(d))
                    .ok_or_else(|| out_of_range("depth"))?,
            };
            Ok(Request::Book {
                market: f.u32("market")?,
                depth,
            })
        }
        "account" => {
            f.only(&["op", "account"])?;
            Ok(Request::Account {
                id: f.u64("account")?,
            })
        }
        _ => Err(ProtoError {
            code: "unknown_op",
            message: "unknown op".to_string(),
        }),
    }
}

fn parse_place(f: &Fields<'_>) -> Result<Request, ProtoError> {
    const COMMON: &[&str] = &[
        "op",
        "market",
        "account",
        "side",
        "type",
        "qty",
        "client_id",
        "reduce_only",
        "stp",
    ];
    let kind_name = f.str("type")?;
    let kind = match kind_name {
        "limit" => {
            let mut allowed = COMMON.to_vec();
            allowed.extend_from_slice(&["price", "tif"]);
            f.only(&allowed)?;
            let tif = match f.str("tif")? {
                "gtc" => Tif::Gtc,
                "ioc" => Tif::Ioc,
                "fok" => Tif::Fok,
                "post_only" => Tif::PostOnly,
                _ => return Err(bad("field 'tif' must be gtc, ioc, fok or post_only")),
            };
            OrderKind::Limit {
                price: price(f.u64("price")?)?,
                tif,
            }
        }
        "market" => {
            let mut allowed = COMMON.to_vec();
            allowed.push("max_slippage_bps");
            f.only(&allowed)?;
            OrderKind::Market {
                max_slippage_bps: f.u32("max_slippage_bps")?,
            }
        }
        _ => return Err(bad("field 'type' must be limit or market")),
    };
    let side = match f.str("side")? {
        "buy" => Side::Buy,
        "sell" => Side::Sell,
        _ => return Err(bad("field 'side' must be buy or sell")),
    };
    let stp = match f.get("stp") {
        None => StpMode::CancelNewest,
        Some(v) => match v.as_str() {
            Some("cancel_newest") => StpMode::CancelNewest,
            Some("cancel_oldest") => StpMode::CancelOldest,
            Some("cancel_both") => StpMode::CancelBoth,
            _ => {
                return Err(bad(
                    "field 'stp' must be cancel_newest, cancel_oldest or cancel_both",
                ))
            }
        },
    };
    Ok(Request::Command(EventBody::Place(PlaceOrder {
        market: f.u32("market")?,
        account: f.u64("account")?,
        side,
        kind,
        qty: qty(f.u64("qty")?)?,
        client_id: f.u64("client_id")?,
        reduce_only: f.bool_or("reduce_only", false)?,
        stp,
    })))
}

// ---------------------------------------------------------------------------
// Responses
// ---------------------------------------------------------------------------

/// `{"ok":false,"error":code,"message":message}`; the `message` member is left
/// out when `message` is empty.
pub fn err(code: &str, message: &str) -> String {
    let mut s = String::with_capacity(48 + message.len());
    let mut o = ObjWriter::new(&mut s);
    o.bool("ok", false).str("error", code);
    if !message.is_empty() {
        o.str("message", message);
    }
    o.finish();
    s
}

/// `{"ok":true}`.
pub fn ok_empty() -> String {
    "{\"ok\":true}".to_string()
}

/// `{"ok":true,"pong":true}`.
pub fn pong() -> String {
    "{\"ok\":true,\"pong\":true}".to_string()
}

/// `{"ok":true,"outputs":[...]}` with the default size cap.
pub fn ok(outputs: &[Output]) -> String {
    ok_capped(outputs, DEFAULT_RESPONSE_CAP)
}

/// Like [`ok`] with an explicit cap in bytes. When the serialized line would
/// exceed `cap` the result is a `response_too_large` error instead. Output
/// serialization stops as soon as the cap is passed, so memory use stays
/// within about `cap` plus one output.
pub fn ok_capped(outputs: &[Output], cap: usize) -> String {
    let mut s = String::from("{\"ok\":true,\"outputs\":[");
    for (i, o) in outputs.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        write_output(&mut s, o);
        if s.len() > cap {
            return too_large();
        }
    }
    s.push_str("]}");
    if s.len() > cap {
        return too_large();
    }
    s
}

fn too_large() -> String {
    err(
        "response_too_large",
        "the result does not fit in one response line; the event was applied",
    )
}

fn side_name(side: Side) -> &'static str {
    match side {
        Side::Buy => "buy",
        Side::Sell => "sell",
    }
}

fn reason_name(r: CancelReason) -> &'static str {
    match r {
        CancelReason::User => "user",
        CancelReason::Remainder => "remainder",
        CancelReason::SelfTrade => "self_trade",
        CancelReason::ReduceOnly => "reduce_only",
        CancelReason::BookFull => "book_full",
        CancelReason::Liquidation => "liquidation",
    }
}

/// Append one engine output as a JSON object with a `type` member.
pub fn write_output(s: &mut String, out: &Output) {
    let mut o = ObjWriter::new(s);
    match out {
        Output::Accepted {
            order_id,
            account,
            market,
            client_id,
        } => {
            o.str("type", "accepted")
                .int("order_id", *order_id)
                .int("account", *account)
                .int("market", *market)
                .int("client_id", *client_id);
        }
        Output::Rested {
            order_id,
            qty,
            price,
        } => {
            o.str("type", "rested")
                .int("order_id", *order_id)
                .int("qty", qty.0)
                .int("price", price.0);
        }
        Output::Fill {
            market,
            taker_order,
            maker_order,
            taker,
            maker,
            price,
            qty,
            taker_side,
            taker_fee,
            maker_fee,
            liquidation,
        } => {
            o.str("type", "fill")
                .int("market", *market)
                .int("taker_order", *taker_order)
                .int("maker_order", *maker_order)
                .int("taker", *taker)
                .int("maker", *maker)
                .int("price", price.0)
                .int("qty", qty.0)
                .str("taker_side", side_name(*taker_side))
                .int("taker_fee", taker_fee.0)
                .int("maker_fee", maker_fee.0)
                .bool("liquidation", *liquidation);
        }
        Output::Canceled {
            order_id,
            account,
            qty,
            reason,
        } => {
            o.str("type", "canceled")
                .int("order_id", *order_id)
                .int("account", *account)
                .int("qty", qty.0)
                .str("reason", reason_name(*reason));
        }
        Output::Replaced {
            order_id,
            kept_priority,
        } => {
            o.str("type", "replaced")
                .int("order_id", *order_id)
                .bool("kept_priority", *kept_priority);
        }
        Output::Deposited {
            account,
            amount,
            balance,
        } => {
            o.str("type", "deposited")
                .int("account", *account)
                .int("amount", amount.0)
                .int("balance", balance.0);
        }
        Output::Withdrawn {
            account,
            amount,
            balance,
        } => {
            o.str("type", "withdrawn")
                .int("account", *account)
                .int("amount", amount.0)
                .int("balance", balance.0);
        }
        Output::MarkUpdated {
            market,
            index,
            mark,
            clamped,
        } => {
            o.str("type", "mark_updated")
                .int("market", *market)
                .int("index", index.0)
                .int("mark", mark.0)
                .bool("clamped", *clamped);
        }
        Output::FundingSettled {
            market,
            rate_ppb,
            transferred,
            carry,
        } => {
            o.str("type", "funding_settled")
                .int("market", *market)
                .int("rate_ppb", *rate_ppb)
                .int("transferred", transferred.0)
                .uint128("carry", *carry);
        }
        Output::LiquidationStarted { account, equity } => {
            o.str("type", "liquidation_started")
                .int("account", *account)
                .int("equity", equity.0);
        }
        Output::InsurancePaid { account, amount } => {
            o.str("type", "insurance_paid")
                .int("account", *account)
                .int("amount", amount.0);
        }
        Output::Deleveraged {
            market,
            bankrupt,
            counterparty,
            price,
            qty,
        } => {
            o.str("type", "deleveraged")
                .int("market", *market)
                .int("bankrupt", *bankrupt)
                .int("counterparty", *counterparty)
                .int("price", price.0)
                .int("qty", qty.0);
        }
        Output::InsuranceFunded { amount } => {
            o.str("type", "insurance_funded").int("amount", amount.0);
        }
        Output::FeeTierSet { account, tier } => {
            o.str("type", "fee_tier_set")
                .int("account", *account)
                .int("tier", *tier);
        }
        Output::MarketAdded { market } => {
            o.str("type", "market_added").int("market", *market);
        }
        Output::Configured => {
            o.str("type", "configured");
        }
    }
    o.finish();
}

fn write_levels(s: &mut String, levels: &[LevelView]) {
    s.push('[');
    for (i, l) in levels.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        let mut o = ObjWriter::new(s);
        o.int("price", l.price.0)
            .int("qty", l.qty.0)
            .int("orders", l.orders);
        o.finish();
    }
    s.push(']');
}

/// `{"ok":true,"book":{...}}`.
pub fn book_response(view: &BookView) -> String {
    let mut s = String::from("{\"ok\":true,\"book\":");
    {
        let mut inner = String::new();
        let mut o = ObjWriter::new(&mut inner);
        o.int("market", view.market);
        let mut bids = String::new();
        write_levels(&mut bids, &view.bids);
        let mut asks = String::new();
        write_levels(&mut asks, &view.asks);
        o.raw("bids", &bids).raw("asks", &asks);
        o.finish();
        s.push_str(&inner);
    }
    s.push('}');
    s
}

fn write_position(s: &mut String, p: &PositionView) {
    let mut o = ObjWriter::new(s);
    o.int("market", p.market)
        .int("size", p.size)
        .opt_int("entry_price", p.entry_price.map(|v| v.0))
        .opt_int("mark", p.mark.map(|v| v.0))
        .int("unrealized", p.unrealized.0);
    o.finish();
}

/// `{"ok":true,"account":{...}}`.
pub fn account_response(view: &AccountView) -> String {
    let mut positions = String::from("[");
    for (i, p) in view.positions.iter().enumerate() {
        if i > 0 {
            positions.push(',');
        }
        write_position(&mut positions, p);
    }
    positions.push(']');
    let mut inner = String::new();
    let mut o = ObjWriter::new(&mut inner);
    o.int("id", view.id)
        .int("collateral", view.collateral.0)
        .int("equity", view.equity.0)
        .int("initial_margin", view.initial_margin.0)
        .int("maintenance_margin", view.maintenance_margin.0)
        .int("open_orders", view.open_orders)
        .int("fee_tier", view.fee_tier)
        .raw("positions", &positions);
    o.finish();
    format!("{{\"ok\":true,\"account\":{inner}}}")
}

/// `{"ok":true,"summary":{...}}`.
pub fn summary_response(v: &Summary) -> String {
    let mut inner = String::new();
    let mut o = ObjWriter::new(&mut inner);
    o.int("events_applied", v.events_applied)
        .int("clock", v.clock)
        .int("accounts", v.accounts)
        .int("markets", v.markets)
        .int("open_orders", v.open_orders)
        .int("total_collateral", v.total_collateral.0)
        .int("insurance_fund", v.insurance_fund.0)
        .int("fee_pool", v.fee_pool.0)
        .int("bad_debt", v.bad_debt.0)
        .int("accounts_with_positions", v.accounts_with_positions)
        .int("liquidation_steps", v.liquidation_steps)
        .int("adl_fills", v.adl_fills)
        .int("fills", v.fills)
        .bool("halted", v.halted);
    o.finish();
    format!("{{\"ok\":true,\"summary\":{inner}}}")
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
mod tests {
    use super::*;
    use perp_domain::error::Error;

    const MAXB: usize = 4096;

    fn parse(line: &str) -> Result<Request, ProtoError> {
        parse_request(line, MAXB)
    }

    fn cmd(line: &str) -> EventBody {
        match parse(line).unwrap() {
            Request::Command(b) => b,
            other => panic!("not a command: {other:?}"),
        }
    }

    fn code(line: &str) -> &'static str {
        parse(line).unwrap_err().code
    }

    fn doc(s: &str) -> Json {
        // Responses may hold more than the default 64 array elements.
        let lim = Limits {
            max_array: 100_000,
            max_bytes: 1 << 22,
            ..Limits::default()
        };
        json::parse_with(s, &lim).unwrap()
    }

    #[test]
    fn auth_and_simple_ops() {
        assert_eq!(
            parse(r#"{"op":"auth","token":"abc"}"#).unwrap(),
            Request::Auth {
                token: "abc".into()
            }
        );
        assert_eq!(parse(r#"{"op":"ping"}"#).unwrap(), Request::Ping);
        assert_eq!(parse(r#"{"op":"summary"}"#).unwrap(), Request::Summary);
        assert_eq!(cmd(r#"{"op":"tick"}"#), EventBody::Tick);
    }

    #[test]
    fn auth_debug_never_prints_token() {
        let r = parse(r#"{"op":"auth","token":"hunter2-hunter2"}"#).unwrap();
        let s = format!("{r:?}");
        assert!(!s.contains("hunter2"));
        assert!(s.contains("redacted"));
    }

    #[test]
    fn deposit_withdraw_fund() {
        assert_eq!(
            cmd(r#"{"op":"deposit","account":7,"amount":1000000}"#),
            EventBody::Deposit {
                account: 7,
                amount: Money(1_000_000)
            }
        );
        assert_eq!(
            cmd(r#"{"op":"withdraw","account":7,"amount":5}"#),
            EventBody::Withdraw {
                account: 7,
                amount: Money(5)
            }
        );
        assert_eq!(
            cmd(r#"{"op":"fund_insurance","amount":42}"#),
            EventBody::FundInsurance { amount: Money(42) }
        );
        // Sign is the engine's business, not the parser's.
        assert_eq!(
            cmd(r#"{"op":"deposit","account":1,"amount":-5}"#),
            EventBody::Deposit {
                account: 1,
                amount: Money(-5)
            }
        );
        // Full 128-bit money range passes through.
        assert_eq!(
            cmd(&format!(
                r#"{{"op":"deposit","account":1,"amount":{}}}"#,
                i128::MAX
            )),
            EventBody::Deposit {
                account: 1,
                amount: Money(i128::MAX)
            }
        );
    }

    #[test]
    fn place_limit_all_tifs() {
        for (name, tif) in [
            ("gtc", Tif::Gtc),
            ("ioc", Tif::Ioc),
            ("fok", Tif::Fok),
            ("post_only", Tif::PostOnly),
        ] {
            let line = format!(
                r#"{{"op":"place","market":1,"account":2,"side":"buy","type":"limit","price":50000,"tif":"{name}","qty":10,"client_id":99}}"#
            );
            assert_eq!(
                cmd(&line),
                EventBody::Place(PlaceOrder {
                    market: 1,
                    account: 2,
                    side: Side::Buy,
                    kind: OrderKind::Limit {
                        price: Price(50_000),
                        tif
                    },
                    qty: Qty(10),
                    client_id: 99,
                    reduce_only: false,
                    stp: StpMode::CancelNewest,
                })
            );
        }
    }

    #[test]
    fn place_market_with_options() {
        let line = r#"{"op":"place","market":3,"account":4,"side":"sell","type":"market","max_slippage_bps":50,"qty":7,"client_id":1,"reduce_only":true,"stp":"cancel_both"}"#;
        assert_eq!(
            cmd(line),
            EventBody::Place(PlaceOrder {
                market: 3,
                account: 4,
                side: Side::Sell,
                kind: OrderKind::Market {
                    max_slippage_bps: 50
                },
                qty: Qty(7),
                client_id: 1,
                reduce_only: true,
                stp: StpMode::CancelBoth,
            })
        );
        let line = line.replace("cancel_both", "cancel_oldest");
        match cmd(&line) {
            EventBody::Place(p) => assert_eq!(p.stp, StpMode::CancelOldest),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn place_rejections() {
        let base = |extra: &str| {
            format!(
                r#"{{"op":"place","market":1,"account":2,"side":"buy","type":"limit","price":5,"tif":"gtc","qty":10,"client_id":9{extra}}}"#
            )
        };
        assert!(parse(&base("")).is_ok());
        // Unknown field.
        assert_eq!(code(&base(r#","foo":1"#)), "bad_request");
        // Market-only field on a limit order.
        assert_eq!(code(&base(r#","max_slippage_bps":5"#)), "bad_request");
        // Wrong types.
        assert_eq!(code(&base(r#","reduce_only":1"#)), "bad_request");
        assert_eq!(code(&base(r#","stp":"nope""#)), "bad_request");
        assert_eq!(code(&base(r#","stp":5"#)), "bad_request");
        // Limit-only fields on a market order.
        let mkt = r#"{"op":"place","market":1,"account":2,"side":"buy","type":"market","max_slippage_bps":5,"qty":10,"client_id":9"#;
        assert!(parse(&format!("{mkt}}}")).is_ok());
        assert_eq!(code(&format!(r#"{mkt},"price":5}}"#)), "bad_request");
        assert_eq!(code(&format!(r#"{mkt},"tif":"gtc"}}"#)), "bad_request");
        // Missing fields.
        for (field, fragment) in [
            ("market", r#""market":1,"#),
            ("account", r#""account":2,"#),
            ("side", r#""side":"buy","#),
            ("type", r#""type":"limit","#),
            ("qty", r#""qty":10,"#),
            ("client_id", r#","client_id":9"#),
            ("price", r#""price":5,"#),
            ("tif", r#""tif":"gtc","#),
        ] {
            let line = base("");
            assert!(line.contains(fragment), "{fragment}");
            let removed = line.replacen(fragment, "", 1);
            assert_eq!(code(&removed), "bad_request", "missing {field}");
        }
        // Bad enum values.
        assert_eq!(
            code(&base("").replace("\"buy\"", "\"hold\"")),
            "bad_request"
        );
        assert_eq!(
            code(&base("").replace("\"limit\"", "\"stop\"")),
            "bad_request"
        );
        assert_eq!(code(&base("").replace("\"gtc\"", "\"day\"")), "bad_request");
        assert_eq!(code(&base("").replace("\"buy\"", "1")), "bad_request");
        // Out-of-range conversions.
        assert_eq!(
            code(&base("").replace("\"market\":1", "\"market\":4294967296")),
            "bad_request"
        );
        assert_eq!(
            code(&base("").replace("\"account\":2", "\"account\":-1")),
            "bad_request"
        );
        assert_eq!(
            code(&base("").replace("\"client_id\":9", "\"client_id\":18446744073709551616")),
            "bad_request"
        );
        // Zero or oversized quantity/price carry the engine's code.
        assert_eq!(
            code(&base("").replace("\"qty\":10", "\"qty\":0")),
            "zero_qty"
        );
        assert_eq!(
            code(&base("").replace("\"qty\":10", &format!("\"qty\":{}", u64::MAX))),
            "invalid_qty"
        );
        assert_eq!(
            code(&base("").replace("\"price\":5", "\"price\":0")),
            "invalid_price"
        );
        assert_eq!(
            code(&base("").replace("\"price\":5", &format!("\"price\":{}", u64::MAX))),
            "invalid_price"
        );
        // Float for an integer field.
        assert_eq!(
            code(&base("").replace("\"qty\":10", "\"qty\":1.5")),
            "bad_json"
        );
        // Slippage out of u32 range.
        assert_eq!(
            code(&format!(
                "{}}}",
                mkt.replace("\"max_slippage_bps\":5", "\"max_slippage_bps\":4294967296")
            )),
            "bad_request"
        );
    }

    #[test]
    fn cancel_and_cancel_all_and_replace() {
        assert_eq!(
            cmd(r#"{"op":"cancel","account":1,"order_id":2}"#),
            EventBody::Cancel {
                account: 1,
                order_id: 2
            }
        );
        assert_eq!(
            cmd(r#"{"op":"cancel_all","account":1}"#),
            EventBody::CancelAll {
                account: 1,
                market: None
            }
        );
        assert_eq!(
            cmd(r#"{"op":"cancel_all","account":1,"market":3}"#),
            EventBody::CancelAll {
                account: 1,
                market: Some(3)
            }
        );
        assert_eq!(
            cmd(r#"{"op":"replace","account":1,"order_id":2,"price":300,"qty":4}"#),
            EventBody::Replace {
                account: 1,
                order_id: 2,
                price: Some(Price(300)),
                qty: Some(Qty(4))
            }
        );
        assert_eq!(
            cmd(r#"{"op":"replace","account":1,"order_id":2,"qty":4}"#),
            EventBody::Replace {
                account: 1,
                order_id: 2,
                price: None,
                qty: Some(Qty(4))
            }
        );
        assert_eq!(
            cmd(r#"{"op":"replace","account":1,"order_id":2,"price":300}"#),
            EventBody::Replace {
                account: 1,
                order_id: 2,
                price: Some(Price(300)),
                qty: None
            }
        );
        assert_eq!(
            code(r#"{"op":"replace","account":1,"order_id":2,"price":0}"#),
            "invalid_price"
        );
        assert_eq!(
            code(r#"{"op":"replace","account":1,"order_id":2,"qty":0}"#),
            "zero_qty"
        );
        assert_eq!(code(r#"{"op":"cancel","account":1}"#), "bad_request");
        assert_eq!(
            code(r#"{"op":"cancel_all","account":1,"market":null}"#),
            "bad_request"
        );
    }

    #[test]
    fn oracle() {
        assert_eq!(
            cmd(
                r#"{"op":"oracle","market":1,"index_price":65000000000,"source_ts":1700000000000}"#
            ),
            EventBody::Oracle {
                market: 1,
                index_price: Price(65_000_000_000),
                source_ts: 1_700_000_000_000
            }
        );
        assert_eq!(
            code(r#"{"op":"oracle","market":1,"index_price":0,"source_ts":1}"#),
            "invalid_price"
        );
        assert_eq!(
            code(r#"{"op":"oracle","market":1,"index_price":5}"#),
            "bad_request"
        );
        assert_eq!(
            code(r#"{"op":"oracle","market":1,"index_price":5,"source_ts":-1}"#),
            "bad_request"
        );
    }

    #[test]
    fn book_and_account_queries() {
        assert_eq!(
            parse(r#"{"op":"book","market":2}"#).unwrap(),
            Request::Book {
                market: 2,
                depth: DEFAULT_BOOK_DEPTH
            }
        );
        assert_eq!(
            parse(r#"{"op":"book","market":2,"depth":100}"#).unwrap(),
            Request::Book {
                market: 2,
                depth: 100
            }
        );
        assert_eq!(
            code(r#"{"op":"book","market":2,"depth":101}"#),
            "bad_request"
        );
        assert_eq!(code(r#"{"op":"book","market":2,"depth":0}"#), "bad_request");
        assert_eq!(
            code(r#"{"op":"book","market":2,"depth":-1}"#),
            "bad_request"
        );
        assert_eq!(code(r#"{"op":"book"}"#), "bad_request");
        assert_eq!(
            parse(r#"{"op":"account","account":9}"#).unwrap(),
            Request::Account { id: 9 }
        );
        assert_eq!(code(r#"{"op":"account"}"#), "bad_request");
    }

    #[test]
    fn configure_and_add_market_are_not_available() {
        assert_eq!(code(r#"{"op":"configure"}"#), "unknown_op");
        assert_eq!(code(r#"{"op":"add_market"}"#), "unknown_op");
        assert_eq!(
            code(r#"{"op":"set_fee_tier","account":1,"tier":0}"#),
            "unknown_op"
        );
    }

    #[test]
    fn envelope_rejections() {
        assert_eq!(code("[]"), "bad_request");
        assert_eq!(code("1"), "bad_request");
        assert_eq!(code("null"), "bad_request");
        assert_eq!(code("\"ping\""), "bad_request");
        assert_eq!(code("{}"), "bad_request");
        assert_eq!(code(r#"{"op":1}"#), "bad_request");
        assert_eq!(code(r#"{"op":null}"#), "bad_request");
        assert_eq!(code(r#"{"op":"PING"}"#), "unknown_op");
        assert_eq!(code(r#"{"op":""}"#), "unknown_op");
        assert_eq!(code(""), "bad_json");
        assert_eq!(code("{"), "bad_json");
        assert_eq!(code(r#"{"op":"ping","op":"ping"}"#), "bad_json");
        assert_eq!(code(r#"{"op":"ping"} x"#), "bad_json");
        assert_eq!(code(r#"{"op":"ping","extra":1}"#), "bad_request");
        assert_eq!(code(r#"{"op":"tick","extra":1}"#), "bad_request");
        assert_eq!(code(r#"{"op":"summary","extra":1}"#), "bad_request");
        assert_eq!(code(r#"{"op":"auth"}"#), "bad_request");
        assert_eq!(code(r#"{"op":"auth","token":5}"#), "bad_request");
        assert_eq!(code(r#"{"op":"auth","token":"a","x":1}"#), "bad_request");
    }

    #[test]
    fn line_size_is_enforced() {
        let line = r#"{"op":"ping"}"#;
        assert!(parse_request(line, line.len()).is_ok());
        assert_eq!(
            parse_request(line, line.len() - 1).unwrap_err().code,
            "bad_json"
        );
    }

    #[test]
    fn error_messages_never_echo_input() {
        let secret = "sUpEr-s3cret-t0ken-value-0123456789";
        let lines = [
            format!(r#"{{"op":"auth","token":"{secret}","bogus":1}}"#),
            format!(r#"{{"op":"{secret}"}}"#),
            format!(r#"{{"{secret}":1}}"#),
            format!(r#"{{"op":"place","type":"{secret}"}}"#),
            format!(r#"{{"op":"deposit","account":"{secret}","amount":1}}"#),
            format!(r#"{{"op":"auth","token":"{secret}" x}}"#),
        ];
        for l in lines {
            let e = parse(&l).unwrap_err();
            assert!(!e.message.contains(secret), "{e}");
            assert!(!e.to_string().contains(secret));
        }
    }

    #[test]
    fn hostile_inputs_never_panic() {
        let long_key = format!("{{\"{}\":1}}", "k".repeat(5000));
        let many = format!(
            "{{\"op\":\"ping\"{}}}",
            (0..100).map(|i| format!(",\"f{i}\":1")).collect::<String>()
        );
        let table: Vec<&str> = vec![
            "",
            " ",
            "\u{0}",
            "{",
            "}",
            "{\"op\"",
            "{\"op\":",
            "{\"op\":\"",
            "{\"op\":\"place\"}",
            "{\"op\":\"place\",\"type\":\"limit\"}",
            "{\"op\":\"place\",\"type\":null}",
            "[[[[[[[[[[[[[[[[[[[[",
            "{\"op\":{\"op\":{\"op\":{\"op\":{\"op\":{\"op\":{\"op\":{\"op\":{\"op\":1}}}}}}}}}",
            "{\"op\":\"deposit\",\"account\":1e3,\"amount\":1}",
            "{\"op\":\"deposit\",\"account\":99999999999999999999999999999999999999999,\"amount\":1}",
            "{\"op\":\"deposit\",\"account\":1,\"amount\":\"1\"}",
            "{\"op\":\"deposit\",\"account\":true,\"amount\":1}",
            "{\"op\":\"deposit\",\"account\":[1],\"amount\":1}",
            "{\"op\":\"book\",\"market\":1,\"depth\":18446744073709551615}",
            "{\"op\":\"\\ud800\"}",
            "{\"op\":\"\\u0000\"}",
            "\u{1f600}\u{1f600}",
            &long_key,
            &many,
        ];
        for t in table {
            assert!(parse(t).is_err() || t.contains("\"ping\""), "{t:.40}");
        }
        // Every prefix of a valid place request parses or fails, never panics.
        let full = r#"{"op":"place","market":1,"account":2,"side":"buy","type":"limit","price":5,"tif":"gtc","qty":10,"client_id":9}"#;
        for (i, _) in full.char_indices() {
            assert!(parse(&full[..i]).is_err());
        }
        assert!(parse(full).is_ok());
    }

    // ---------------- responses ----------------

    fn sample_outputs() -> Vec<Output> {
        vec![
            Output::Accepted {
                order_id: 1,
                account: 2,
                market: 3,
                client_id: 4,
            },
            Output::Rested {
                order_id: 1,
                qty: Qty(5),
                price: Price(6),
            },
            Output::Fill {
                market: 1,
                taker_order: 2,
                maker_order: 0,
                taker: 3,
                maker: 4,
                price: Price(5),
                qty: Qty(6),
                taker_side: Side::Sell,
                taker_fee: Money(7),
                maker_fee: Money(-8),
                liquidation: true,
            },
            Output::Canceled {
                order_id: 1,
                account: 2,
                qty: Qty(3),
                reason: CancelReason::User,
            },
            Output::Replaced {
                order_id: 1,
                kept_priority: true,
            },
            Output::Deposited {
                account: 1,
                amount: Money(2),
                balance: Money(3),
            },
            Output::Withdrawn {
                account: 1,
                amount: Money(2),
                balance: Money(1),
            },
            Output::MarkUpdated {
                market: 1,
                index: Price(2),
                mark: Price(3),
                clamped: false,
            },
            Output::FundingSettled {
                market: 1,
                rate_ppb: -5,
                transferred: Money(6),
                carry: 1u128 << 100,
            },
            Output::LiquidationStarted {
                account: 1,
                equity: Money(-2),
            },
            Output::InsurancePaid {
                account: 1,
                amount: Money(2),
            },
            Output::Deleveraged {
                market: 1,
                bankrupt: 2,
                counterparty: 3,
                price: Price(4),
                qty: Qty(5),
            },
            Output::InsuranceFunded { amount: Money(1) },
            Output::FeeTierSet {
                account: 1,
                tier: 2,
            },
            Output::MarketAdded { market: 9 },
            Output::Configured,
        ]
    }

    #[test]
    fn every_output_variant_serializes_to_a_typed_object() {
        let outs = sample_outputs();
        let expected_types = [
            "accepted",
            "rested",
            "fill",
            "canceled",
            "replaced",
            "deposited",
            "withdrawn",
            "mark_updated",
            "funding_settled",
            "liquidation_started",
            "insurance_paid",
            "deleveraged",
            "insurance_funded",
            "fee_tier_set",
            "market_added",
            "configured",
        ];
        assert_eq!(outs.len(), expected_types.len());
        let line = ok(&outs);
        assert!(!line.contains('\n'));
        let d = doc(&line);
        assert_eq!(d.get("ok"), Some(&Json::Bool(true)));
        let arr = d.get("outputs").and_then(Json::as_array).unwrap();
        assert_eq!(arr.len(), outs.len());
        for (o, ty) in arr.iter().zip(expected_types) {
            assert_eq!(o.get("type").and_then(Json::as_str), Some(ty));
        }
    }

    fn field(v: &Json, k: &str) -> i128 {
        v.get(k)
            .and_then(Json::as_int)
            .unwrap_or_else(|| panic!("no int {k}"))
    }

    #[test]
    fn output_fields_carry_the_right_values() {
        let line = ok(&sample_outputs());
        let d = doc(&line);
        let a = d.get("outputs").and_then(Json::as_array).unwrap();
        // accepted
        assert_eq!(field(&a[0], "order_id"), 1);
        assert_eq!(field(&a[0], "account"), 2);
        assert_eq!(field(&a[0], "market"), 3);
        assert_eq!(field(&a[0], "client_id"), 4);
        // rested
        assert_eq!(field(&a[1], "qty"), 5);
        assert_eq!(field(&a[1], "price"), 6);
        // fill
        assert_eq!(field(&a[2], "maker_order"), 0);
        assert_eq!(field(&a[2], "maker_fee"), -8);
        assert_eq!(field(&a[2], "taker_fee"), 7);
        assert_eq!(a[2].get("taker_side").and_then(Json::as_str), Some("sell"));
        assert_eq!(a[2].get("liquidation").and_then(Json::as_bool), Some(true));
        // canceled
        assert_eq!(a[3].get("reason").and_then(Json::as_str), Some("user"));
        // replaced
        assert_eq!(
            a[4].get("kept_priority").and_then(Json::as_bool),
            Some(true)
        );
        // balances
        assert_eq!(field(&a[5], "balance"), 3);
        assert_eq!(field(&a[6], "balance"), 1);
        // mark
        assert_eq!(a[7].get("clamped").and_then(Json::as_bool), Some(false));
        assert_eq!(field(&a[7], "mark"), 3);
        // funding: carry is a bare integer.
        assert_eq!(field(&a[8], "rate_ppb"), -5);
        assert_eq!(field(&a[8], "carry"), 1i128 << 100);
        // liquidation
        assert_eq!(field(&a[9], "equity"), -2);
        assert_eq!(field(&a[10], "amount"), 2);
        assert_eq!(field(&a[11], "counterparty"), 3);
        assert_eq!(field(&a[12], "amount"), 1);
        assert_eq!(field(&a[13], "tier"), 2);
        assert_eq!(field(&a[14], "market"), 9);
    }

    #[test]
    fn money_is_written_as_full_precision_integer() {
        let big = Money(i128::MAX);
        let line = ok(&[Output::Deposited {
            account: u64::MAX,
            amount: big,
            balance: Money(i128::MIN),
        }]);
        assert!(line.contains(&format!("\"amount\":{}", i128::MAX)));
        assert!(line.contains(&format!("\"balance\":{}", i128::MIN)));
        assert!(line.contains(&format!("\"account\":{}", u64::MAX)));
        assert_eq!(
            field(
                &doc(&line).get("outputs").and_then(Json::as_array).unwrap()[0],
                "balance"
            ),
            i128::MIN
        );
    }

    #[test]
    fn every_cancel_reason_has_a_distinct_name() {
        let reasons = [
            CancelReason::User,
            CancelReason::Remainder,
            CancelReason::SelfTrade,
            CancelReason::ReduceOnly,
            CancelReason::BookFull,
            CancelReason::Liquidation,
        ];
        let names: std::collections::HashSet<&str> =
            reasons.iter().map(|r| reason_name(*r)).collect();
        assert_eq!(names.len(), reasons.len());
        assert_eq!(side_name(Side::Buy), "buy");
        assert_eq!(side_name(Side::Sell), "sell");
    }

    #[test]
    fn empty_outputs() {
        assert_eq!(ok(&[]), "{\"ok\":true,\"outputs\":[]}");
    }

    #[test]
    fn response_cap_replaces_large_responses() {
        let outs: Vec<Output> = (0..1000)
            .map(|i| Output::Rested {
                order_id: i,
                qty: Qty(1),
                price: Price(1),
            })
            .collect();
        let full = ok_capped(&outs, usize::MAX);
        assert!(full.len() > 1024);
        let capped = ok_capped(&outs, 1024);
        assert!(capped.len() < 200);
        assert!(capped.contains("response_too_large"));
        assert!(capped.starts_with("{\"ok\":false"));
        // Exactly at the cap passes, one byte under fails.
        assert_eq!(ok_capped(&outs, full.len()), full);
        assert!(ok_capped(&outs, full.len() - 1).contains("response_too_large"));
        // Default cap is large enough for a typical response.
        assert!(ok(&outs[..10]).len() < DEFAULT_RESPONSE_CAP);
    }

    #[test]
    fn err_builder() {
        assert_eq!(
            err("rate_limited", ""),
            r#"{"ok":false,"error":"rate_limited"}"#
        );
        assert_eq!(
            err("bad_request", "field 'x' \"quoted\""),
            r#"{"ok":false,"error":"bad_request","message":"field 'x' \"quoted\""}"#
        );
        assert_eq!(ok_empty(), r#"{"ok":true}"#);
        assert_eq!(pong(), r#"{"ok":true,"pong":true}"#);
        assert_eq!(
            doc(&err("a\nb", "c\"d"))
                .get("error")
                .and_then(Json::as_str),
            Some("a\nb")
        );
    }

    #[test]
    fn engine_error_codes_fit_the_error_shape() {
        let e = Error::MarginBreach;
        let line = err(e.code(), &e.to_string());
        let d = doc(&line);
        assert_eq!(d.get("ok"), Some(&Json::Bool(false)));
        assert_eq!(d.get("error").and_then(Json::as_str), Some("margin_breach"));
    }

    #[test]
    fn book_view_json() {
        let v = BookView {
            market: 4,
            bids: vec![
                LevelView {
                    price: Price(100),
                    qty: Qty(5),
                    orders: 2,
                },
                LevelView {
                    price: Price(99),
                    qty: Qty(1),
                    orders: 1,
                },
            ],
            asks: vec![],
        };
        let line = book_response(&v);
        let d = doc(&line);
        let b = d.get("book").unwrap();
        assert_eq!(field(b, "market"), 4);
        let bids = b.get("bids").and_then(Json::as_array).unwrap();
        assert_eq!(bids.len(), 2);
        assert_eq!(field(&bids[0], "price"), 100);
        assert_eq!(field(&bids[0], "orders"), 2);
        assert_eq!(b.get("asks").and_then(Json::as_array).unwrap().len(), 0);
    }

    #[test]
    fn account_view_json() {
        let v = AccountView {
            id: 7,
            collateral: Money(1000),
            equity: Money(1010),
            initial_margin: Money(50),
            maintenance_margin: Money(25),
            open_orders: 3,
            fee_tier: 1,
            positions: vec![
                PositionView {
                    market: 1,
                    size: -5,
                    entry_price: Some(Price(100)),
                    mark: Some(Price(101)),
                    unrealized: Money(-5),
                },
                PositionView {
                    market: 2,
                    size: 0,
                    entry_price: None,
                    mark: None,
                    unrealized: Money(0),
                },
            ],
        };
        let d = doc(&account_response(&v));
        let a = d.get("account").unwrap();
        assert_eq!(field(a, "id"), 7);
        assert_eq!(field(a, "equity"), 1010);
        assert_eq!(field(a, "maintenance_margin"), 25);
        assert_eq!(field(a, "open_orders"), 3);
        assert_eq!(field(a, "fee_tier"), 1);
        let ps = a.get("positions").and_then(Json::as_array).unwrap();
        assert_eq!(field(&ps[0], "size"), -5);
        assert_eq!(field(&ps[0], "entry_price"), 100);
        assert_eq!(ps[1].get("entry_price"), Some(&Json::Null));
        assert_eq!(ps[1].get("mark"), Some(&Json::Null));
    }

    #[test]
    fn summary_json() {
        let s = Summary {
            events_applied: 10,
            clock: 20,
            accounts: 3,
            markets: 2,
            open_orders: 4,
            total_collateral: Money(5),
            insurance_fund: Money(6),
            fee_pool: Money(7),
            bad_debt: Money(0),
            accounts_with_positions: 1,
            liquidation_steps: 8,
            adl_fills: 9,
            fills: 11,
            halted: true,
        };
        let d = doc(&summary_response(&s));
        let v = d.get("summary").unwrap();
        assert_eq!(field(v, "events_applied"), 10);
        assert_eq!(field(v, "clock"), 20);
        assert_eq!(field(v, "insurance_fund"), 6);
        assert_eq!(field(v, "bad_debt"), 0);
        assert_eq!(field(v, "fills"), 11);
        assert_eq!(v.get("halted").and_then(Json::as_bool), Some(true));
        assert_eq!(v.as_object().unwrap().len(), 14);
    }

    #[test]
    fn funding_carry_beyond_i128_is_written_as_a_bare_integer() {
        let mut s = String::new();
        write_output(
            &mut s,
            &Output::FundingSettled {
                market: 1,
                rate_ppb: 0,
                transferred: Money(0),
                carry: u128::MAX,
            },
        );
        assert!(s.contains("\"carry\":340282366920938463463374607431768211455}"));
    }
}
