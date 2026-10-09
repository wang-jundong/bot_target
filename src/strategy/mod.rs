pub mod common;
pub mod engine;
pub mod v011;
pub mod v022;
pub mod v031;

use crate::events::{lamports_to_sol, to_strategy_price, PoolTradeEvent, Side};
use common::MarketEvent;

pub fn to_market_event(event: &PoolTradeEvent) -> MarketEvent {
    MarketEvent {
        signature: event.signature.clone(),
        slot: event.slot,
        transaction_index: event.transaction_index,
        event_index: Some(event.event_index),
        timestamp_ms: event.timestamp_ms,
        side: match event.side {
            Side::Buy => "BUY",
            Side::Sell => "SELL",
        },
        wallet: event.trader.clone(),
        sol_amount: lamports_to_sol(event.sol_amount),
        token_amount: event.token_amount as f64,
        price: to_strategy_price(event.price),
    }
}
