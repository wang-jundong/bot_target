//! strategy_v_081 — clock entry, then a sliding buy streak to sell.
//!
//! Same flow as scalpingbot `strategy_v_081.py`, for
//! ardinRsN1mNYVeoJWTBsWeYeXvuR9UUDGMsCDKpb6AT.
//! Knobs from `config/strategy_v_081.json` (2026-10-22).
//! `buy_hit_count` of 0 buys the first print inside the clock. A zero bound
//! disables that gate. Max sell is 80% of the bag. Our own prints are not
//! other-wallet flow. One instance per mint.

use crate::config::StrategyV081Config;
use crate::strategy::common::{
    ChainCursor, MarketEvent, SELL_RETRY_MS, cursor_of, is_max_sell, is_strictly_after, round,
};

pub const STRATEGY_NAME: &str = "strategy_v_081";

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
    largest_buy_sol: f64,
    largest_sell_sol: f64,
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
    pub largest_buy_sol: f64,
    pub price_ratio: f64,
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
        largest_buy_sol: 0.0,
        largest_sell_sol: 0.0,
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

pub struct StrategyV081Engine {
    cfg: StrategyV081Config,
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

impl StrategyV081Engine {
    pub fn new(cfg: StrategyV081Config, own_wallet: impl Into<String>) -> Self {
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
        self.st.largest_sell_sol = 0.0;
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
        self.st.largest_sell_sol = 0.0;
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
                self.buy_reason = if self.st.why == "clock" {
                    "clock".to_string()
                } else {
                    format!("buy_hit {:.2} SOL", self.st.hit_sol)
                };
                self.buy_diag = self.st.diag.clone();
                Some("BUY")
            }
            Some("SELL") => {
                self.sell_reason = match self.st.why.as_str() {
                    "sell_hit" => format!("sell_hit {:.2} SOL", self.st.exit_sol),
                    "take_profit" => "take_profit".to_string(),
                    "hold_time" => "hold_time".to_string(),
                    "hold_sell" => format!("hold_sell {:.2} SOL", self.st.largest_sell_sol),
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
        if self.st.second_buy_seen || !self.cfg.repeat_entries || self.st.phase != PHASE_WAIT {
            return;
        }
        if self.cfg.max_entry_s > 0.0 && self.age_s(self.st.now_ms) > self.cfg.max_entry_s {
            return;
        }
        self.st.phase = PHASE_SEEK_SELL;
        self.st.entry_px = 0.0;
        self.st.hit_sol = 0.0;
        clear_streak(&mut self.st);
    }

    fn note_other_buy(&mut self, side: u8, sol: f64, is_target: bool, is_own: bool) {
        if self.st.phase != PHASE_SEEK_SELL || is_target || is_own || side != SIDE_BUY || sol <= 0.0
        {
            return;
        }
        if sol > self.st.largest_buy_sol {
            self.st.largest_buy_sol = sol;
        }
    }

    fn note_hold_sell(&mut self, side: u8, sol: f64, is_target: bool, is_own: bool) {
        if self.st.phase != PHASE_HOLD || is_target || is_own || side != SIDE_SELL || sol <= 0.0 {
            return;
        }
        if sol > self.st.largest_sell_sol {
            self.st.largest_sell_sol = sol;
        }
    }

    fn largest_buy_blocks(&self) -> bool {
        let seen = self.st.largest_buy_sol;
        if self.cfg.min_largest_buy_sol > 0.0 && seen < self.cfg.min_largest_buy_sol {
            return true;
        }
        if self.cfg.max_largest_buy_sol > 0.0 && seen > self.cfg.max_largest_buy_sol {
            return true;
        }
        false
    }

    fn price_ratio_blocks(&self, px: f64) -> bool {
        let lo = self.cfg.min_price_ratio;
        let hi = self.cfg.max_price_ratio;
        if lo <= 0.0 && hi <= 0.0 {
            return false;
        }
        if self.st.px0 <= 0.0 || px <= 0.0 {
            return true;
        }
        let ratio = round(px / self.st.px0, 4);
        if lo > 0.0 && ratio < lo {
            return true;
        }
        if hi > 0.0 && ratio > hi {
            return true;
        }
        false
    }

    fn mcap_blocks(&self, px: f64) -> bool {
        let mcap = if px > 0.0 { px * 1_000_000_000.0 } else { 0.0 };
        (self.cfg.min_mc_sol > 0.0 && mcap < self.cfg.min_mc_sol)
            || (self.cfg.max_mc_sol > 0.0 && mcap > self.cfg.max_mc_sol)
    }

    fn hold_sell_exit(&self, now_ms: u64) -> bool {
        if self.cfg.hold_sell_s <= 0.0 || self.held_s(now_ms) < self.cfg.hold_sell_s {
            return false;
        }
        let seen = self.st.largest_sell_sol;
        let under = self.cfg.min_largest_sell_sol > 0.0 && seen < self.cfg.min_largest_sell_sol;
        let over = self.cfg.max_largest_sell_sol > 0.0 && seen > self.cfg.max_largest_sell_sol;
        under || over
    }

    fn arm_buy(&mut self, px: f64, age: f64, total: f64, why: &str) {
        let mcap = if px > 0.0 { px * 1_000_000_000.0 } else { 0.0 };
        let chase = if self.st.px0 > 0.0 && px > 0.0 {
            px / self.st.px0 - 1.0
        } else {
            0.0
        };
        let price_ratio = if self.st.px0 > 0.0 && px > 0.0 {
            round(px / self.st.px0, 4)
        } else {
            0.0
        };
        self.st.hit_sol = total;
        self.st.phase = PHASE_PENDING;
        self.st.why = why.to_string();
        self.st.entry_px = px;
        self.st.diag = BuyDiag {
            wall_s: round(age, 3),
            end_ret: round(chase, 4),
            mcap: round(mcap, 3),
            buy_hit_sol: round(total, 4),
            largest_buy_sol: round(self.st.largest_buy_sol, 4),
            price_ratio,
        };
        clear_streak(&mut self.st);
    }

    fn clock_entry(&mut self, px: f64) -> Option<&'static str> {
        if self.st.phase != PHASE_SEEK_SELL || px <= 0.0 {
            return None;
        }
        let age = self.age_s(self.st.now_ms);
        if self.cfg.min_entry_s > 0.0 && age < self.cfg.min_entry_s {
            return None;
        }
        if self.cfg.max_entry_s > 0.0 && age > self.cfg.max_entry_s {
            self.expire();
            return None;
        }
        if self.mcap_blocks(px) || self.largest_buy_blocks() || self.price_ratio_blocks(px) {
            return None;
        }
        self.arm_buy(px, age, 0.0, "clock");
        Some("BUY")
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

        self.note_hold_sell(side, sol, is_target, is_own);
        let signal = self.flow_inner(side, sol, tokens, px, is_target, is_own, now_ms, order);
        if signal != Some("BUY") {
            self.note_other_buy(side, sol, is_target, is_own);
        }
        signal
    }

    fn flow_inner(
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
            self.st.largest_buy_sol = 0.0;
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

        if self.st.phase == PHASE_SEEK_SELL && self.cfg.buy_hit_count < 1 {
            return self.clock_entry(px);
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
            if self.mcap_blocks(px) || self.largest_buy_blocks() || self.price_ratio_blocks(px) {
                self.st.prints.remove(0);
                return None;
            }
            self.arm_buy(px, age, total, "buy_hit");
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
        if self.hold_sell_exit(now_ms) {
            self.st.phase = PHASE_WAIT;
            self.st.why = "hold_sell".to_string();
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
    use crate::config::StrategyV081Config;

    const T0: u64 = 1_700_000_000_000;
    const PX: f64 = 50e-9;

    fn cfg(overrides: impl FnOnce(&mut StrategyV081Config)) -> StrategyV081Config {
        let mut cfg = StrategyV081Config::default_v081();
        overrides(&mut cfg);
        cfg
    }

    fn print_at(side: &'static str, sol: f64, signature: &str, ts: u64, price: f64) -> MarketEvent {
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
            price,
        }
    }

    fn print(side: &'static str, sol: f64, signature: &str, ts: u64) -> MarketEvent {
        print_at(side, sol, signature, ts, PX)
    }

    fn bound_at(
        px: f64,
        overrides: impl FnOnce(&mut StrategyV081Config),
        own: &str,
    ) -> StrategyV081Engine {
        let mut engine = StrategyV081Engine::new(cfg(overrides), own);
        engine.bind_gate("target", 1_000.0, px, 1.0, T0);
        engine
    }

    fn bound(overrides: impl FnOnce(&mut StrategyV081Config), own: &str) -> StrategyV081Engine {
        bound_at(PX, overrides, own)
    }

    fn seed(engine: &mut StrategyV081Engine, sol: f64) {
        assert_eq!(engine.on_event(&print("BUY", sol, "seed", T0 + 200)), None);
    }

    fn arm(engine: &mut StrategyV081Engine) {
        seed(engine, 3.0);
        assert_eq!(
            engine.on_event(&print("SELL", 0.1, "clock", T0 + 500)),
            Some("BUY")
        );
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
        let loaded = StrategyV081Config::default_v081();
        assert_eq!(
            loaded.gate_wallet,
            "ardinRsN1mNYVeoJWTBsWeYeXvuR9UUDGMsCDKpb6AT"
        );
        assert_eq!(loaded.dust_sol, 0.3);
        assert_eq!(loaded.min_entry_s, 0.5);
        assert_eq!(loaded.max_entry_s, 8.0);
        assert_eq!(loaded.min_price_ratio, 1.0);
        assert_eq!(loaded.max_price_ratio, 1.5);
        assert_eq!(loaded.buy_hit_count, 0);
        assert_eq!(loaded.buy_hit_min, 0.0);
        assert_eq!(loaded.min_mc_sol, 0.0);
        assert_eq!(loaded.max_mc_sol, 80.0);
        assert_eq!(loaded.min_largest_buy_sol, 2.0);
        assert_eq!(loaded.max_largest_buy_sol, 6.0);
        assert_eq!(loaded.sell_hit_count, 3);
        assert_eq!(loaded.sell_hit_min, 4.0);
        assert_eq!(loaded.take_profit, 0.4);
        assert_eq!(loaded.max_hold_s, 30.0);
        assert_eq!(loaded.hold_sell_s, 8.0);
        assert_eq!(loaded.min_largest_sell_sol, 2.0);
        assert_eq!(loaded.max_largest_sell_sol, 5.0);
        assert!(loaded.first_cycle_only);
        assert!(loaded.target_sell_exit);
        assert!(!loaded.second_buy_exit);
        assert!(loaded.repeat_entries);
        assert_eq!(loaded.size_sol, 0.3);
    }

    #[test]
    fn buys_first_print_inside_the_clock() {
        let mut engine = bound(|_| {}, "");
        assert_eq!(engine.phase_name(), "seek_sell");
        assert!(engine.needs_pool_tape());
        assert_eq!(
            engine.on_event(&print("SELL", 0.1, "too-early", T0 + 499)),
            None
        );
        seed(&mut engine, 3.0);
        assert_eq!(
            engine.on_event(&print("BUY", 10.0, "signal", T0 + 500)),
            Some("BUY")
        );
        assert_eq!(engine.phase_name(), "pending");
        assert_eq!(engine.last_buy_reason(), "clock");
        assert_eq!(engine.last_buy_diag().wall_s, 0.5);
        assert_eq!(engine.last_buy_diag().buy_hit_sol, 0.0);
        assert_eq!(engine.last_buy_diag().largest_buy_sol, 3.0);
        assert_eq!(engine.last_buy_diag().price_ratio, 1.0);
        assert_eq!(engine.last_buy_diag().mcap, 50.0);
        assert_eq!(engine.last_buy_diag().end_ret, 0.0);
    }

    #[test]
    fn clock_edges_ratio_and_mcap_are_inclusive() {
        let mut early = bound(|_| {}, "");
        seed(&mut early, 3.0);
        assert_eq!(early.on_event(&print("SELL", 0.1, "early", T0 + 499)), None);
        assert_eq!(early.phase_name(), "seek_sell");

        let mut open = bound(|_| {}, "");
        seed(&mut open, 2.0);
        assert_eq!(
            open.on_event(&print("SELL", 0.1, "open", T0 + 8_000)),
            Some("BUY")
        );
        assert_eq!(open.last_buy_diag().wall_s, 8.0);
        assert_eq!(open.last_buy_diag().largest_buy_sol, 2.0);

        let mut late = bound(|_| {}, "");
        seed(&mut late, 3.0);
        assert_eq!(late.on_event(&print("SELL", 0.1, "late", T0 + 8_001)), None);
        assert_eq!(late.phase_name(), "wait");
        assert!(!late.needs_pool_tape());

        let mut ratio = bound(|_| {}, "");
        seed(&mut ratio, 3.0);
        assert_eq!(
            ratio.on_event(&print_at("SELL", 0.1, "hot", T0 + 500, 76e-9)),
            None
        );
        assert_eq!(
            ratio.on_event(&print_at("SELL", 0.1, "cap", T0 + 600, 75e-9)),
            Some("BUY")
        );
        assert_eq!(ratio.last_buy_diag().price_ratio, 1.5);
        assert_eq!(ratio.last_buy_diag().mcap, 75.0);

        let mut cheap = bound(|_| {}, "");
        seed(&mut cheap, 3.0);
        assert_eq!(
            cheap.on_event(&print_at("SELL", 0.1, "under", T0 + 500, 49e-9)),
            None
        );
        assert_eq!(cheap.phase_name(), "seek_sell");

        let mut mcap = bound_at(60e-9, |_| {}, "");
        seed(&mut mcap, 3.0);
        assert_eq!(
            mcap.on_event(&print_at("SELL", 0.1, "over-mc", T0 + 500, 81e-9)),
            None
        );
        assert_eq!(
            mcap.on_event(&print_at("SELL", 0.1, "at-mc", T0 + 600, 80e-9)),
            Some("BUY")
        );
        assert_eq!(mcap.last_buy_diag().mcap, 80.0);
    }

    #[test]
    fn largest_buy_bounds_include_dust_and_exclude_the_signal() {
        let mut small = bound(|_| {}, "");
        seed(&mut small, 1.99);
        assert_eq!(small.on_event(&print("SELL", 0.1, "short", T0 + 500)), None);
        seed(&mut small, 2.0);
        assert_eq!(
            small.on_event(&print("SELL", 0.1, "floor", T0 + 600)),
            Some("BUY")
        );

        let mut wide = bound(|_| {}, "");
        seed(&mut wide, 6.0);
        assert_eq!(wide.on_event(&print("BUY", 6.01, "wider", T0 + 300)), None);
        assert_eq!(
            wide.on_event(&print("SELL", 0.1, "blocked", T0 + 500)),
            None
        );
        assert_eq!(wide.phase_name(), "seek_sell");

        let mut dust = bound(|cfg| cfg.min_largest_buy_sol = 0.2, "");
        assert_eq!(dust.on_event(&print("BUY", 0.2, "dusty", T0 + 200)), None);
        assert_eq!(
            dust.on_event(&print("SELL", 0.1, "ok", T0 + 500)),
            Some("BUY")
        );
        assert_eq!(dust.last_buy_diag().largest_buy_sol, 0.2);

        let mut target = bound(|_| {}, "");
        assert_eq!(
            target.on_event(&target_trade("BUY", 10.0, 100.0, "bag", T0 + 200, 2)),
            None
        );
        assert_eq!(
            target.on_event(&print("SELL", 0.1, "no-other", T0 + 500)),
            None
        );

        let mut excluded = bound(|_| {}, "");
        seed(&mut excluded, 3.0);
        assert_eq!(
            excluded.on_event(&print("BUY", 10.0, "signal-buy", T0 + 500)),
            Some("BUY")
        );
        assert_eq!(excluded.last_buy_diag().largest_buy_sol, 3.0);
    }

    #[test]
    fn sells_on_buy_cluster_take_profit_hold_or_target() {
        let mut cluster = bound(|_| {}, "");
        arm(&mut cluster);
        cluster.on_buy_fill(PX, T0 + 500);
        assert_eq!(cluster.phase_name(), "hold");
        assert_eq!(cluster.on_event(&print("BUY", 1.0, "a", T0 + 600)), None);
        assert_eq!(cluster.on_event(&print("BUY", 1.0, "b", T0 + 700)), None);
        assert_eq!(
            cluster.on_event(&print("BUY", 2.0, "c", T0 + 800)),
            Some("SELL")
        );
        assert!(cluster.last_sell_reason().contains("sell_hit"));

        let mut tp = bound(|_| {}, "");
        arm(&mut tp);
        tp.on_buy_fill(PX, T0 + 500);
        let mut under = print("SELL", 0.05, "under", T0 + 600);
        under.price = PX * 1.39;
        assert_eq!(tp.on_event(&under), None);
        let mut took = print("SELL", 0.05, "tp", T0 + 700);
        took.price = PX * 1.5;
        assert_eq!(tp.on_event(&took), Some("SELL"));
        assert_eq!(tp.last_sell_reason(), "take_profit");

        let mut held = bound(|_| {}, "");
        arm(&mut held);
        held.on_buy_fill(PX, T0 + 500);
        assert_eq!(
            held.on_event(&print("SELL", 3.0, "in-band", T0 + 1_000)),
            None
        );
        assert_eq!(held.on_clock(T0 + 500 + 8_000, PX), None);
        assert_eq!(held.on_clock(T0 + 500 + 29_999, PX), None);
        assert_eq!(held.on_clock(T0 + 500 + 30_000, PX), Some("SELL"));
        assert_eq!(held.last_sell_reason(), "hold_time");

        let mut target = bound(|_| {}, "");
        arm(&mut target);
        target.on_buy_fill(PX, T0 + 500);
        let mut partial = print("SELL", 0.05, "partial", T0 + 600);
        partial.wallet = "target".to_string();
        partial.token_amount = 100.0;
        assert_eq!(target.on_event(&partial), None);
        let mut dumped = print("SELL", 0.05, "max", T0 + 700);
        dumped.wallet = "target".to_string();
        dumped.token_amount = 900.0;
        assert_eq!(target.on_event(&dumped), Some("SELL"));
        assert_eq!(target.last_sell_reason(), "target_sell");
        assert_eq!(target.phase_name(), "done");
    }

    #[test]
    fn hold_sell_exits_outside_the_largest_sell_band() {
        let mut quiet = bound(|_| {}, "");
        arm(&mut quiet);
        quiet.on_buy_fill(PX, T0 + 500);
        assert_eq!(quiet.on_clock(T0 + 500 + 7_999, PX), None);
        assert_eq!(quiet.on_clock(T0 + 500 + 8_000, PX), Some("SELL"));
        assert_eq!(quiet.last_sell_reason(), "hold_sell 0.00 SOL");

        let mut floor = bound(|_| {}, "");
        arm(&mut floor);
        floor.on_buy_fill(PX, T0 + 500);
        assert_eq!(
            floor.on_event(&print("SELL", 2.0, "floor", T0 + 1_000)),
            None
        );
        assert_eq!(floor.on_clock(T0 + 500 + 8_000, PX), None);

        let mut cap = bound(|_| {}, "");
        arm(&mut cap);
        cap.on_buy_fill(PX, T0 + 500);
        assert_eq!(cap.on_event(&print("SELL", 5.0, "cap", T0 + 1_000)), None);
        assert_eq!(cap.on_clock(T0 + 500 + 8_000, PX), None);

        let mut over = bound(|_| {}, "");
        arm(&mut over);
        over.on_buy_fill(PX, T0 + 500);
        assert_eq!(
            over.on_event(&print("SELL", 5.01, "over", T0 + 1_000)),
            None
        );
        assert_eq!(over.on_clock(T0 + 500 + 8_000, PX), Some("SELL"));
        assert_eq!(over.last_sell_reason(), "hold_sell 5.01 SOL");

        let mut target = bound(|_| {}, "");
        arm(&mut target);
        target.on_buy_fill(PX, T0 + 500);
        let mut dumped = print("SELL", 10.0, "target-sell", T0 + 1_000);
        dumped.wallet = "target".to_string();
        dumped.token_amount = 100.0;
        assert_eq!(target.on_event(&dumped), None);
        assert_eq!(target.on_clock(T0 + 500 + 8_000, PX), Some("SELL"));
        assert_eq!(target.last_sell_reason(), "hold_sell 0.00 SOL");
    }

    #[test]
    fn own_sell_does_not_break_the_buy_window() {
        let mut engine = bound(|_| {}, "me");
        arm(&mut engine);
        engine.on_buy_fill(PX, T0 + 500);
        assert_eq!(engine.on_event(&print("BUY", 1.5, "a", T0 + 600)), None);
        assert_eq!(engine.on_event(&print("BUY", 1.5, "b", T0 + 700)), None);
        let mut own = print("SELL", 5.0, "own", T0 + 800);
        own.wallet = "me".to_string();
        assert_eq!(engine.on_event(&own), None);
        assert_eq!(engine.phase_name(), "hold");
        assert_eq!(
            engine.on_event(&print("BUY", 1.5, "c", T0 + 900)),
            Some("SELL")
        );
        assert!(engine.last_sell_reason().contains("sell_hit"));
    }

    #[test]
    fn repeat_entries_reseeks_until_the_clock_or_max_sell() {
        let mut engine = bound(|_| {}, "");
        arm(&mut engine);
        engine.on_buy_fill(PX, T0 + 500);
        let mut took = print("SELL", 0.05, "tp", T0 + 700);
        took.price = PX * 1.5;
        assert_eq!(engine.on_event(&took), Some("SELL"));
        engine.on_sell_fill();
        assert_eq!(engine.phase_name(), "seek_sell");
        assert_eq!(
            engine.on_event(&print("SELL", 0.1, "again", T0 + 1_000)),
            Some("BUY")
        );

        let mut late = bound(|_| {}, "");
        arm(&mut late);
        late.on_buy_fill(PX, T0 + 500);
        let mut late_tp = print("SELL", 0.05, "late-tp", T0 + 9_000);
        late_tp.price = PX * 1.5;
        assert_eq!(late.on_event(&late_tp), Some("SELL"));
        late.on_sell_fill();
        assert_eq!(late.phase_name(), "wait");

        let mut failed = bound(|_| {}, "");
        arm(&mut failed);
        failed.on_buy_failed();
        assert_eq!(failed.phase_name(), "seek_sell");
        assert_eq!(
            failed.on_event(&print("SELL", 0.1, "retry", T0 + 800)),
            Some("BUY")
        );

        let mut once = bound(|cfg| cfg.repeat_entries = false, "");
        arm(&mut once);
        once.on_buy_failed();
        assert_eq!(once.phase_name(), "wait");
        assert_eq!(once.on_event(&print("SELL", 0.1, "no", T0 + 800)), None);
    }

    #[test]
    fn second_buy_sells_a_hold_and_first_cycle_never_reopens() {
        let mut engine = bound(|cfg| cfg.second_buy_exit = true, "");
        arm(&mut engine);
        engine.on_buy_fill(PX, T0 + 500);
        assert_eq!(
            engine.on_event(&target_trade("BUY", 0.0, 100.0, "zero", T0 + 600, 2)),
            None
        );
        assert_eq!(engine.phase_name(), "hold");
        assert_eq!(
            engine.on_event(&target_trade("BUY", 1.0, 100.0, "second", T0 + 700, 3)),
            Some("SELL")
        );
        assert_eq!(engine.last_sell_reason(), "second_buy");
        engine.on_sell_fill();
        assert_eq!(engine.phase_name(), "wait");
        assert_eq!(
            engine.on_event(&print("SELL", 0.1, "still", T0 + 800)),
            None
        );

        let mut seeking = bound(|cfg| cfg.second_buy_exit = true, "");
        assert_eq!(
            seeking.on_event(&target_trade("BUY", 1.0, 500.0, "second", T0 + 200, 2)),
            None
        );
        assert_eq!(seeking.phase_name(), "wait");

        let mut once = bound(|_| {}, "");
        let mut flat = print("SELL", 1.0, "flat", T0 + 200);
        flat.wallet = "target".to_string();
        flat.token_amount = 1_000.0;
        assert_eq!(once.on_event(&flat), None);
        assert_eq!(once.phase_name(), "done");
        let mut again = print("BUY", 1.0, "again", T0 + 30_000);
        again.wallet = "target".to_string();
        again.token_amount = 1_000.0;
        again.slot = 3;
        assert_eq!(once.on_event(&again), None);
        assert_eq!(once.phase_name(), "done");
    }

    #[test]
    fn sliding_sell_window_buys_when_the_streak_is_on() {
        let mut engine = bound(
            |cfg| {
                cfg.buy_hit_count = 2;
                cfg.buy_hit_min = 1.0;
                cfg.min_largest_buy_sol = 0.0;
                cfg.max_largest_buy_sol = 0.0;
                cfg.min_price_ratio = 0.0;
                cfg.max_price_ratio = 0.0;
            },
            "",
        );
        assert_eq!(
            engine.on_event(&print("SELL", 0.6, "early-a", T0 + 100)),
            None
        );
        assert_eq!(
            engine.on_event(&print("SELL", 0.6, "early-b", T0 + 200)),
            None
        );
        assert_eq!(engine.phase_name(), "seek_sell");
        assert_eq!(
            engine.on_event(&print("SELL", 0.6, "open", T0 + 500)),
            Some("BUY")
        );
        assert!(engine.last_buy_reason().contains("buy_hit"));
        assert_eq!(engine.last_buy_diag().buy_hit_sol, 1.2);
        assert_eq!(engine.last_buy_diag().wall_s, 0.5);
    }

    #[test]
    fn retries_failed_sell_and_applies_a_missed_target_sell() {
        let mut engine = bound(|_| {}, "");
        arm(&mut engine);
        engine.on_buy_fill(PX, T0 + 500);
        assert_eq!(engine.on_event(&print("BUY", 2.0, "a", T0 + 600)), None);
        assert_eq!(engine.on_event(&print("BUY", 2.0, "b", T0 + 700)), None);
        assert_eq!(
            engine.on_event(&print("BUY", 2.0, "c", T0 + 800)),
            Some("SELL")
        );
        engine.on_sell_failed(T0 + 800);
        assert_eq!(engine.phase_name(), "hold");
        assert_eq!(engine.on_clock(T0 + 1_000, PX), None);
        assert_eq!(engine.on_clock(T0 + 2_800, PX), Some("SELL"));
        assert!(engine.last_sell_reason().contains("sell_hit"));

        let mut missed = bound(|_| {}, "");
        arm(&mut missed);
        let mut during = print("SELL", 1.0, "during", T0 + 700);
        during.wallet = "target".to_string();
        during.token_amount = 1_000.0;
        assert_eq!(missed.on_event(&during), None);
        assert_eq!(missed.phase_name(), "done");
        missed.on_buy_fill(PX, T0 + 700);
        assert_eq!(missed.phase_name(), "hold");
        missed.apply_missed_target_sell();
        assert_eq!(missed.phase_name(), "done");
        assert_eq!(missed.last_sell_reason(), "target_sell");
    }
}
