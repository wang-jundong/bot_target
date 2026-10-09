use crate::events::{compare_events, event_key, pool_key, PoolDescriptor, PoolTradeEvent, Side};
use crate::journal::PositionPrices;
use crate::strategy::engine::Engine;
use anyhow::{bail, Result};
use solana_sdk::instruction::Instruction;
use solana_sdk::pubkey::Pubkey;
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lifecycle {
    TargetBuyDetected,
    TrackingPool,
    BuyPrepared,
    BuySent,
    BuyProcessed,
    PositionActiveUnconfirmed,
    BuyConfirmed,
    PositionActiveConfirmed,
    SellPrepared,
    SellSent,
    Closed,
    Failed,
}

impl Lifecycle {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::TargetBuyDetected => "TARGET_BUY_DETECTED",
            Self::TrackingPool => "TRACKING_POOL",
            Self::BuyPrepared => "BUY_PREPARED",
            Self::BuySent => "BUY_SENT",
            Self::BuyProcessed => "BUY_PROCESSED",
            Self::PositionActiveUnconfirmed => "POSITION_ACTIVE_UNCONFIRMED",
            Self::BuyConfirmed => "BUY_CONFIRMED",
            Self::PositionActiveConfirmed => "POSITION_ACTIVE_CONFIRMED",
            Self::SellPrepared => "SELL_PREPARED",
            Self::SellSent => "SELL_SENT",
            Self::Closed => "CLOSED",
            Self::Failed => "FAILED",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "TARGET_BUY_DETECTED" => Self::TargetBuyDetected,
            "TRACKING_POOL" => Self::TrackingPool,
            "BUY_PREPARED" => Self::BuyPrepared,
            "BUY_SENT" => Self::BuySent,
            "BUY_PROCESSED" => Self::BuyProcessed,
            "POSITION_ACTIVE_UNCONFIRMED" => Self::PositionActiveUnconfirmed,
            "BUY_CONFIRMED" => Self::BuyConfirmed,
            "POSITION_ACTIVE_CONFIRMED" => Self::PositionActiveConfirmed,
            "SELL_PREPARED" => Self::SellPrepared,
            "SELL_SENT" => Self::SellSent,
            "CLOSED" => Self::Closed,
            "FAILED" => Self::Failed,
            _ => return None,
        })
    }
}

pub fn assert_transition(from: Lifecycle, to: Lifecycle) -> Result<()> {
    let ok = match from {
        Lifecycle::TargetBuyDetected => matches!(to, Lifecycle::TrackingPool | Lifecycle::Failed),
        Lifecycle::TrackingPool => matches!(to, Lifecycle::BuyPrepared | Lifecycle::Closed | Lifecycle::Failed),
        Lifecycle::BuyPrepared => matches!(to, Lifecycle::BuySent | Lifecycle::TrackingPool | Lifecycle::Failed),
        Lifecycle::BuySent => matches!(to, Lifecycle::BuyProcessed | Lifecycle::TrackingPool | Lifecycle::Failed),
        Lifecycle::BuyProcessed => matches!(to, Lifecycle::PositionActiveUnconfirmed | Lifecycle::Failed),
        Lifecycle::PositionActiveUnconfirmed => matches!(to, Lifecycle::BuyConfirmed | Lifecycle::SellPrepared | Lifecycle::Failed),
        Lifecycle::BuyConfirmed => matches!(to, Lifecycle::PositionActiveConfirmed | Lifecycle::Failed),
        Lifecycle::PositionActiveConfirmed => matches!(to, Lifecycle::SellPrepared | Lifecycle::SellSent | Lifecycle::Failed),
        Lifecycle::SellPrepared => matches!(to, Lifecycle::BuyConfirmed | Lifecycle::PositionActiveConfirmed | Lifecycle::SellSent | Lifecycle::TrackingPool | Lifecycle::Failed),
        Lifecycle::SellSent => matches!(
            to,
            Lifecycle::Closed | Lifecycle::TrackingPool | Lifecycle::PositionActiveConfirmed | Lifecycle::BuyPrepared | Lifecycle::Failed
        ),
        Lifecycle::Closed | Lifecycle::Failed => false,
    };
    if !ok {
        bail!("invalid lifecycle transition {} -> {}", from.as_str(), to.as_str());
    }
    Ok(())
}

pub fn is_holding(lifecycle: Lifecycle) -> bool {
    matches!(
        lifecycle,
        Lifecycle::PositionActiveUnconfirmed
            | Lifecycle::PositionActiveConfirmed
            | Lifecycle::BuyConfirmed
            | Lifecycle::SellPrepared
            | Lifecycle::BuyProcessed
    )
}

#[derive(Clone)]
pub struct PreparedTrade {
    pub instructions: Vec<Instruction>,
    pub payer: Pubkey,
    pub compute_unit_limit: u32,
    pub priority_fee_lamports: u64,
    pub tip_lamports: u64,
}

pub struct EventBuffer {
    events: Vec<PoolTradeEvent>,
    keys: HashSet<String>,
    retention_ms: u64,
}

impl EventBuffer {
    pub fn new(retention_ms: u64) -> Self {
        Self { events: Vec::new(), keys: HashSet::new(), retention_ms }
    }

    pub fn add(&mut self, event: PoolTradeEvent) -> bool {
        let key = event_key(&event.signature, event.event_index);
        if !self.keys.insert(key) {
            return false;
        }
        let insert_at = self.events.iter().position(|existing| compare_events(existing, &event).is_gt()).unwrap_or(self.events.len());
        let cutoff = event.timestamp_ms.saturating_sub(self.retention_ms);
        self.events.insert(insert_at, event);
        let mut count = 0;
        while count < self.events.len() && self.events[count].timestamp_ms < cutoff {
            self.keys.remove(&event_key(&self.events[count].signature, self.events[count].event_index));
            count += 1;
        }
        if count > 0 {
            self.events.drain(0..count);
        }
        true
    }
}

pub struct TokenInner {
    pub descriptor: PoolDescriptor,
    pub lifecycle: Lifecycle,
    pub events: EventBuffer,
    pub target_buy: PoolTradeEvent,
    pub target_observed_token_amount: u64,
    target_trades: HashSet<String>,
    pub prepared_buy: Option<PreparedTrade>,
    pub prepared_sell: Option<PreparedTrade>,
    pub buy_signature: Option<String>,
    pub sell_signature: Option<String>,
    pub actual_token_amount: Option<u64>,
    pub actual_entry_sol_amount: Option<u64>,
    pub buy_lamports: Option<u64>,
    pub prices: PositionPrices,
    pub buy_send_claimed: bool,
    pub sell_send_claimed: bool,
    pub entry_processed_ms: Option<u64>,
    pub buy_slippage_bps: Option<u64>,
    pub sell_slippage_bps: Option<u64>,
    pub engine: Engine,
    pub strategy_name: String,
    pub sell_after_fill: bool,
    pub pool_tape_needed: Option<bool>,
}

impl TokenInner {
    pub fn new(descriptor: PoolDescriptor, target_buy: PoolTradeEvent, retention_ms: u64, engine: Engine) -> Self {
        let strategy_name = engine.strategy_name().to_string();
        let amount = target_buy.token_amount;
        let mut target_trades = HashSet::new();
        target_trades.insert(event_key(&target_buy.signature, target_buy.event_index));
        Self {
            descriptor,
            lifecycle: Lifecycle::TargetBuyDetected,
            events: EventBuffer::new(retention_ms),
            target_buy,
            target_observed_token_amount: amount,
            target_trades,
            prepared_buy: None,
            prepared_sell: None,
            buy_signature: None,
            sell_signature: None,
            actual_token_amount: None,
            actual_entry_sol_amount: None,
            buy_lamports: None,
            prices: PositionPrices::default(),
            buy_send_claimed: false,
            sell_send_claimed: false,
            entry_processed_ms: None,
            buy_slippage_bps: None,
            sell_slippage_bps: None,
            engine,
            strategy_name,
            sell_after_fill: false,
            pool_tape_needed: None,
        }
    }

    pub fn record_target_trade(&mut self, event: &PoolTradeEvent) -> bool {
        if event.trader != self.target_buy.trader {
            return false;
        }
        let key = event_key(&event.signature, event.event_index);
        if !self.target_trades.insert(key) {
            return false;
        }
        if event.side == Side::Buy {
            self.target_observed_token_amount = self.target_observed_token_amount.saturating_add(event.token_amount);
        } else if event.token_amount >= self.target_observed_token_amount {
            self.target_observed_token_amount = 0;
        } else {
            self.target_observed_token_amount -= event.token_amount;
        }
        true
    }

    pub fn transition(&mut self, next: Lifecycle) -> Result<()> {
        if next == self.lifecycle {
            return Ok(());
        }
        assert_transition(self.lifecycle, next)?;
        self.lifecycle = next;
        Ok(())
    }

    pub fn restore_position(&mut self, token_amount: u64, entry_price: f64, current_price: f64, entry_processed_ms: u64) {
        self.actual_token_amount = Some(token_amount);
        self.prices.actual_entry_fill_price = Some(entry_price);
        self.prices.current_mark_price = Some(current_price);
        self.entry_processed_ms = Some(entry_processed_ms);
        self.lifecycle = Lifecycle::PositionActiveConfirmed;
    }

    pub fn clear_filled_position(&mut self) {
        self.actual_token_amount = None;
        self.actual_entry_sol_amount = None;
        self.buy_lamports = None;
        self.prepared_buy = None;
        self.prepared_sell = None;
        self.buy_signature = None;
        self.sell_signature = None;
        self.entry_processed_ms = None;
        self.buy_slippage_bps = None;
        self.sell_slippage_bps = None;
        self.prices = PositionPrices::default();
    }

    pub fn claim_buy_send(&mut self) -> bool {
        if self.buy_send_claimed {
            return false;
        }
        self.buy_send_claimed = true;
        true
    }

    pub fn reset_buy_send_claim(&mut self) {
        self.buy_send_claimed = false;
    }

    pub fn claim_sell_send(&mut self) -> bool {
        if self.sell_send_claimed {
            return false;
        }
        self.sell_send_claimed = true;
        true
    }

    pub fn release_sell_send(&mut self) {
        self.sell_send_claimed = false;
    }

    pub fn reset_sell_send_claim(&mut self) {
        self.sell_send_claimed = false;
    }

    pub fn key(&self) -> String {
        pool_key(&self.descriptor.program_id, &self.descriptor.pool)
    }
}

pub struct TokenStateManager {
    states: HashMap<String, usize>,
    retention_ms: u64,
}

impl TokenStateManager {
    pub fn new(retention_ms: u64) -> Self {
        Self { states: HashMap::new(), retention_ms }
    }

    pub fn retention_ms(&self) -> u64 {
        self.retention_ms
    }

    pub fn insert_key(&mut self, key: String, index: usize) {
        self.states.insert(key, index);
    }

    pub fn get_index(&self, program_id: &str, pool: &str) -> Option<usize> {
        self.states.get(&pool_key(program_id, pool)).copied()
    }

    pub fn remove(&mut self, program_id: &str, pool: &str) -> Option<usize> {
        self.states.remove(&pool_key(program_id, pool))
    }
}
