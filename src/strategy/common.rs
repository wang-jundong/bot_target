//! Shared market-event and chain-cursor helpers for strategy engines.

pub const MAX_SELL_FRAC: f64 = 0.80;
pub const SELL_RETRY_MS: u64 = 2_000;

#[derive(Debug, Clone)]
pub struct MarketEvent {
    pub signature: String,
    pub slot: u64,
    pub transaction_index: Option<u64>,
    pub event_index: Option<u32>,
    pub timestamp_ms: u64,
    pub side: &'static str, // "BUY" | "SELL"
    pub wallet: String,
    pub sol_amount: f64,
    pub token_amount: f64,
    pub price: f64, // strategy price units
}

#[derive(Clone, Copy, Debug)]
pub struct ChainCursor {
    pub slot: u64,
    pub transaction_index: u64,
    pub event_index: u32,
}

pub fn cursor_of(event: &MarketEvent) -> ChainCursor {
    ChainCursor {
        slot: event.slot,
        transaction_index: event.transaction_index.unwrap_or(u64::MAX),
        event_index: event.event_index.unwrap_or(0),
    }
}

pub fn is_strictly_after(next: ChainCursor, prev: ChainCursor) -> bool {
    if next.slot != prev.slot {
        return next.slot > prev.slot;
    }
    if next.transaction_index != prev.transaction_index {
        return next.transaction_index > prev.transaction_index;
    }
    next.event_index > prev.event_index
}

pub fn is_max_sell(held: f64, sold: f64) -> bool {
    if sold <= 0.0 {
        return false;
    }
    if held <= 0.0 {
        return true;
    }
    sold >= held * MAX_SELL_FRAC
}

pub fn round(x: f64, ndigits: u32) -> f64 {
    let f = 10f64.powi(ndigits as i32);
    (x * f + f64::EPSILON).round() / f
}
