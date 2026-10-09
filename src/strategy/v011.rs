//! strategy_v_011 — wall-clock entry + mark/event exits.
//!
//! Faithful port of bot_target StrategyV011Engine. One instance per mint.

use crate::config::StrategyV011Config;
use crate::strategy::common::{is_max_sell, round, MarketEvent, SELL_RETRY_MS};

pub const STRATEGY_NAME: &str = "strategy_v_011";

pub const PHASE_UNBOUND: &str = "unbound";
pub const PHASE_WATCHING: &str = "watching";
pub const PHASE_HOLDING: &str = "holding";
pub const PHASE_DONE: &str = "done";

#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    None,
    Skip { detail: String },
    FireBuy { reason: String },
    FireSell { reason: String },
}

pub struct StrategyV011Engine {
    cfg: StrategyV011Config,
    timer_ms: u64,
    phase: String,
    px0: f64,
    t0_wall_ms: u64,
    now_ms: u64,
    bound: bool,
    eligible: bool,
    entry_done: bool,
    aborted: bool,
    target_max_sold: bool,
    target_tokens: f64,
    gate_buy_sig: String,
    last_mark_px: f64,
    sell_prints: Vec<f64>,
    entry_price: f64,
    peak_price: f64,
    exit_in_flight: bool,
    exit_retry: bool,
    sell_retry_at_ms: u64,
    missed_target_sell: bool,
    target_wallet: String,
    last_buy_reason: String,
    last_skip: String,
    last_sell_reason: String,
}

impl StrategyV011Engine {
    pub fn new(cfg: StrategyV011Config) -> Self {
        let timer_ms = if cfg.timer_ms > 0 { cfg.timer_ms } else { 200 };
        Self {
            cfg,
            timer_ms,
            phase: PHASE_UNBOUND.to_string(),
            px0: 0.0,
            t0_wall_ms: 0,
            now_ms: 0,
            bound: false,
            eligible: false,
            entry_done: false,
            aborted: false,
            target_max_sold: false,
            target_tokens: 0.0,
            gate_buy_sig: String::new(),
            last_mark_px: 0.0,
            sell_prints: Vec::new(),
            entry_price: 0.0,
            peak_price: 0.0,
            exit_in_flight: false,
            exit_retry: false,
            sell_retry_at_ms: 0,
            missed_target_sell: false,
            target_wallet: String::new(),
            last_buy_reason: String::new(),
            last_skip: String::new(),
            last_sell_reason: String::new(),
        }
    }

    pub fn timer_ms(&self) -> u64 {
        self.timer_ms
    }

    pub fn phase_name(&self) -> &str {
        &self.phase
    }

    pub fn is_done(&self) -> bool {
        self.phase == PHASE_DONE
    }

    pub fn needs_pool_tape(&self) -> bool {
        self.phase == PHASE_WATCHING || self.phase == PHASE_HOLDING
    }

    pub fn last_mark_px(&self) -> f64 {
        self.last_mark_px
    }

    pub fn last_skip(&self) -> &str {
        &self.last_skip
    }

    pub fn last_buy_reason(&self) -> String {
        if self.last_buy_reason.is_empty() {
            STRATEGY_NAME.to_string()
        } else {
            self.last_buy_reason.clone()
        }
    }

    pub fn last_sell_reason(&self) -> String {
        if self.last_sell_reason.is_empty() {
            "exit signal".to_string()
        } else {
            self.last_sell_reason.clone()
        }
    }

    pub fn buy_size_sol(&self) -> f64 {
        self.cfg.size_sol
    }

    pub fn timer_wants_ticks(&self) -> bool {
        let watching_live = self.phase == PHASE_WATCHING && self.eligible && !self.entry_done;
        let retry_due = self.exit_retry && self.phase == PHASE_HOLDING;
        watching_live || retry_due
    }

    pub fn bind_gate_buy(
        &mut self,
        price: f64,
        sol: f64,
        wallet: &str,
        gate_sig: &str,
        token_amount: f64,
        now_ms: u64,
    ) -> Decision {
        self.t0_wall_ms = now_ms;
        self.now_ms = self.t0_wall_ms;
        let decision = self.bind_gate_buy_core(price, sol, wallet, gate_sig, token_amount);
        self.apply_decision(decision)
    }

    /// Returns a sell reason when the target already max-sold while this buy was in flight.
    pub fn on_buy_fill(&mut self, fill_price: f64) -> Option<String> {
        let missed = self.missed_target_sell && self.cfg.target_sell_exit;
        self.missed_target_sell = false;
        self.entry_price = fill_price;
        self.peak_price = fill_price;
        self.phase = PHASE_HOLDING.to_string();
        self.exit_in_flight = missed;
        self.exit_retry = false;
        self.sell_retry_at_ms = 0;
        self.sell_prints.clear();
        if fill_price > 0.0 {
            self.note_price(fill_price);
        }
        if !missed {
            return None;
        }
        self.last_sell_reason = "target_sell — gate max sell before fill".to_string();
        Some(self.last_sell_reason.clone())
    }

    pub fn on_buy_failed(&mut self) {
        self.entry_price = 0.0;
        self.missed_target_sell = false;
        self.exit_in_flight = false;
        self.exit_retry = false;
        self.phase = PHASE_DONE.to_string();
        self.entry_done = true;
    }

    pub fn on_sell_failed(&mut self, now_ms: u64) {
        self.exit_in_flight = false;
        self.exit_retry = true;
        self.now_ms = now_ms;
        self.sell_retry_at_ms = self.now_ms + SELL_RETRY_MS;
    }

    pub fn on_sell_fill(&mut self) {
        self.entry_price = 0.0;
        self.exit_in_flight = false;
        self.exit_retry = false;
        self.phase = PHASE_DONE.to_string();
        self.entry_done = true;
    }

    pub fn on_event(&mut self, event: &MarketEvent, holding: bool) -> Decision {
        self.now_ms = event.timestamp_ms;
        let decision = self.on_event_core(event, holding);
        self.apply_decision(decision)
    }

    pub fn on_timer(&mut self, mark_px: f64, now_ms: u64) -> Decision {
        self.now_ms = now_ms;
        let decision = self.on_timer_core(mark_px);
        self.apply_decision(decision)
    }

    fn none() -> Decision {
        Decision::None
    }

    fn skip(detail: impl Into<String>) -> Decision {
        Decision::Skip { detail: detail.into() }
    }

    fn fire_buy(reason: impl Into<String>) -> Decision {
        Decision::FireBuy { reason: reason.into() }
    }

    fn fire_sell(reason: impl Into<String>) -> Decision {
        Decision::FireSell { reason: reason.into() }
    }

    fn apply_decision(&mut self, decision: Decision) -> Decision {
        match &decision {
            Decision::FireBuy { reason } => self.last_buy_reason = reason.clone(),
            Decision::FireSell { reason } => self.last_sell_reason = reason.clone(),
            Decision::Skip { detail } => self.last_skip = detail.clone(),
            Decision::None => {}
        }
        decision
    }

    fn bind_gate_buy_core(
        &mut self,
        price: f64,
        sol: f64,
        wallet: &str,
        gate_sig: &str,
        tokens: f64,
    ) -> Decision {
        if self.bound {
            return Self::none();
        }
        self.target_wallet = wallet.to_string();
        self.px0 = price;
        self.bound = true;
        self.target_max_sold = false;
        self.target_tokens = Self::resolve_tokens(tokens, price, sol);
        self.gate_buy_sig = gate_sig.to_string();
        self.sell_prints.clear();
        self.last_mark_px = if price > 0.0 { price } else { 0.0 };
        if price > 0.0 {
            self.note_price(price);
        }
        self.eligible = true;
        self.phase = PHASE_WATCHING.to_string();
        Self::none()
    }

    fn resolve_tokens(tokens: f64, price: f64, sol: f64) -> f64 {
        let tok = tokens.max(0.0);
        if tok <= 0.0 && price > 0.0 && sol > 0.0 {
            return sol / price;
        }
        tok
    }

    fn note_price(&mut self, px: f64) {
        if px <= 0.0 {
            return;
        }
        self.last_mark_px = px;
    }

    fn wall_age_s(&self) -> f64 {
        self.now_ms.saturating_sub(self.t0_wall_ms) as f64 / 1000.0
    }

    fn note_target_bag(&mut self, event: &MarketEvent) -> bool {
        if self.target_wallet.is_empty() || event.wallet != self.target_wallet {
            return false;
        }
        let tokens = Self::resolve_tokens(event.token_amount, event.price, event.sol_amount);
        if event.side == "BUY" {
            if !self.gate_buy_sig.is_empty() && event.signature == self.gate_buy_sig {
                return false;
            }
            self.target_tokens += tokens;
            return false;
        }
        if event.side != "SELL" {
            return false;
        }
        let max_sell = is_max_sell(self.target_tokens, tokens);
        if max_sell {
            self.target_max_sold = true;
        }
        self.target_tokens = (self.target_tokens - tokens).max(0.0);
        max_sell
    }

    fn note_sell_hit(&mut self, event: &MarketEvent) -> Option<Decision> {
        let sell_hit_count = self.cfg.sell_hit_count;
        let sell_hit_min = self.cfg.sell_hit_min;
        let dust_sol = self.cfg.dust_sol;
        if sell_hit_count == 0 || sell_hit_min <= 0.0 {
            return None;
        }
        let sol = event.sol_amount;
        if sol <= dust_sol {
            return None;
        }
        if event.side == "SELL" {
            self.sell_prints.clear();
            return None;
        }
        if event.side != "BUY" {
            return None;
        }
        self.sell_prints.push(sol);
        let need = sell_hit_count as usize;
        if self.sell_prints.len() > need {
            let extra = self.sell_prints.len() - need;
            self.sell_prints.drain(0..extra);
        }
        if self.sell_prints.len() < need {
            return None;
        }
        let total: f64 = self.sell_prints.iter().sum();
        if total >= sell_hit_min {
            self.sell_prints.clear();
            self.exit_in_flight = true;
            return Some(Self::fire_sell(format!(
                "sell_hit — last {} buys sum={:.3} SOL >= {:.3}",
                need, total, sell_hit_min
            )));
        }
        self.sell_prints.remove(0);
        None
    }

    fn abort_sold_max(&mut self) -> Option<Decision> {
        if !self.target_max_sold {
            return None;
        }
        if self.cfg.target_sell_exit && self.entry_done && self.entry_price <= 0.0 {
            self.missed_target_sell = true;
        }
        self.aborted = true;
        self.entry_done = true;
        self.phase = PHASE_DONE.to_string();
        Some(Self::skip("sold_max — target max sell before our buy"))
    }

    fn rearm_watch_from_buy(&mut self, event: &MarketEvent) {
        let price = event.price;
        let sol = event.sol_amount;
        self.px0 = price;
        self.t0_wall_ms = event.timestamp_ms;
        self.now_ms = self.t0_wall_ms;
        self.gate_buy_sig = event.signature.clone();
        self.target_tokens = Self::resolve_tokens(event.token_amount, price, sol);
        self.target_max_sold = false;
        self.entry_done = false;
        self.aborted = false;
        self.entry_price = 0.0;
        self.peak_price = 0.0;
        self.exit_in_flight = false;
        self.exit_retry = false;
        self.missed_target_sell = false;
        self.sell_prints.clear();
        self.last_mark_px = if price > 0.0 { price } else { 0.0 };
        if price > 0.0 {
            self.note_price(price);
        }
        self.eligible = true;
        self.phase = PHASE_WATCHING.to_string();
    }

    fn maybe_rearm(&mut self, event: &MarketEvent) -> bool {
        if !self.target_max_sold {
            return false;
        }
        if event.side != "BUY" {
            return false;
        }
        if self.target_wallet.is_empty() || event.wallet != self.target_wallet {
            return false;
        }
        if !self.gate_buy_sig.is_empty() && event.signature == self.gate_buy_sig {
            return false;
        }
        self.rearm_watch_from_buy(event);
        true
    }

    fn try_entry(&mut self, px: f64, slot: u64) -> Decision {
        if self.entry_done || self.aborted || self.phase == PHASE_DONE {
            return Self::skip("mint already attempted (entry_done) — no re-entry");
        }
        if !self.eligible || self.phase != PHASE_WATCHING {
            return Self::skip("mint not eligible after gate bind");
        }
        if let Some(aborted) = self.abort_sold_max() {
            return aborted;
        }
        if px <= 0.0 {
            return Self::none();
        }

        let age_s = self.wall_age_s();
        let watch = if self.cfg.min_entry_s > 0.0 && self.cfg.min_entry_s == self.cfg.min_entry_s {
            self.cfg.min_entry_s
        } else {
            0.0
        };
        let max_age = self.cfg.max_entry_s;
        let max_chase = self.cfg.max_chase;
        if age_s < watch {
            return Self::none();
        }
        if age_s > max_age {
            self.entry_done = true;
            self.aborted = true;
            self.phase = PHASE_DONE.to_string();
            return Self::skip(format!(
                "entry_expired — wall age={:.1}s > max {}s — abort",
                age_s, max_age
            ));
        }

        let end_ret = if self.px0 > 0.0 { px / self.px0 - 1.0 } else { 0.0 };
        if max_chase > 0.0 && end_ret >= max_chase {
            return Self::none();
        }

        self.entry_done = true;
        let mcap = px * 1_000_000_000.0;
        let _ = (round(age_s, 3), round(end_ret, 4), round(mcap, 3), slot);
        let diag = format!(
            "wall={:.2}s end_ret={:.3} mcap={:.1} slot={}",
            age_s, end_ret, mcap, slot
        );
        Self::fire_buy(format!("entry | {diag}"))
    }

    fn exit_blocked(&self) -> bool {
        self.exit_in_flight || self.exit_retry
    }

    fn on_hold_mark(&mut self, px: f64) -> Decision {
        if self.exit_blocked() {
            return Self::none();
        }
        if px <= 0.0 {
            return Self::none();
        }
        if px > self.peak_price {
            self.peak_price = px;
        }
        let take_profit = self.cfg.take_profit;
        if take_profit > 0.0 && self.entry_price > 0.0 {
            let ret = px / self.entry_price - 1.0;
            if ret >= take_profit {
                self.exit_in_flight = true;
                return Self::fire_sell(format!(
                    "take_profit | ret={:.3} >= {:.3} (vs fill)",
                    ret, take_profit
                ));
            }
        }
        Self::none()
    }

    fn on_hold_event(&mut self, event: &MarketEvent) -> Decision {
        let target_sell_exit = self.cfg.target_sell_exit;
        if self.exit_blocked() {
            return Self::none();
        }
        let px = event.price;
        if px > 0.0 {
            self.note_price(px);
        }

        let max_sell = self.note_target_bag(event);
        if target_sell_exit && max_sell {
            let ret = if self.entry_price > 0.0 && px > 0.0 {
                px / self.entry_price - 1.0
            } else {
                0.0
            };
            self.exit_in_flight = true;
            return Self::fire_sell(format!(
                "target_sell — gate max sell {:.3} SOL | ret={:.3}",
                event.sol_amount, ret
            ));
        }

        if let Some(hit) = self.note_sell_hit(event) {
            return hit;
        }
        let mark = if px > 0.0 { px } else { self.last_mark_px };
        self.on_hold_mark(mark)
    }

    fn retry_exit(&mut self) -> Option<Decision> {
        if !self.exit_retry {
            return None;
        }
        if self.now_ms < self.sell_retry_at_ms {
            return Some(Self::none());
        }
        self.exit_retry = false;
        if self.phase != PHASE_HOLDING {
            return Some(Self::none());
        }
        self.exit_in_flight = true;
        let reason = if self.last_sell_reason.is_empty() {
            "exit retry".to_string()
        } else {
            self.last_sell_reason.clone()
        };
        Some(Self::fire_sell(reason))
    }

    fn on_timer_core(&mut self, mark_px: f64) -> Decision {
        if let Some(retry) = self.retry_exit() {
            return retry;
        }
        if !self.bound {
            return Self::none();
        }
        if mark_px > 0.0 {
            self.note_price(mark_px);
        }
        let px = if mark_px > 0.0 { mark_px } else { self.last_mark_px };

        if self.phase == PHASE_HOLDING || (self.entry_price > 0.0 && !self.entry_done) {
            if self.phase == PHASE_HOLDING {
                return self.on_hold_mark(px);
            }
        }
        if self.phase == PHASE_WATCHING {
            if self.entry_done && self.entry_price <= 0.0 {
                return Self::none();
            }
            return self.try_entry(px, 0);
        }
        Self::none()
    }

    fn on_event_core(&mut self, event: &MarketEvent, holding: bool) -> Decision {
        if !self.bound {
            return Self::none();
        }
        if self.exit_retry {
            return Self::none();
        }
        if holding || self.phase == PHASE_HOLDING {
            return self.on_hold_event(event);
        }
        if self.maybe_rearm(event) {
            return Self::none();
        }

        self.note_target_bag(event);
        if event.price > 0.0 {
            self.note_price(event.price);
        }
        if self.entry_done && !self.aborted && self.entry_price <= 0.0 {
            if self.cfg.target_sell_exit && self.target_max_sold {
                self.missed_target_sell = true;
            }
            return Self::none();
        }
        if self.phase == PHASE_WATCHING {
            if let Some(aborted) = self.abort_sold_max() {
                return aborted;
            }
            let px = if event.price > 0.0 {
                event.price
            } else {
                self.last_mark_px
            };
            return self.try_entry(px, event.slot);
        }
        Self::none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::StrategyV011Config;

    const T0: u64 = 1_700_000_000_000;

    fn cfg() -> StrategyV011Config {
        StrategyV011Config::default_v011()
    }

    fn event(side: &'static str, wallet: &str, sol: f64, tokens: f64, price: f64, sig: &str, ts: u64) -> MarketEvent {
        MarketEvent {
            signature: sig.to_string(),
            slot: 1,
            transaction_index: None,
            event_index: None,
            timestamp_ms: ts,
            side,
            wallet: wallet.to_string(),
            sol_amount: sol,
            token_amount: tokens,
            price,
        }
    }

    fn bind(engine: &mut StrategyV011Engine, tokens: f64, now_ms: u64) -> Decision {
        engine.bind_gate_buy(50e-9, 3.0, "target", "gate", tokens, now_ms)
    }

    fn enter(engine: &mut StrategyV011Engine, now_ms: u64) -> Decision {
        bind(engine, 1_000_000.0, now_ms);
        let min_ms = (cfg().min_entry_s * 1000.0) as u64;
        engine.on_timer(50e-9, now_ms + min_ms + 500)
    }

    #[test]
    fn binds_gate_into_watching() {
        let mut engine = StrategyV011Engine::new(cfg());
        assert!(matches!(bind(&mut engine, 1_000_000.0, T0), Decision::None));
        assert_eq!(engine.phase_name(), PHASE_WATCHING);
    }

    #[test]
    fn fires_after_watch_window() {
        let mut engine = StrategyV011Engine::new(cfg());
        let config = cfg();
        bind(&mut engine, 1_000_000.0, T0);
        let early = (config.min_entry_s - 0.5) * 1000.0;
        assert!(matches!(engine.on_timer(50e-9, T0 + early as u64), Decision::None));
        let late = (config.min_entry_s + 0.5) * 1000.0;
        let decision = engine.on_timer(50e-9, T0 + late as u64);
        match decision {
            Decision::FireBuy { reason } => assert!(reason.contains("entry")),
            other => panic!("expected fire_buy, got {other:?}"),
        }
    }

    #[test]
    fn waits_chase_and_aborts_on_expiry() {
        let mut engine = StrategyV011Engine::new(cfg());
        let config = cfg();
        bind(&mut engine, 1_000_000.0, T0);
        let chased = 50e-9 * (1.0 + config.max_chase + 0.01);
        let held = engine.on_timer(chased, T0 + ((config.min_entry_s + 0.5) * 1000.0) as u64);
        assert!(matches!(held, Decision::None));
        assert_eq!(engine.phase_name(), PHASE_WATCHING);
        let expired = engine.on_timer(chased, T0 + ((config.max_entry_s + 1.0) * 1000.0) as u64);
        match expired {
            Decision::Skip { detail } => assert!(detail.contains("entry_expired")),
            other => panic!("expected skip, got {other:?}"),
        }
        assert_eq!(engine.phase_name(), PHASE_DONE);
    }

    #[test]
    fn takes_profit_against_fill() {
        let mut engine = StrategyV011Engine::new(cfg());
        let config = cfg();
        assert!(matches!(enter(&mut engine, T0), Decision::FireBuy { .. }));
        engine.on_buy_fill(50e-9);
        assert_eq!(engine.phase_name(), PHASE_HOLDING);
        let flat = engine.on_timer(50e-9 * (1.0 + config.take_profit * 0.5), T0 + 8_000);
        assert!(matches!(flat, Decision::None));
        let decision = engine.on_timer(50e-9 * (1.0 + config.take_profit + 0.01), T0 + 9_000);
        match decision {
            Decision::FireSell { reason } => assert!(reason.contains("take_profit")),
            other => panic!("expected fire_sell, got {other:?}"),
        }
    }

    #[test]
    fn retries_exit_after_sell_fails() {
        let mut engine = StrategyV011Engine::new(cfg());
        let config = cfg();
        assert!(matches!(enter(&mut engine, T0), Decision::FireBuy { .. }));
        engine.on_buy_fill(50e-9);
        let px = 50e-9 * (1.0 + config.take_profit + 0.01);
        assert!(matches!(engine.on_timer(px, T0 + 8_000), Decision::FireSell { .. }));
        engine.on_sell_failed(T0 + 8_000);
        assert!(matches!(engine.on_timer(px, T0 + 8_000), Decision::None));
        assert!(matches!(engine.on_timer(px, T0 + 10_000), Decision::FireSell { .. }));
        assert_eq!(engine.phase_name(), PHASE_HOLDING);
    }

    #[test]
    fn exits_on_target_max_sell() {
        let mut engine = StrategyV011Engine::new(cfg());
        assert!(matches!(enter(&mut engine, T0), Decision::FireBuy { .. }));
        engine.on_buy_fill(50e-9);
        let partial = engine.on_event(
            &event("SELL", "target", 0.4, 100_000.0, 50e-9, "partial", T0 + 7_000),
            true,
        );
        assert!(matches!(partial, Decision::None));
        let max_sell = engine.on_event(
            &event("SELL", "target", 2.5, 900_000.0, 50e-9, "max", T0 + 8_000),
            true,
        );
        match max_sell {
            Decision::FireSell { reason } => assert!(reason.contains("target_sell")),
            other => panic!("expected fire_sell, got {other:?}"),
        }
    }

    #[test]
    fn exits_on_sell_hit_cluster() {
        let mut engine = StrategyV011Engine::new(cfg());
        let config = cfg();
        assert!(matches!(enter(&mut engine, T0), Decision::FireBuy { .. }));
        engine.on_buy_fill(50e-9);
        let each = config.sell_hit_min / f64::from(config.sell_hit_count) + 0.05;
        for i in 0..config.sell_hit_count - 1 {
            let d = engine.on_event(
                &event("BUY", "other", each, 1_000.0, 50e-9, &format!("buy-{i}"), T0 + 7_000 + u64::from(i)),
                true,
            );
            assert!(matches!(d, Decision::None));
        }
        let decision = engine.on_event(
            &event("BUY", "other", each, 1_000.0, 50e-9, "buy-last", T0 + 8_000),
            true,
        );
        match decision {
            Decision::FireSell { reason } => assert!(reason.contains("sell_hit")),
            other => panic!("expected fire_sell, got {other:?}"),
        }
    }

    #[test]
    fn aborts_on_target_max_then_rearms() {
        let mut engine = StrategyV011Engine::new(cfg());
        let config = cfg();
        bind(&mut engine, 1_000.0, T0);
        let sold = engine.on_event(
            &event("SELL", "target", 3.0, 1_000.0, 50e-9, "dump", T0 + 1_000),
            false,
        );
        match sold {
            Decision::Skip { detail } => assert!(detail.contains("sold_max")),
            other => panic!("expected skip, got {other:?}"),
        }
        assert_eq!(engine.phase_name(), PHASE_DONE);

        let rearm_at = T0 + 30_000;
        let rearm = engine.on_event(
            &event("BUY", "target", 4.0, 2_000.0, 60e-9, "rebuy", rearm_at),
            false,
        );
        assert!(matches!(rearm, Decision::None));
        assert_eq!(engine.phase_name(), PHASE_WATCHING);
        let early = rearm_at + ((config.min_entry_s - 0.5) * 1000.0) as u64;
        assert!(matches!(engine.on_timer(60e-9, early), Decision::None));
        let late = rearm_at + ((config.min_entry_s + 0.5) * 1000.0) as u64;
        assert!(matches!(engine.on_timer(60e-9, late), Decision::FireBuy { .. }));
    }

    #[test]
    fn sells_after_fill_when_target_max_sold_in_flight() {
        let mut engine = StrategyV011Engine::new(cfg());
        assert!(matches!(enter(&mut engine, T0), Decision::FireBuy { .. }));
        let sold = engine.on_event(
            &event("SELL", "target", 3.0, 1_000_000.0, 50e-9, "dump", T0 + 6_000),
            false,
        );
        assert!(matches!(sold, Decision::None));
        assert_eq!(engine.phase_name(), PHASE_WATCHING);
        let reason = engine.on_buy_fill(50e-9);
        assert!(reason.as_ref().is_some_and(|r| r.contains("target_sell")));
        assert_eq!(engine.phase_name(), PHASE_HOLDING);
        assert!(engine.last_sell_reason().contains("target_sell"));
    }
}
