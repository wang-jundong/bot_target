//! Unified strategy engine wrapper for runtime / execution / state.

use crate::config::{StrategyName, StrategyV011Config, StrategyV022Config, StrategyV031Config};
use crate::strategy::common::MarketEvent;
use crate::strategy::v011::{Decision, StrategyV011Engine};
use crate::strategy::v022::StrategyV022Engine;
use crate::strategy::v031::StrategyV031Engine;

pub enum Engine {
    V011(StrategyV011Engine),
    V022(StrategyV022Engine),
    V031(StrategyV031Engine),
}

impl Engine {
    pub fn new_v011(cfg: StrategyV011Config) -> Self {
        Self::V011(StrategyV011Engine::new(cfg))
    }

    pub fn new_v022(cfg: StrategyV022Config, own_wallet: impl Into<String>) -> Self {
        Self::V022(StrategyV022Engine::new(cfg, own_wallet))
    }

    pub fn new_v031(cfg: StrategyV031Config, own_wallet: impl Into<String>) -> Self {
        Self::V031(StrategyV031Engine::new(cfg, own_wallet))
    }

    pub fn name(&self) -> StrategyName {
        match self {
            Self::V011(_) => StrategyName::V011,
            Self::V022(_) => StrategyName::V022,
            Self::V031(_) => StrategyName::V031,
        }
    }

    pub fn strategy_name(&self) -> &'static str {
        self.name().as_str()
    }

    pub fn phase_name(&self) -> String {
        match self {
            Self::V011(e) => e.phase_name().to_string(),
            Self::V022(e) => e.phase_name().to_string(),
            Self::V031(e) => e.phase_name().to_string(),
        }
    }

    pub fn needs_pool_tape(&self) -> bool {
        match self {
            Self::V011(e) => e.needs_pool_tape(),
            Self::V022(e) => e.needs_pool_tape(),
            Self::V031(e) => e.needs_pool_tape(),
        }
    }

    pub fn is_bound(&self) -> bool {
        match self {
            Self::V011(e) => e.phase_name() != crate::strategy::v011::PHASE_UNBOUND,
            Self::V022(e) => e.is_bound(),
            Self::V031(e) => e.is_bound(),
        }
    }

    pub fn is_done(&self) -> bool {
        match self {
            Self::V011(e) => e.is_done(),
            Self::V022(_) => false,
            Self::V031(e) => e.is_done(),
        }
    }

    pub fn last_buy_reason(&self) -> String {
        match self {
            Self::V011(e) => e.last_buy_reason(),
            Self::V022(e) => e.last_buy_reason(),
            Self::V031(e) => e.last_buy_reason(),
        }
    }

    pub fn last_sell_reason(&self) -> String {
        match self {
            Self::V011(e) => e.last_sell_reason(),
            Self::V022(e) => e.last_sell_reason(),
            Self::V031(e) => e.last_sell_reason(),
        }
    }

    pub fn buy_size_sol(&self) -> f64 {
        match self {
            Self::V011(e) => e.buy_size_sol(),
            Self::V022(e) => e.buy_size_sol(),
            Self::V031(e) => e.buy_size_sol(),
        }
    }

    pub fn timer_ms(&self) -> u64 {
        match self {
            Self::V011(e) => e.timer_ms(),
            Self::V022(_) | Self::V031(_) => 1_000,
        }
    }

    pub fn on_buy_fill(&mut self, fill_px: f64, now_ms: u64) -> Option<String> {
        match self {
            Self::V011(e) => e.on_buy_fill(fill_px),
            Self::V022(e) => {
                e.on_buy_fill(fill_px);
                None
            }
            Self::V031(e) => {
                e.on_buy_fill(fill_px, now_ms);
                None
            }
        }
    }

    pub fn apply_missed_target_sell(&mut self) {
        match self {
            Self::V011(_) => {}
            Self::V022(e) => e.apply_missed_target_sell(),
            Self::V031(e) => e.apply_missed_target_sell(),
        }
    }

    pub fn on_buy_failed(&mut self) {
        match self {
            Self::V011(e) => e.on_buy_failed(),
            Self::V022(e) => e.on_buy_failed(),
            Self::V031(e) => e.on_buy_failed(),
        }
    }

    pub fn on_sell_fill(&mut self) {
        match self {
            Self::V011(e) => e.on_sell_fill(),
            Self::V022(e) => e.on_sell_fill(),
            Self::V031(e) => e.on_sell_fill(),
        }
    }

    pub fn on_sell_failed(&mut self, now_ms: u64) {
        match self {
            Self::V011(e) => e.on_sell_failed(now_ms),
            Self::V022(e) => e.on_sell_failed(now_ms),
            Self::V031(e) => e.on_sell_failed(now_ms),
        }
    }

    pub fn restore_hold(&mut self, entry_px: f64, now_ms: u64) {
        match self {
            Self::V011(e) => {
                let _ = e.on_buy_fill(entry_px);
            }
            Self::V022(e) => e.restore_hold(entry_px),
            Self::V031(e) => e.restore_hold(entry_px, now_ms),
        }
    }

    /// Bind on the gate buy (first target buy that opens tracking).
    pub fn bind_gate(
        &mut self,
        wallet: &str,
        token_amount: f64,
        price: f64,
        sol: f64,
        gate_sig: &str,
        now_ms: u64,
    ) {
        match self {
            Self::V011(e) => {
                let _ = e.bind_gate_buy(price, sol, wallet, gate_sig, token_amount, now_ms);
            }
            Self::V022(e) => e.bind_gate(wallet, token_amount),
            Self::V031(e) => e.bind_gate(wallet, token_amount, price, now_ms),
        }
    }

    /// Process a non-gate pool print. Returns BUY/SELL when the engine fires.
    pub fn on_event(&mut self, event: &MarketEvent, holding: bool) -> Option<&'static str> {
        match self {
            Self::V011(e) => match e.on_event(event, holding) {
                Decision::FireBuy { .. } => Some("BUY"),
                Decision::FireSell { .. } => Some("SELL"),
                Decision::Skip { .. } | Decision::None => None,
            },
            Self::V022(e) => e.on_event(event),
            Self::V031(e) => e.on_event(event),
        }
    }

    pub fn on_clock(&mut self, now_ms: u64, mark_px: f64, holding: bool) -> Option<&'static str> {
        match self {
            Self::V011(e) => {
                if e.is_done() || !e.timer_wants_ticks() {
                    return None;
                }
                let px = if mark_px > 0.0 { mark_px } else { e.last_mark_px() };
                match e.on_timer(px, now_ms) {
                    Decision::FireBuy { .. } => Some("BUY"),
                    Decision::FireSell { .. } => Some("SELL"),
                    Decision::Skip { .. } | Decision::None => None,
                }
            }
            Self::V022(e) => {
                if !holding {
                    return None;
                }
                e.on_clock(now_ms)
            }
            Self::V031(e) => {
                if !holding {
                    return None;
                }
                e.on_clock(now_ms, mark_px)
            }
        }
    }
}
