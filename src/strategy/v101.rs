//! strategy_v_101 — sliding sell-hit inside a time clock.
//!
//! Same flow as scalpingbot `strategy_v_101.py`, for
//! BNnN2MqfWLvgThYBsv6v8JQaYZXYKYahC5YCy27Ct1cX.
//! Knobs from `config/strategy_v_101.json` (2026-10-20).
//! A `min_entry_s`, `max_entry_s`, `max_chase`, `min_mc_sol`, `max_mc_sol`, or
//! `max_hold_s` of 0 disables that bound. Max sell is 80% of the bag.
//! One instance per mint.

use crate::config::StrategyV101Config;
use crate::strategy::common::{
    ChainCursor, MarketEvent, SELL_RETRY_MS, cursor_of, is_max_sell, is_strictly_after, round,
};

pub const STRATEGY_NAME: &str = "strategy_v_101";

const PHASE_IDLE: u8 = 0;
const PHASE_SEEK_SELL: u8 = 1;
const PHASE_PENDING: u8 = 2;
const PHASE_HOLD: u8 = 3;
const PHASE_WAIT: u8 = 4;
const PHASE_DONE: u8 = 5;

const SIDE_BUY: u8 = 1;
const SIDE_SELL: u8 = 2;

const PHASE_NAMES: [&str; 6] = ["idle", "seek_sell", "pending", "hold", "wait", "done"];

struct FlowState {
    phase: u8,
    streak_side: u8,
    prints: Vec<f64>,
    why: String,
    hit_sol: f64,
    exit_sol: f64,
    entry_px: f64,
    held_since_ms: u64,
    target_tokens: f64,
    t0_ms: u64,
    px0: f64,
    target_buys: u32,
    second_buy_seen: bool,
    now_ms: u64,
    diag: BuyDiag,
    opened_at: Option<ChainCursor>,
    closed_at: Option<ChainCursor>,
}

#[derive(Clone, Debug, Default)]
pub struct BuyDiag {
    pub wall_s: f64,
    pub end_ret: f64,
    pub mcap: f64,
    pub buy_hit_sol: f64,
}

fn fresh_state() -> FlowState {
    FlowState {
        phase: PHASE_SEEK_SELL,
        streak_side: 0,
        prints: Vec::new(),
        why: String::new(),
        hit_sol: 0.0,
        exit_sol: 0.0,
        entry_px: 0.0,
        held_since_ms: 0,
        target_tokens: 0.0,
        t0_ms: 0,
        px0: 0.0,
        target_buys: 0,
        second_buy_seen: false,
        now_ms: 0,
        diag: BuyDiag::default(),
        opened_at: None,
        closed_at: None,
    }
}

fn clear_streak(st: &mut FlowState) {
    st.streak_side = 0;
    st.prints.clear();
}

pub struct StrategyV101Engine {
    cfg: StrategyV101Config,
    own_wallet: String,
    st: FlowState,
    target_wallet: String,
    bound: bool,
    buy_reason: String,
    sell_reason: String,
    buy_diag: BuyDiag,
    last_px: f64,
    exit_retry: bool,
    exit_retry_at_ms: u64,
}

impl StrategyV101Engine {
    pub fn new(cfg: StrategyV101Config, own_wallet: impl Into<String>) -> Self {
        let mut engine = Self {
            cfg,
            own_wallet: own_wallet.into(),
            st: fresh_state(),
            target_wallet: String::new(),
            bound: false,
            buy_reason: String::new(),
            sell_reason: String::new(),
            buy_diag: BuyDiag::default(),
            last_px: 0.0,
            exit_retry: false,
            exit_retry_at_ms: 0,
        };
        engine.st.phase = PHASE_IDLE;
        engine
    }

    pub fn phase_name(&self) -> &'static str {
        PHASE_NAMES
            .get(self.st.phase as usize)
            .copied()
            .unwrap_or("idle")
    }

    pub fn needs_pool_tape(&self) -> bool {
        self.exit_retry || matches!(self.st.phase, PHASE_SEEK_SELL | PHASE_PENDING | PHASE_HOLD)
    }

    pub fn is_bound(&self) -> bool {
        self.bound
    }

    pub fn is_done(&self) -> bool {
        self.st.phase == PHASE_DONE
    }

    pub fn last_buy_reason(&self) -> String {
        if self.buy_reason.is_empty() {
            STRATEGY_NAME.to_string()
        } else {
            self.buy_reason.clone()
        }
    }

    pub fn last_sell_reason(&self) -> String {
        if self.sell_reason.is_empty() {
            "exit".to_string()
        } else {
            self.sell_reason.clone()
        }
    }

    pub fn last_buy_diag(&self) -> BuyDiag {
        self.buy_diag.clone()
    }

    pub fn buy_size_sol(&self) -> f64 {
        self.cfg.size_sol
    }

    pub fn bind_gate(
        &mut self,
        wallet: &str,
        gate_token_amount: f64,
        price: f64,
        sol: f64,
        now_ms: u64,
    ) {
        self.target_wallet = wallet.to_string();
        self.st = fresh_state();
        self.st.target_tokens = gate_token_amount.max(0.0);
        self.st.target_buys = if sol > 0.0 { 1 } else { 0 };
        self.st.t0_ms = now_ms;
        self.st.now_ms = self.st.t0_ms;
        self.st.px0 = if price > 0.0 { price } else { 0.0 };
        self.last_px = self.st.px0;
        self.bound = true;
        self.buy_reason.clear();
        self.sell_reason.clear();
        self.buy_diag = BuyDiag::default();
        self.exit_retry = false;
        self.exit_retry_at_ms = 0;
    }

    pub fn on_buy_fill(&mut self, fill_px: f64, held_since_ms: u64) {
        if fill_px > 0.0 {
            self.st.entry_px = fill_px;
        }
        self.commit_buy(true);
        self.st.held_since_ms = held_since_ms;
    }

    pub fn on_buy_failed(&mut self) {
        self.exit_retry = false;
        if self.st.phase == PHASE_DONE || self.st.phase == PHASE_IDLE {
            return;
        }
        self.commit_buy(false);
        self.st.held_since_ms = 0;
        self.resume_seek();
    }

    pub fn on_sell_fill(&mut self) {
        self.exit_retry = false;
        self.st.held_since_ms = 0;
        if self.st.phase == PHASE_HOLD {
            self.st.phase = PHASE_WAIT;
            self.st.why = "external_exit".to_string();
            clear_streak(&mut self.st);
        }
        self.resume_seek();
    }

    pub fn on_sell_failed(&mut self, now_ms: u64) {
        self.exit_retry = true;
        self.exit_retry_at_ms = now_ms + SELL_RETRY_MS;
        self.st.phase = PHASE_HOLD;
        clear_streak(&mut self.st);
    }

    pub fn on_clock(&mut self, now_ms: u64, mark_px: f64) -> Option<&'static str> {
        self.st.now_ms = now_ms;
        if self.exit_retry {
            if now_ms < self.exit_retry_at_ms {
                return None;
            }
            self.exit_retry = false;
            self.st.phase = PHASE_WAIT;
            clear_streak(&mut self.st);
            return Some("SELL");
        }
        if mark_px > 0.0 {
            self.last_px = mark_px;
        }
        if self.st.phase != PHASE_HOLD {
            return None;
        }
        let px = if mark_px > 0.0 { mark_px } else { self.last_px };
        let signal = self.risk_exit(px, now_ms);
        self.note_signal(signal)
    }

    pub fn apply_missed_target_sell(&mut self) {
        if self.st.phase != PHASE_HOLD {
            return;
        }
        if !self.cfg.target_sell_exit && !self.cfg.first_cycle_only {
            return;
        }
        self.st.why = "target_sell".to_string();
        self.sell_reason = "target_sell".to_string();
        self.after_max_sell();
    }

    pub fn restore_hold(&mut self, entry_px: f64, held_since_ms: u64) {
        self.bound = true;
        self.st.phase = PHASE_HOLD;
        self.st.entry_px = entry_px;
        self.st.held_since_ms = held_since_ms;
        self.st.why = "restored".to_string();
        clear_streak(&mut self.st);
    }

    pub fn on_event(&mut self, event: &MarketEvent) -> Option<&'static str> {
        if !self.bound {
            return None;
        }
        if self.exit_retry {
            return None;
        }
        if event.price > 0.0 {
            self.last_px = event.price;
        }
        let side = if event.side == "BUY" {
            SIDE_BUY
        } else if event.side == "SELL" {
            SIDE_SELL
        } else {
            0
        };
        let is_target = !self.target_wallet.is_empty()
            && event.wallet == self.target_wallet
            && (side == SIDE_BUY || side == SIDE_SELL);
        let is_own = !self.own_wallet.is_empty() && event.wallet == self.own_wallet;
        let signal = self.flow_step(
            side,
            event.sol_amount,
            event.token_amount,
            event.price,
            is_target,
            is_own,
            event.timestamp_ms,
            cursor_of(event),
        );
        self.note_signal(signal)
    }

    fn note_signal(&mut self, signal: Option<&'static str>) -> Option<&'static str> {
        match signal {
            Some("BUY") => {
                self.buy_reason = format!("buy_hit {:.2} SOL", self.st.hit_sol);
                self.buy_diag = self.st.diag.clone();
                Some("BUY")
            }
            Some("SELL") => {
                self.sell_reason = match self.st.why.as_str() {
                    "sell_hit" => format!("sell_hit {:.2} SOL", self.st.exit_sol),
                    "take_profit" => "take_profit".to_string(),
                    "hold_time" => "hold_time".to_string(),
                    "second_buy" => "second_buy".to_string(),
                    _ => "target_sell".to_string(),
                };
                Some("SELL")
            }
            _ => None,
        }
    }

    fn commit_buy(&mut self, ok: bool) {
        clear_streak(&mut self.st);
        self.st.phase = if ok { PHASE_HOLD } else { PHASE_WAIT };
    }

    fn age_s(&self, now_ms: u64) -> f64 {
        if self.st.t0_ms == 0 {
            return 0.0;
        }
        now_ms.saturating_sub(self.st.t0_ms) as f64 / 1000.0
    }

    fn held_s(&self, now_ms: u64) -> f64 {
        if self.st.held_since_ms == 0 {
            return 0.0;
        }
        now_ms.saturating_sub(self.st.held_since_ms) as f64 / 1000.0
    }

    fn expire(&mut self) {
        self.st.phase = PHASE_WAIT;
        clear_streak(&mut self.st);
    }

    fn after_max_sell(&mut self) {
        clear_streak(&mut self.st);
        self.st.phase = if self.cfg.first_cycle_only {
            PHASE_DONE
        } else {
            PHASE_IDLE
        };
    }

    fn resume_seek(&mut self) {
        let p = &self.cfg;
        if self.st.second_buy_seen || !p.repeat_entries || self.st.phase != PHASE_WAIT {
            return;
        }
        if p.max_entry_s > 0.0 && self.age_s(self.st.now_ms) > p.max_entry_s {
            return;
        }
        self.st.phase = PHASE_SEEK_SELL;
        self.st.entry_px = 0.0;
        self.st.hit_sol = 0.0;
        clear_streak(&mut self.st);
    }

    fn flow_step(
        &mut self,
        side: u8,
        sol: f64,
        tokens: f64,
        px: f64,
        is_target: bool,
        is_own: bool,
        now_ms: u64,
        order: ChainCursor,
    ) -> Option<&'static str> {
        self.st.now_ms = now_ms;
        if is_target
            && side == SIDE_BUY
            && self
                .st
                .closed_at
                .is_some_and(|closed| !is_strictly_after(order, closed))
        {
            return None;
        }
        if is_target && side == SIDE_SELL && self.sell_already_passed(order) {
            return None;
        }

        if is_target && side == SIDE_BUY {
            self.st.target_tokens += tokens.max(0.0);
            self.st.opened_at = Some(order);
            if sol > 0.0 && self.st.phase != PHASE_IDLE {
                self.st.target_buys += 1;
                if self.cfg.second_buy_exit && self.st.target_buys >= 2 {
                    self.st.second_buy_seen = true;
                    if self.st.phase == PHASE_HOLD {
                        self.st.why = "second_buy".to_string();
                        self.st.phase = PHASE_WAIT;
                        clear_streak(&mut self.st);
                        return Some("SELL");
                    }
                    if matches!(self.st.phase, PHASE_SEEK_SELL | PHASE_PENDING) {
                        self.expire();
                        return None;
                    }
                }
            }
        }

        if is_target && side == SIDE_SELL {
            let max_sell = is_max_sell(self.st.target_tokens, tokens);
            self.st.target_tokens = (self.st.target_tokens - tokens.max(0.0)).max(0.0);
            if max_sell && self.st.phase != PHASE_DONE {
                if self
                    .st
                    .closed_at
                    .is_none_or(|closed| is_strictly_after(order, closed))
                {
                    self.st.closed_at = Some(order);
                }
                if self.st.phase == PHASE_HOLD
                    && (self.cfg.target_sell_exit || self.cfg.first_cycle_only)
                {
                    self.st.why = "target_sell".to_string();
                    self.after_max_sell();
                    return Some("SELL");
                }
                if matches!(self.st.phase, PHASE_SEEK_SELL | PHASE_PENDING | PHASE_WAIT) {
                    self.after_max_sell();
                    return None;
                }
            }
        }

        if is_target && side == SIDE_BUY && self.st.phase == PHASE_IDLE {
            self.st.phase = PHASE_SEEK_SELL;
            self.st.hit_sol = 0.0;
            self.st.t0_ms = now_ms;
            self.st.px0 = px;
            self.st.target_buys = if sol > 0.0 { 1 } else { 0 };
            self.st.second_buy_seen = false;
            clear_streak(&mut self.st);
            return None;
        }

        if self.st.phase == PHASE_SEEK_SELL
            && self.cfg.max_entry_s > 0.0
            && self.age_s(now_ms) > self.cfg.max_entry_s
        {
            self.expire();
            return None;
        }

        if is_own {
            return self.risk_exit(px, now_ms);
        }

        if sol <= self.cfg.dust_sol || (side != SIDE_BUY && side != SIDE_SELL) {
            return self.risk_exit(px, now_ms);
        }
        if self.st.phase != PHASE_SEEK_SELL && self.st.phase != PHASE_HOLD {
            return None;
        }

        if self.st.streak_side != side {
            self.st.streak_side = side;
            self.st.prints.clear();
        }
        self.st.prints.push(sol);
        let raw_need = if self.st.phase == PHASE_SEEK_SELL && side == SIDE_SELL {
            self.cfg.buy_hit_count
        } else {
            self.cfg.sell_hit_count
        };
        let need = raw_need.max(1) as usize;
        if self.st.prints.len() > need {
            let extra = self.st.prints.len() - need;
            self.st.prints.drain(0..extra);
        }
        if self.st.prints.len() < need {
            return self.risk_exit(px, now_ms);
        }

        let total: f64 = self.st.prints.iter().sum();
        let done = self.st.streak_side;
        if self.st.phase == PHASE_SEEK_SELL && done == SIDE_SELL && total >= self.cfg.buy_hit_min {
            let age = self.age_s(now_ms);
            if self.cfg.min_entry_s > 0.0 && age < self.cfg.min_entry_s {
                self.st.prints.remove(0);
                return None;
            }
            let mcap = if px > 0.0 { px * 1_000_000_000.0 } else { 0.0 };
            let chase = if self.st.px0 > 0.0 && px > 0.0 {
                px / self.st.px0 - 1.0
            } else {
                0.0
            };
            if (self.cfg.min_mc_sol > 0.0 && mcap < self.cfg.min_mc_sol)
                || (self.cfg.max_mc_sol > 0.0 && mcap > self.cfg.max_mc_sol)
            {
                self.st.prints.remove(0);
                return None;
            }
            if self.cfg.max_chase > 0.0 && chase >= self.cfg.max_chase {
                self.st.prints.remove(0);
                return None;
            }
            self.st.hit_sol = total;
            self.st.phase = PHASE_PENDING;
            self.st.why = "buy_hit".to_string();
            self.st.entry_px = px;
            self.st.diag = BuyDiag {
                wall_s: round(age, 3),
                end_ret: round(chase, 4),
                mcap: round(mcap, 3),
                buy_hit_sol: round(total, 4),
            };
            clear_streak(&mut self.st);
            return Some("BUY");
        }
        if self.st.phase == PHASE_HOLD && done == SIDE_BUY && total >= self.cfg.sell_hit_min {
            self.st.phase = PHASE_WAIT;
            self.st.why = "sell_hit".to_string();
            self.st.exit_sol = total;
            clear_streak(&mut self.st);
            return Some("SELL");
        }
        self.st.prints.remove(0);
        self.risk_exit(px, now_ms)
    }

    fn sell_already_passed(&self, order: ChainCursor) -> bool {
        if self
            .st
            .opened_at
            .is_some_and(|opened| is_strictly_after(opened, order))
        {
            return true;
        }
        if self
            .st
            .closed_at
            .is_some_and(|closed| is_strictly_after(closed, order))
        {
            return true;
        }
        false
    }

    fn risk_exit(&mut self, px: f64, now_ms: u64) -> Option<&'static str> {
        if self.st.phase != PHASE_HOLD {
            return None;
        }
        if self.cfg.take_profit > 0.0
            && self.st.entry_px > 0.0
            && px > 0.0
            && px / self.st.entry_px - 1.0 >= self.cfg.take_profit
        {
            self.st.phase = PHASE_WAIT;
            self.st.why = "take_profit".to_string();
            clear_streak(&mut self.st);
            return Some("SELL");
        }
        if self.cfg.max_hold_s > 0.0 && self.held_s(now_ms) >= self.cfg.max_hold_s {
            self.st.phase = PHASE_WAIT;
            self.st.why = "hold_time".to_string();
            clear_streak(&mut self.st);
            return Some("SELL");
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::StrategyV101Config;

    const T0: u64 = 1_700_000_000_000;

    fn cfg(overrides: impl FnOnce(&mut StrategyV101Config)) -> StrategyV101Config {
        let mut cfg = StrategyV101Config::default_v101();
        overrides(&mut cfg);
        cfg
    }

    fn print(side: &'static str, sol: f64, signature: &str, ts: u64) -> MarketEvent {
        MarketEvent {
            signature: signature.to_string(),
            slot: 1,
            transaction_index: None,
            event_index: None,
            timestamp_ms: ts,
            side,
            wallet: "other".to_string(),
            sol_amount: sol,
            token_amount: 0.0,
            price: 80e-9,
        }
    }

    fn bound(overrides: impl FnOnce(&mut StrategyV101Config), own: &str) -> StrategyV101Engine {
        let mut engine = StrategyV101Engine::new(cfg(overrides), own);
        engine.bind_gate("target", 1_000.0, 80e-9, 1.0, T0);
        engine
    }

    fn sells(
        engine: &mut StrategyV101Engine,
        tag: &str,
        ts: u64,
        sol: f64,
    ) -> Option<&'static str> {
        let mut last = None;
        for i in 0..4 {
            last = engine.on_event(&print("SELL", sol, &format!("{tag}-{i}"), ts));
        }
        last
    }

    fn buys(
        engine: &mut StrategyV101Engine,
        tag: &str,
        ts: u64,
        sol: f64,
        count: usize,
    ) -> Option<&'static str> {
        let mut last = None;
        for i in 0..count {
            last = engine.on_event(&print("BUY", sol, &format!("{tag}-{i}"), ts));
        }
        last
    }

    fn arm(engine: &mut StrategyV101Engine) {
        assert_eq!(sells(engine, "arm", T0 + 5_000, 0.4), Some("BUY"));
    }

    fn target_trade(
        side: &'static str,
        sol: f64,
        tokens: f64,
        signature: &str,
        ts: u64,
        slot: u64,
    ) -> MarketEvent {
        let mut event = print(side, sol, signature, ts);
        event.wallet = "target".to_string();
        event.token_amount = tokens;
        event.slot = slot;
        event
    }

    #[test]
    fn loads_scalping_knobs() {
        let loaded = StrategyV101Config::default_v101();
        assert_eq!(
            loaded.gate_wallet,
            "BNnN2MqfWLvgThYBsv6v8JQaYZXYKYahC5YCy27Ct1cX"
        );
        assert_eq!(loaded.dust_sol, 0.25);
        assert_eq!(loaded.min_entry_s, 2.0);
        assert_eq!(loaded.max_entry_s, 37.5);
        assert_eq!(loaded.max_chase, 0.6);
        assert_eq!(loaded.buy_hit_count, 4);
        assert_eq!(loaded.buy_hit_min, 1.5);
        assert_eq!(loaded.min_mc_sol, 55.0);
        assert_eq!(loaded.max_mc_sol, 150.0);
        assert_eq!(loaded.sell_hit_count, 4);
        assert_eq!(loaded.sell_hit_min, 10.0);
        assert_eq!(loaded.take_profit, 0.8);
        assert_eq!(loaded.max_hold_s, 0.0);
        assert!(loaded.first_cycle_only);
        assert!(!loaded.target_sell_exit);
        assert!(!loaded.second_buy_exit);
        assert!(loaded.repeat_entries);
        assert_eq!(loaded.size_sol, 0.3);
    }

    #[test]
    fn buys_sliding_sell_window_inside_the_clock() {
        let mut engine = bound(|_| {}, "");
        assert_eq!(engine.phase_name(), "seek_sell");
        assert!(engine.needs_pool_tape());
        assert_eq!(sells(&mut engine, "open", T0 + 2_500, 0.4), Some("BUY"));
        assert_eq!(engine.phase_name(), "pending");
        assert!(engine.last_buy_reason().contains("buy_hit"));
        assert_eq!(engine.last_buy_diag().buy_hit_sol, 1.6);
        assert_eq!(engine.last_buy_diag().wall_s, 2.5);
        assert_eq!(engine.last_buy_diag().end_ret, 0.0);
        assert_eq!(engine.last_buy_diag().mcap, 80.0);

        let mut gated = bound(|_| {}, "");
        assert_eq!(sells(&mut gated, "early", T0 + 1_500, 0.4), None);
        assert_eq!(gated.phase_name(), "seek_sell");
        assert_eq!(
            gated.on_event(&print("SELL", 0.4, "later", T0 + 2_000)),
            Some("BUY")
        );
        assert_eq!(gated.last_buy_diag().wall_s, 2.0);
    }

    #[test]
    fn slides_window_outside_mcap_or_chase() {
        let mut low = bound(|_| {}, "");
        for i in 0..4 {
            let mut cheap = print("SELL", 0.4, &format!("cheap-{i}"), T0 + 3_000);
            cheap.price = 54e-9;
            assert_eq!(low.on_event(&cheap), None);
        }
        assert_eq!(low.phase_name(), "seek_sell");
        let mut floor = print("SELL", 0.4, "floor", T0 + 4_000);
        floor.price = 55e-9;
        assert_eq!(low.on_event(&floor), Some("BUY"));
        assert_eq!(low.last_buy_diag().mcap, 55.0);

        let mut high = bound(|cfg| cfg.max_chase = 0.0, "");
        for i in 0..4 {
            let mut rich = print("SELL", 0.4, &format!("rich-{i}"), T0 + 3_000);
            rich.price = 151e-9;
            assert_eq!(high.on_event(&rich), None);
        }
        assert_eq!(high.phase_name(), "seek_sell");
        let mut cap = print("SELL", 0.4, "cap", T0 + 4_000);
        cap.price = 150e-9;
        assert_eq!(high.on_event(&cap), Some("BUY"));
        assert_eq!(high.last_buy_diag().mcap, 150.0);

        let mut chased = bound(|_| {}, "");
        for i in 0..4 {
            let mut hot = print("SELL", 0.4, &format!("hot-{i}"), T0 + 3_000);
            hot.price = 128e-9;
            assert_eq!(chased.on_event(&hot), None);
        }
        assert_eq!(chased.phase_name(), "seek_sell");
        assert_eq!(
            chased.on_event(&print("SELL", 0.4, "cooled", T0 + 4_000)),
            Some("BUY")
        );
        assert_eq!(chased.last_buy_diag().end_ret, 0.0);
    }

    #[test]
    fn slides_short_sell_window_until_sum_qualifies() {
        let mut engine = bound(|_| {}, "");
        assert_eq!(sells(&mut engine, "short", T0 + 5_000, 0.3), None);
        assert_eq!(engine.phase_name(), "seek_sell");
        assert_eq!(
            engine.on_event(&print("SELL", 0.6, "slide", T0 + 6_000)),
            Some("BUY")
        );
        assert_eq!(engine.last_buy_diag().buy_hit_sol, 1.5);
    }

    #[test]
    fn ends_cycle_when_window_finishes_after_clock() {
        let mut engine = bound(|_| {}, "");
        assert_eq!(sells(&mut engine, "late", T0 + 37_501, 0.4), None);
        assert_eq!(engine.phase_name(), "wait");
        assert!(!engine.needs_pool_tape());

        let mut edge = bound(|_| {}, "");
        assert_eq!(sells(&mut edge, "edge", T0 + 37_500, 0.4), Some("BUY"));

        let mut open = bound(|cfg| cfg.max_entry_s = 0.0, "");
        assert_eq!(sells(&mut open, "open", T0 + 60_000, 0.4), Some("BUY"));
        assert_eq!(open.phase_name(), "pending");
    }

    #[test]
    fn sells_hold_on_cluster_tp_or_target() {
        let mut cluster = bound(|_| {}, "");
        arm(&mut cluster);
        cluster.on_buy_fill(80e-9, T0 + 5_000);
        assert_eq!(cluster.phase_name(), "hold");
        assert_eq!(buys(&mut cluster, "short", T0 + 6_000, 2.0, 4), None);
        assert_eq!(
            cluster.on_event(&print("BUY", 4.0, "slide", T0 + 7_000)),
            Some("SELL")
        );
        assert!(cluster.last_sell_reason().contains("sell_hit"));

        let mut tp = bound(|_| {}, "");
        arm(&mut tp);
        let fill = 80e-9;
        tp.on_buy_fill(fill, T0 + 5_000);
        let mut under = print("SELL", 0.05, "under", T0 + 6_000);
        under.price = fill * 1.79;
        assert_eq!(tp.on_event(&under), None);
        let mut took = print("SELL", 0.05, "tp", T0 + 6_000);
        took.price = fill * 1.8;
        assert_eq!(tp.on_event(&took), Some("SELL"));
        assert_eq!(tp.last_sell_reason(), "take_profit");

        let mut held = bound(|_| {}, "");
        arm(&mut held);
        held.on_buy_fill(fill, T0 + 5_000);
        assert_eq!(held.on_clock(T0 + 5_000 + 90_000, fill), None);
        assert_eq!(held.phase_name(), "hold");

        let mut capped = bound(|cfg| cfg.max_hold_s = 30.0, "");
        arm(&mut capped);
        capped.on_buy_fill(fill, T0 + 5_000);
        assert_eq!(capped.on_clock(T0 + 5_000 + 29_999, fill), None);
        assert_eq!(capped.on_clock(T0 + 5_000 + 30_000, fill), Some("SELL"));
        assert_eq!(capped.last_sell_reason(), "hold_time");

        let mut target = bound(|_| {}, "");
        arm(&mut target);
        target.on_buy_fill(fill, T0 + 5_000);
        let mut partial = print("SELL", 0.05, "partial", T0 + 6_000);
        partial.wallet = "target".to_string();
        partial.token_amount = 100.0;
        assert_eq!(target.on_event(&partial), None);
        let mut dumped = print("SELL", 0.05, "max", T0 + 7_000);
        dumped.wallet = "target".to_string();
        dumped.token_amount = 900.0;
        assert_eq!(target.on_event(&dumped), Some("SELL"));
        assert_eq!(target.last_sell_reason(), "target_sell");
        assert_eq!(target.phase_name(), "done");
    }

    #[test]
    fn own_sell_does_not_complete_the_buy_window() {
        let mut engine = bound(|_| {}, "me");
        for i in 0..3 {
            assert_eq!(
                engine.on_event(&print("SELL", 0.4, &format!("s-{i}"), T0 + 3_000)),
                None
            );
        }
        let mut own = print("SELL", 5.0, "own", T0 + 4_000);
        own.wallet = "me".to_string();
        assert_eq!(engine.on_event(&own), None);
        assert_eq!(engine.phase_name(), "seek_sell");
        assert_eq!(
            engine.on_event(&print("SELL", 0.4, "fourth", T0 + 5_000)),
            Some("BUY")
        );
    }

    #[test]
    fn second_buy_sells_a_hold_and_blocks_another_entry() {
        let mut engine = bound(
            |cfg| {
                cfg.second_buy_exit = true;
            },
            "",
        );
        arm(&mut engine);
        engine.on_buy_fill(80e-9, T0 + 5_000);
        assert_eq!(
            engine.on_event(&target_trade("BUY", 0.0, 100.0, "zero", T0 + 6_000, 2)),
            None
        );
        assert_eq!(engine.phase_name(), "hold");
        assert_eq!(
            engine.on_event(&target_trade("BUY", 1.0, 100.0, "second", T0 + 7_000, 3)),
            Some("SELL")
        );
        assert_eq!(engine.last_sell_reason(), "second_buy");
        engine.on_sell_fill();
        assert_eq!(engine.phase_name(), "wait");
        assert_eq!(sells(&mut engine, "still", T0 + 8_000, 0.4), None);
    }

    #[test]
    fn second_buy_ends_the_search_before_entry() {
        let mut engine = bound(|cfg| cfg.second_buy_exit = true, "");
        assert_eq!(
            engine.on_event(&target_trade("BUY", 1.0, 500.0, "second", T0 + 1_000, 2)),
            None
        );
        assert_eq!(engine.phase_name(), "wait");
        assert_eq!(sells(&mut engine, "too-late", T0 + 3_000, 0.4), None);
    }

    #[test]
    fn first_cycle_only_never_reopens() {
        let mut engine = bound(|_| {}, "");
        let mut flat = print("SELL", 1.0, "flat", T0 + 1_000);
        flat.wallet = "target".to_string();
        flat.token_amount = 1_000.0;
        assert_eq!(engine.on_event(&flat), None);
        assert_eq!(engine.phase_name(), "done");
        assert!(!engine.needs_pool_tape());
        let mut again = print("BUY", 1.0, "again", T0 + 30_000);
        again.wallet = "target".to_string();
        again.token_amount = 1_000.0;
        again.slot = 3;
        assert_eq!(engine.on_event(&again), None);
        assert_eq!(engine.phase_name(), "done");
    }

    #[test]
    fn repeat_entries_reseeks_on_same_clock() {
        let mut engine = bound(|_| {}, "");
        arm(&mut engine);
        engine.on_buy_fill(80e-9, T0 + 5_000);
        let mut tp = print("SELL", 0.05, "tp", T0 + 6_000);
        tp.price = 80e-9 * 1.8;
        assert_eq!(engine.on_event(&tp), Some("SELL"));
        assert_eq!(engine.phase_name(), "wait");
        engine.on_sell_fill();
        assert_eq!(engine.phase_name(), "seek_sell");
        assert_eq!(sells(&mut engine, "again", T0 + 7_000, 0.4), Some("BUY"));
    }

    #[test]
    fn retries_failed_sell() {
        let mut engine = bound(|_| {}, "");
        arm(&mut engine);
        engine.on_buy_fill(80e-9, T0 + 5_000);
        assert_eq!(buys(&mut engine, "exit", T0 + 6_000, 2.5, 4), Some("SELL"));
        engine.on_sell_failed(T0 + 6_000);
        assert_eq!(engine.phase_name(), "hold");
        assert_eq!(engine.on_clock(T0 + 7_000, 0.0), None);
        assert_eq!(engine.on_clock(T0 + 8_000, 0.0), Some("SELL"));
        assert!(engine.last_sell_reason().contains("sell_hit"));
    }

    #[test]
    fn applies_missed_target_sell_after_fill() {
        let mut engine = bound(|_| {}, "");
        arm(&mut engine);
        let mut during = print("SELL", 1.0, "during", T0 + 6_000);
        during.wallet = "target".to_string();
        during.token_amount = 1_000.0;
        assert_eq!(engine.on_event(&during), None);
        assert_eq!(engine.phase_name(), "done");
        engine.on_buy_fill(80e-9, T0 + 6_000);
        assert_eq!(engine.phase_name(), "hold");
        engine.apply_missed_target_sell();
        assert_eq!(engine.phase_name(), "done");
        assert_eq!(engine.last_sell_reason(), "target_sell");
    }
}
