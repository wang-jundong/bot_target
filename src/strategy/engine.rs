//! Unified strategy engine wrapper for runtime / execution / state.

use crate::config::{
    StrategyName, StrategyV011Config, StrategyV022Config, StrategyV031Config, StrategyV051Config,
    StrategyV061Config, StrategyV071Config, StrategyV081Config, StrategyV101Config,
    StrategyV121Config, StrategyV131Config,
};
use crate::strategy::common::MarketEvent;
use crate::strategy::v011::{Decision, StrategyV011Engine};
use crate::strategy::v022::StrategyV022Engine;
use crate::strategy::v031::StrategyV031Engine;
use crate::strategy::v051::StrategyV051Engine;
use crate::strategy::v061::StrategyV061Engine;
use crate::strategy::v071::StrategyV071Engine;
use crate::strategy::v081::StrategyV081Engine;
use crate::strategy::v101::StrategyV101Engine;
use crate::strategy::v121::StrategyV121Engine;
use crate::strategy::v131::StrategyV131Engine;

pub enum Engine {
    V011(StrategyV011Engine),
    V022(StrategyV022Engine),
    V031(StrategyV031Engine),
    V051(StrategyV051Engine),
    V061(StrategyV061Engine),
    V071(StrategyV071Engine),
    V081(StrategyV081Engine),
    V101(StrategyV101Engine),
    V121(StrategyV121Engine),
    V131(StrategyV131Engine),
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

    pub fn new_v051(cfg: StrategyV051Config, own_wallet: impl Into<String>) -> Self {
        Self::V051(StrategyV051Engine::new(cfg, own_wallet))
    }

    pub fn new_v061(cfg: StrategyV061Config, own_wallet: impl Into<String>) -> Self {
        Self::V061(StrategyV061Engine::new(cfg, own_wallet))
    }

    pub fn new_v071(cfg: StrategyV071Config, own_wallet: impl Into<String>) -> Self {
        Self::V071(StrategyV071Engine::new(cfg, own_wallet))
    }

    pub fn new_v081(cfg: StrategyV081Config, own_wallet: impl Into<String>) -> Self {
        Self::V081(StrategyV081Engine::new(cfg, own_wallet))
    }

    pub fn new_v101(cfg: StrategyV101Config, own_wallet: impl Into<String>) -> Self {
        Self::V101(StrategyV101Engine::new(cfg, own_wallet))
    }

    pub fn new_v121(cfg: StrategyV121Config, own_wallet: impl Into<String>) -> Self {
        Self::V121(StrategyV121Engine::new(cfg, own_wallet))
    }

    pub fn new_v131(cfg: StrategyV131Config, own_wallet: impl Into<String>) -> Self {
        Self::V131(StrategyV131Engine::new(cfg, own_wallet))
    }

    pub fn name(&self) -> StrategyName {
        match self {
            Self::V011(_) => StrategyName::V011,
            Self::V022(_) => StrategyName::V022,
            Self::V031(_) => StrategyName::V031,
            Self::V051(_) => StrategyName::V051,
            Self::V061(_) => StrategyName::V061,
            Self::V071(_) => StrategyName::V071,
            Self::V081(_) => StrategyName::V081,
            Self::V101(_) => StrategyName::V101,
            Self::V121(_) => StrategyName::V121,
            Self::V131(_) => StrategyName::V131,
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
            Self::V051(e) => e.phase_name().to_string(),
            Self::V061(e) => e.phase_name().to_string(),
            Self::V071(e) => e.phase_name().to_string(),
            Self::V081(e) => e.phase_name().to_string(),
            Self::V101(e) => e.phase_name().to_string(),
            Self::V121(e) => e.phase_name().to_string(),
            Self::V131(e) => e.phase_name().to_string(),
        }
    }

    pub fn needs_pool_tape(&self) -> bool {
        match self {
            Self::V011(e) => e.needs_pool_tape(),
            Self::V022(e) => e.needs_pool_tape(),
            Self::V031(e) => e.needs_pool_tape(),
            Self::V051(e) => e.needs_pool_tape(),
            Self::V061(e) => e.needs_pool_tape(),
            Self::V071(e) => e.needs_pool_tape(),
            Self::V081(e) => e.needs_pool_tape(),
            Self::V101(e) => e.needs_pool_tape(),
            Self::V121(e) => e.needs_pool_tape(),
            Self::V131(e) => e.needs_pool_tape(),
        }
    }

    pub fn is_bound(&self) -> bool {
        match self {
            Self::V011(e) => e.phase_name() != crate::strategy::v011::PHASE_UNBOUND,
            Self::V022(e) => e.is_bound(),
            Self::V031(e) => e.is_bound(),
            Self::V051(e) => e.is_bound(),
            Self::V061(e) => e.is_bound(),
            Self::V071(e) => e.is_bound(),
            Self::V081(e) => e.is_bound(),
            Self::V101(e) => e.is_bound(),
            Self::V121(e) => e.is_bound(),
            Self::V131(e) => e.is_bound(),
        }
    }

    pub fn is_done(&self) -> bool {
        match self {
            Self::V011(e) => e.is_done(),
            Self::V022(_) => false,
            Self::V031(e) => e.is_done(),
            Self::V051(e) => e.is_done(),
            Self::V061(e) => e.is_done(),
            Self::V071(e) => e.is_done(),
            Self::V081(e) => e.is_done(),
            Self::V101(e) => e.is_done(),
            Self::V121(e) => e.is_done(),
            Self::V131(e) => e.is_done(),
        }
    }

    pub fn last_buy_reason(&self) -> String {
        match self {
            Self::V011(e) => e.last_buy_reason(),
            Self::V022(e) => e.last_buy_reason(),
            Self::V031(e) => e.last_buy_reason(),
            Self::V051(e) => e.last_buy_reason(),
            Self::V061(e) => e.last_buy_reason(),
            Self::V071(e) => e.last_buy_reason(),
            Self::V081(e) => e.last_buy_reason(),
            Self::V101(e) => e.last_buy_reason(),
            Self::V121(e) => e.last_buy_reason(),
            Self::V131(e) => e.last_buy_reason(),
        }
    }

    pub fn last_sell_reason(&self) -> String {
        match self {
            Self::V011(e) => e.last_sell_reason(),
            Self::V022(e) => e.last_sell_reason(),
            Self::V031(e) => e.last_sell_reason(),
            Self::V051(e) => e.last_sell_reason(),
            Self::V061(e) => e.last_sell_reason(),
            Self::V071(e) => e.last_sell_reason(),
            Self::V081(e) => e.last_sell_reason(),
            Self::V101(e) => e.last_sell_reason(),
            Self::V121(e) => e.last_sell_reason(),
            Self::V131(e) => e.last_sell_reason(),
        }
    }

    pub fn buy_size_sol(&self) -> f64 {
        match self {
            Self::V011(e) => e.buy_size_sol(),
            Self::V022(e) => e.buy_size_sol(),
            Self::V031(e) => e.buy_size_sol(),
            Self::V051(e) => e.buy_size_sol(),
            Self::V061(e) => e.buy_size_sol(),
            Self::V071(e) => e.buy_size_sol(),
            Self::V081(e) => e.buy_size_sol(),
            Self::V101(e) => e.buy_size_sol(),
            Self::V121(e) => e.buy_size_sol(),
            Self::V131(e) => e.buy_size_sol(),
        }
    }

    pub fn timer_ms(&self) -> u64 {
        match self {
            Self::V011(e) => e.timer_ms(),
            Self::V022(_)
            | Self::V031(_)
            | Self::V051(_)
            | Self::V061(_)
            | Self::V071(_)
            | Self::V081(_)
            | Self::V101(_)
            | Self::V121(_)
            | Self::V131(_) => 1_000,
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
            Self::V051(e) => {
                e.on_buy_fill(fill_px, now_ms);
                None
            }
            Self::V061(e) => {
                e.on_buy_fill(fill_px, now_ms);
                None
            }
            Self::V071(e) => {
                e.on_buy_fill(fill_px, now_ms);
                None
            }
            Self::V081(e) => {
                e.on_buy_fill(fill_px, now_ms);
                None
            }
            Self::V101(e) => {
                e.on_buy_fill(fill_px, now_ms);
                None
            }
            Self::V121(e) => {
                e.on_buy_fill(fill_px, now_ms);
                None
            }
            Self::V131(e) => {
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
            Self::V051(e) => e.apply_missed_target_sell(),
            Self::V061(e) => e.apply_missed_target_sell(),
            Self::V071(e) => e.apply_missed_target_sell(),
            Self::V081(e) => e.apply_missed_target_sell(),
            Self::V101(e) => e.apply_missed_target_sell(),
            Self::V121(e) => e.apply_missed_target_sell(),
            Self::V131(e) => e.apply_missed_target_sell(),
        }
    }

    pub fn on_buy_failed(&mut self) {
        match self {
            Self::V011(e) => e.on_buy_failed(),
            Self::V022(e) => e.on_buy_failed(),
            Self::V031(e) => e.on_buy_failed(),
            Self::V051(e) => e.on_buy_failed(),
            Self::V061(e) => e.on_buy_failed(),
            Self::V071(e) => e.on_buy_failed(),
            Self::V081(e) => e.on_buy_failed(),
            Self::V101(e) => e.on_buy_failed(),
            Self::V121(e) => e.on_buy_failed(),
            Self::V131(e) => e.on_buy_failed(),
        }
    }

    pub fn on_sell_fill(&mut self) {
        match self {
            Self::V011(e) => e.on_sell_fill(),
            Self::V022(e) => e.on_sell_fill(),
            Self::V031(e) => e.on_sell_fill(),
            Self::V051(e) => e.on_sell_fill(),
            Self::V061(e) => e.on_sell_fill(),
            Self::V071(e) => e.on_sell_fill(),
            Self::V081(e) => e.on_sell_fill(),
            Self::V101(e) => e.on_sell_fill(),
            Self::V121(e) => e.on_sell_fill(),
            Self::V131(e) => e.on_sell_fill(),
        }
    }

    pub fn on_sell_failed(&mut self, now_ms: u64) {
        match self {
            Self::V011(e) => e.on_sell_failed(now_ms),
            Self::V022(e) => e.on_sell_failed(now_ms),
            Self::V031(e) => e.on_sell_failed(now_ms),
            Self::V051(e) => e.on_sell_failed(now_ms),
            Self::V061(e) => e.on_sell_failed(now_ms),
            Self::V071(e) => e.on_sell_failed(now_ms),
            Self::V081(e) => e.on_sell_failed(now_ms),
            Self::V101(e) => e.on_sell_failed(now_ms),
            Self::V121(e) => e.on_sell_failed(now_ms),
            Self::V131(e) => e.on_sell_failed(now_ms),
        }
    }

    pub fn restore_hold(&mut self, entry_px: f64, now_ms: u64) {
        match self {
            Self::V011(e) => {
                let _ = e.on_buy_fill(entry_px);
            }
            Self::V022(e) => e.restore_hold(entry_px),
            Self::V031(e) => e.restore_hold(entry_px, now_ms),
            Self::V051(e) => e.restore_hold(entry_px, now_ms),
            Self::V061(e) => e.restore_hold(entry_px, now_ms),
            Self::V071(e) => e.restore_hold(entry_px, now_ms),
            Self::V081(e) => e.restore_hold(entry_px, now_ms),
            Self::V101(e) => e.restore_hold(entry_px, now_ms),
            Self::V121(e) => e.restore_hold(entry_px, now_ms),
            Self::V131(e) => e.restore_hold(entry_px, now_ms),
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
            Self::V051(e) => e.bind_gate(wallet, token_amount, price, now_ms),
            Self::V061(e) => e.bind_gate(wallet, token_amount, price, sol, now_ms),
            Self::V071(e) => e.bind_gate(wallet, token_amount, price, sol, now_ms),
            Self::V081(e) => e.bind_gate(wallet, token_amount, price, sol, now_ms),
            Self::V101(e) => e.bind_gate(wallet, token_amount, price, sol, now_ms),
            Self::V121(e) => e.bind_gate(wallet, token_amount, price, sol, now_ms),
            Self::V131(e) => e.bind_gate(wallet, token_amount, price, sol, now_ms),
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
            Self::V051(e) => e.on_event(event),
            Self::V061(e) => e.on_event(event),
            Self::V071(e) => e.on_event(event),
            Self::V081(e) => e.on_event(event),
            Self::V101(e) => e.on_event(event),
            Self::V121(e) => e.on_event(event),
            Self::V131(e) => e.on_event(event),
        }
    }

    pub fn on_clock(&mut self, now_ms: u64, mark_px: f64, holding: bool) -> Option<&'static str> {
        match self {
            Self::V011(e) => {
                if e.is_done() || !e.timer_wants_ticks() {
                    return None;
                }
                let px = if mark_px > 0.0 {
                    mark_px
                } else {
                    e.last_mark_px()
                };
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
            Self::V051(e) => {
                if !holding {
                    return None;
                }
                e.on_clock(now_ms, mark_px)
            }
            Self::V061(e) => {
                if !holding {
                    return None;
                }
                e.on_clock(now_ms, mark_px)
            }
            Self::V071(e) => {
                if !holding {
                    return None;
                }
                e.on_clock(now_ms, mark_px)
            }
            Self::V081(e) => {
                if !holding {
                    return None;
                }
                e.on_clock(now_ms, mark_px)
            }
            Self::V101(e) => {
                if !holding {
                    return None;
                }
                e.on_clock(now_ms, mark_px)
            }
            Self::V121(e) => {
                if !holding {
                    return None;
                }
                e.on_clock(now_ms, mark_px)
            }
            Self::V131(e) => {
                if !holding {
                    return None;
                }
                e.on_clock(now_ms, mark_px)
            }
        }
    }
}
