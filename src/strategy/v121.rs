//! strategy_v_121 — sliding sell-hit inside a time clock.
//!
//! Same flow as scalpingbot `strategy_v_121.py`, for
//! CyaE1VxvBrahnPWkqm5VsdCvyS2QmNht2UFrKJHga54o.
//! Knobs from `config/strategy_v_121.json` (2026-10-24).
//! The buy is the last `buy_hit_count` sells, each above dust, summing to
//! `buy_hit_min`, finishing from `min_entry_s` through `max_entry_s` after
//! this cycle's target buy. `buy_hit_count` of 0 buys the first print inside
//! that clock. A zero bound disables that gate. Market cap is the signal
//! price times 1e9. The largest buy is the biggest other-wallet buy since
//! this cycle's target buy, and the signal print is not part of that max.
//! Round-trip holders bought after that target buy and later sold. The signal
//! print counts, and each wallet counts once. A count equal to a bound still
//! passes. Our own prints are not other-wallet flow. Max sell is 80% of the
//! bag. Later cycles are allowed. One instance per mint.

use std::collections::{HashMap, HashSet};

use crate::config::StrategyV121Config;
use crate::strategy::common::{
    ChainCursor, MarketEvent, SELL_RETRY_MS, cursor_of, is_max_sell, is_strictly_after, round,
};

pub const STRATEGY_NAME: &str = "strategy_v_121";

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
    target_buys: u32,
    second_buy_seen: bool,
    now_ms: u64,
    rt_buy_at: HashMap<String, ChainCursor>,
    rt_sold: HashSet<String>,
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
    pub round_trip_holders: u32,
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
        target_buys: 0,
        second_buy_seen: false,
        now_ms: 0,
        rt_buy_at: HashMap::new(),
        rt_sold: HashSet::new(),
        diag: BuyDiag::default(),
        opened_at: None,
        closed_at: None,
    }
}

fn clear_streak(st: &mut FlowState) {
    st.streak_side = 0;
    st.prints.clear();
}

pub struct StrategyV121Engine {
    cfg: StrategyV121Config,
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

impl StrategyV121Engine {
    pub fn new(cfg: StrategyV121Config, own_wallet: impl Into<String>) -> Self {
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
            &event.wallet,
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

    fn note_other_buy(&mut self, side: u8, sol: f64, is_target: bool, is_own: bool) {
        if self.st.phase != PHASE_SEEK_SELL || is_target || is_own || side != SIDE_BUY || sol <= 0.0
        {
            return;
        }
        if sol > self.st.largest_buy_sol {
            self.st.largest_buy_sol = sol;
        }
    }

    fn note_round_trip(
        &mut self,
        wallet: &str,
        side: u8,
        sol: f64,
        tokens: f64,
        is_target: bool,
        is_own: bool,
        order: ChainCursor,
    ) {
        if is_target
            || is_own
            || wallet.is_empty()
            || self.st.phase == PHASE_IDLE
            || self.st.phase == PHASE_DONE
        {
            return;
        }
        if side == SIDE_BUY && sol > 0.0 {
            if !self.st.rt_buy_at.contains_key(wallet) && !self.st.rt_sold.contains(wallet) {
                self.st.rt_buy_at.insert(wallet.to_string(), order);
            }
            return;
        }
        if side != SIDE_SELL || sol <= 0.0 || tokens <= 0.0 {
            return;
        }
        let bought_after = self
            .st
            .rt_buy_at
            .get(wallet)
            .is_some_and(|bought_at| is_strictly_after(order, *bought_at));
        if bought_after {
            self.st.rt_sold.insert(wallet.to_string());
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

    fn price_ratio(&self, px: f64) -> f64 {
        if self.st.px0 <= 0.0 || px <= 0.0 {
            0.0
        } else {
            round(px / self.st.px0, 4)
        }
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
        let ratio = self.price_ratio(px);
        if lo > 0.0 && ratio < lo {
            return true;
        }
        if hi > 0.0 && ratio > hi {
            return true;
        }
        false
    }

    fn round_trip_blocks(&self) -> bool {
        let lo = self.cfg.min_buy_round_trip_holders;
        let hi = self.cfg.max_buy_round_trip_holders;
        if lo == 0 && hi == 0 {
            return false;
        }
        let n = self.st.rt_sold.len() as u32;
        if lo > 0 && n < lo {
            return true;
        }
        if hi > 0 && n > hi {
            return true;
        }
        false
    }

    fn mcap_blocks(&self, px: f64) -> bool {
        let mcap = if px > 0.0 { px * 1_000_000_000.0 } else { 0.0 };
        (self.cfg.min_mc_sol > 0.0 && mcap < self.cfg.min_mc_sol)
            || (self.cfg.max_mc_sol > 0.0 && mcap > self.cfg.max_mc_sol)
    }

    fn entry_blocks(&self, px: f64) -> bool {
        self.mcap_blocks(px)
            || self.largest_buy_blocks()
            || self.price_ratio_blocks(px)
            || self.round_trip_blocks()
    }

    fn arm_buy(&mut self, px: f64, age: f64, total: f64, why: &str) {
        let mcap = if px > 0.0 { px * 1_000_000_000.0 } else { 0.0 };
        let ratio = self.price_ratio(px);
        let end_ret = if ratio > 0.0 { round(ratio - 1.0, 4) } else { 0.0 };
        self.st.hit_sol = total;
        self.st.phase = PHASE_PENDING;
        self.st.why = why.to_string();
        self.st.entry_px = px;
        self.st.diag = BuyDiag {
            wall_s: round(age, 3),
            end_ret,
            mcap: round(mcap, 3),
            buy_hit_sol: round(total, 4),
            largest_buy_sol: round(self.st.largest_buy_sol, 4),
            price_ratio: ratio,
            round_trip_holders: self.st.rt_sold.len() as u32,
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
        if self.entry_blocks(px) {
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
        wallet: &str,
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

        self.note_round_trip(wallet, side, sol, tokens, is_target, is_own, order);
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
            self.st.rt_buy_at.clear();
            self.st.rt_sold.clear();
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
            if self.entry_blocks(px) {
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
    use crate::config::StrategyV121Config;

    const T0: u64 = 1_700_000_000_000;
    const PX0: f64 = 40e-9;
    const SIGNAL: f64 = 60e-9;

    fn cfg(overrides: impl FnOnce(&mut StrategyV121Config)) -> StrategyV121Config {
        let mut cfg = StrategyV121Config::default_v121();
        overrides(&mut cfg);
        cfg
    }

    fn print(side: &'static str, sol: f64, signature: &str, ts: u64) -> MarketEvent {
        MarketEvent {
            signature: signature.to_string(),
            slot: 1,
            transaction_index: Some(0),
            event_index: Some(0),
            timestamp_ms: ts,
            side,
            wallet: "other".to_string(),
            sol_amount: sol,
            token_amount: 0.0,
            price: SIGNAL,
        }
    }

    fn bound(overrides: impl FnOnce(&mut StrategyV121Config), own: &str) -> StrategyV121Engine {
        let mut engine = StrategyV121Engine::new(cfg(overrides), own);
        engine.bind_gate("target", 1_000.0, PX0, 1.0, T0);
        engine
    }

    fn loose(cfg: &mut StrategyV121Config) {
        cfg.min_entry_s = 0.0;
        cfg.max_entry_s = 0.0;
        cfg.min_mc_sol = 0.0;
        cfg.max_mc_sol = 0.0;
        cfg.min_largest_buy_sol = 0.0;
        cfg.max_largest_buy_sol = 0.0;
        cfg.min_price_ratio = 0.0;
        cfg.max_price_ratio = 0.0;
        cfg.min_buy_round_trip_holders = 0;
        cfg.max_buy_round_trip_holders = 0;
    }

    fn at_slot(mut event: MarketEvent, slot: u64) -> MarketEvent {
        event.slot = slot;
        event
    }

    fn round_trip(engine: &mut StrategyV121Engine, name: &str, slot: u64, ts: u64) {
        let mut buy = print("BUY", 0.05, &format!("{name}-b"), ts);
        buy.wallet = name.to_string();
        buy.token_amount = 10.0;
        buy.slot = slot;
        assert_eq!(engine.on_event(&buy), None);
        let mut sell = print("SELL", 0.05, &format!("{name}-s"), ts);
        sell.wallet = name.to_string();
        sell.token_amount = 10.0;
        sell.slot = slot;
        sell.transaction_index = Some(1);
        assert_eq!(engine.on_event(&sell), None);
    }

    fn seed_holders(engine: &mut StrategyV121Engine, n: u32) {
        for i in 0..n {
            round_trip(engine, &format!("h{i}"), 10 + u64::from(i), T0 + 1_000);
        }
    }

    fn arm_default(engine: &mut StrategyV121Engine) {
        seed_holders(engine, 10);
        assert_eq!(
            engine.on_event(&at_slot(print("BUY", 4.0, "big", T0 + 6_000), 80)),
            None
        );
        assert_eq!(
            engine.on_event(&at_slot(print("SELL", 0.5, "s0", T0 + 7_000), 90)),
            None
        );
        assert_eq!(
            engine.on_event(&at_slot(print("SELL", 0.5, "s1", T0 + 7_000), 91)),
            None
        );
        assert_eq!(
            engine.on_event(&at_slot(print("SELL", 0.5, "s2", T0 + 7_000), 92)),
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
        let loaded = StrategyV121Config::default_v121();
        assert_eq!(
            loaded.gate_wallet,
            "CyaE1VxvBrahnPWkqm5VsdCvyS2QmNht2UFrKJHga54o"
        );
        assert_eq!(loaded.dust_sol, 0.2);
        assert_eq!(loaded.min_entry_s, 7.0);
        assert_eq!(loaded.max_entry_s, 20.0);
        assert_eq!(loaded.buy_hit_count, 3);
        assert_eq!(loaded.buy_hit_min, 1.5);
        assert_eq!(loaded.min_mc_sol, 0.0);
        assert_eq!(loaded.max_mc_sol, 80.0);
        assert_eq!(loaded.min_largest_buy_sol, 4.0);
        assert_eq!(loaded.max_largest_buy_sol, 0.0);
        assert_eq!(loaded.min_price_ratio, 1.0);
        assert_eq!(loaded.max_price_ratio, 2.0);
        assert_eq!(loaded.min_buy_round_trip_holders, 10);
        assert_eq!(loaded.max_buy_round_trip_holders, 30);
        assert_eq!(loaded.sell_hit_count, 3);
        assert_eq!(loaded.sell_hit_min, 5.0);
        assert_eq!(loaded.take_profit, 0.15);
        assert_eq!(loaded.max_hold_s, 30.0);
        assert!(!loaded.first_cycle_only);
        assert!(loaded.target_sell_exit);
        assert!(!loaded.second_buy_exit);
        assert!(!loaded.repeat_entries);
        assert_eq!(loaded.size_sol, 0.3);
    }

    #[test]
    fn buys_the_sell_window_inside_the_published_gates() {
        let mut engine = bound(|_| {}, "");
        assert_eq!(engine.phase_name(), "seek_sell");
        seed_holders(&mut engine, 9);
        assert_eq!(
            engine.on_event(&at_slot(print("BUY", 4.0, "big", T0 + 6_000), 80)),
            None
        );
        assert_eq!(
            engine.on_event(&at_slot(print("SELL", 0.5, "early0", T0 + 6_999), 81)),
            None
        );
        assert_eq!(
            engine.on_event(&at_slot(print("SELL", 0.5, "early1", T0 + 6_999), 82)),
            None
        );
        assert_eq!(
            engine.on_event(&at_slot(print("SELL", 0.5, "early2", T0 + 6_999), 83)),
            None
        );
        assert_eq!(engine.phase_name(), "seek_sell");

        let mut ninth = print("SELL", 0.5, "ninth", T0 + 7_000);
        ninth.wallet = "late".to_string();
        ninth.token_amount = 10.0;
        ninth.slot = 84;
        ninth.transaction_index = Some(1);
        let mut late_buy = print("BUY", 0.05, "late-b", T0 + 6_500);
        late_buy.wallet = "late".to_string();
        late_buy.token_amount = 10.0;
        late_buy.slot = 84;
        late_buy.transaction_index = Some(0);
        assert_eq!(engine.on_event(&late_buy), None);
        assert_eq!(
            engine.on_event(&at_slot(print("SELL", 0.5, "s0", T0 + 7_000), 85)),
            None
        );
        assert_eq!(engine.on_event(&ninth), Some("BUY"));
        assert_eq!(engine.phase_name(), "pending");
        assert_eq!(engine.last_buy_reason(), "buy_hit 1.50 SOL");
        let diag = engine.last_buy_diag();
        assert_eq!(diag.buy_hit_sol, 1.5);
        assert_eq!(diag.wall_s, 7.0);
        assert_eq!(diag.price_ratio, 1.5);
        assert_eq!(diag.end_ret, 0.5);
        assert_eq!(diag.mcap, 60.0);
        assert_eq!(diag.largest_buy_sol, 4.0);
        assert_eq!(diag.round_trip_holders, 10);
    }

    #[test]
    fn blocks_outside_mcap_largest_buy_and_price_ratio() {
        let mut small = bound(|_| {}, "");
        seed_holders(&mut small, 10);
        assert_eq!(
            small.on_event(&at_slot(print("BUY", 3.99, "short", T0 + 6_000), 80)),
            None
        );
        assert_eq!(
            small.on_event(&at_slot(print("SELL", 0.5, "a", T0 + 7_000), 90)),
            None
        );
        assert_eq!(
            small.on_event(&at_slot(print("SELL", 0.5, "b", T0 + 7_000), 91)),
            None
        );
        assert_eq!(
            small.on_event(&at_slot(print("SELL", 0.5, "c", T0 + 7_000), 92)),
            None
        );
        assert_eq!(small.phase_name(), "seek_sell");

        let mut wide = bound(|_| {}, "");
        seed_holders(&mut wide, 10);
        assert_eq!(
            wide.on_event(&at_slot(print("BUY", 4.0, "floor", T0 + 6_000), 80)),
            None
        );
        let mut over_mc = print("SELL", 0.5, "mc", T0 + 8_000);
        over_mc.price = 80.001e-9;
        over_mc.slot = 93;
        assert_eq!(wide.on_event(&at_slot(print("SELL", 0.5, "a", T0 + 8_000), 90)), None);
        assert_eq!(wide.on_event(&at_slot(print("SELL", 0.5, "b", T0 + 8_000), 91)), None);
        assert_eq!(wide.on_event(&over_mc), None);

        let mut cap = bound(|_| {}, "");
        seed_holders(&mut cap, 10);
        assert_eq!(
            cap.on_event(&at_slot(print("BUY", 4.0, "floor", T0 + 6_000), 80)),
            None
        );
        let mut at_cap = print("SELL", 0.5, "cap", T0 + 8_000);
        at_cap.price = 80e-9;
        at_cap.slot = 93;
        assert_eq!(cap.on_event(&at_slot(print("SELL", 0.5, "a", T0 + 8_000), 90)), None);
        assert_eq!(cap.on_event(&at_slot(print("SELL", 0.5, "b", T0 + 8_000), 91)), None);
        assert_eq!(cap.on_event(&at_cap), Some("BUY"));
        assert_eq!(cap.last_buy_diag().mcap, 80.0);
        assert_eq!(cap.last_buy_diag().price_ratio, 2.0);

        let mut ratio = bound(|cfg| cfg.max_mc_sol = 0.0, "");
        seed_holders(&mut ratio, 10);
        assert_eq!(
            ratio.on_event(&at_slot(print("BUY", 4.0, "floor", T0 + 6_000), 80)),
            None
        );
        let mut over_ratio = print("SELL", 0.5, "ratio", T0 + 8_000);
        over_ratio.price = PX0 * 2.01;
        over_ratio.slot = 93;
        assert_eq!(ratio.on_event(&at_slot(print("SELL", 0.5, "a", T0 + 8_000), 90)), None);
        assert_eq!(ratio.on_event(&at_slot(print("SELL", 0.5, "b", T0 + 8_000), 91)), None);
        assert_eq!(ratio.on_event(&over_ratio), None);
        assert_eq!(ratio.phase_name(), "seek_sell");
    }

    #[test]
    fn round_trip_edges_are_inclusive_and_the_signal_sell_counts() {
        let mut under = bound(
            |cfg| {
                loose(cfg);
                cfg.min_buy_round_trip_holders = 2;
                cfg.max_buy_round_trip_holders = 3;
            },
            "me",
        );
        round_trip(&mut under, "one", 10, T0 + 1_000);
        assert_eq!(under.on_event(&print("SELL", 0.5, "a", T0 + 2_000)), None);
        assert_eq!(under.on_event(&print("SELL", 0.5, "b", T0 + 2_000)), None);
        assert_eq!(under.on_event(&print("SELL", 0.5, "c", T0 + 2_000)), None);
        assert_eq!(under.phase_name(), "seek_sell");

        let mut mine = print("BUY", 1.0, "mine-b", T0 + 2_100);
        mine.wallet = "me".to_string();
        mine.token_amount = 10.0;
        mine.slot = 11;
        assert_eq!(under.on_event(&mine), None);
        let mut mine_sell = print("SELL", 1.0, "mine-s", T0 + 2_200);
        mine_sell.wallet = "me".to_string();
        mine_sell.token_amount = 10.0;
        mine_sell.slot = 11;
        mine_sell.transaction_index = Some(1);
        assert_eq!(under.on_event(&mine_sell), None);

        let mut target_buy = target_trade("BUY", 1.0, 10.0, "bag", T0 + 2_300, 12);
        target_buy.transaction_index = Some(0);
        assert_eq!(under.on_event(&target_buy), None);
        let mut target_sell = target_trade("SELL", 1.0, 5.0, "trim", T0 + 2_400, 12);
        target_sell.transaction_index = Some(1);
        assert_eq!(under.on_event(&target_sell), None);

        let mut second_buy = print("BUY", 0.05, "two-b", T0 + 2_500);
        second_buy.wallet = "two".to_string();
        second_buy.token_amount = 10.0;
        second_buy.slot = 13;
        assert_eq!(under.on_event(&second_buy), None);
        assert_eq!(under.on_event(&print("SELL", 0.5, "d", T0 + 2_600)), None);
        assert_eq!(under.on_event(&print("SELL", 0.5, "e", T0 + 2_600)), None);
        let mut second_sell = print("SELL", 0.5, "two-s", T0 + 2_700);
        second_sell.wallet = "two".to_string();
        second_sell.token_amount = 10.0;
        second_sell.slot = 13;
        second_sell.transaction_index = Some(1);
        assert_eq!(under.on_event(&second_sell), Some("BUY"));
        assert_eq!(under.last_buy_diag().round_trip_holders, 2);

        let mut crowded = bound(
            |cfg| {
                loose(cfg);
                cfg.min_buy_round_trip_holders = 1;
                cfg.max_buy_round_trip_holders = 1;
            },
            "",
        );
        round_trip(&mut crowded, "one", 10, T0 + 1_000);
        round_trip(&mut crowded, "two", 11, T0 + 1_100);
        assert_eq!(crowded.on_event(&print("SELL", 0.5, "a", T0 + 2_000)), None);
        assert_eq!(crowded.on_event(&print("SELL", 0.5, "b", T0 + 2_000)), None);
        assert_eq!(crowded.on_event(&print("SELL", 0.5, "c", T0 + 2_000)), None);
        assert_eq!(crowded.phase_name(), "seek_sell");
    }

    #[test]
    fn sell_window_slides_and_dust_does_not_break_it() {
        let mut engine = bound(loose, "");
        assert_eq!(engine.on_event(&print("SELL", 0.5, "a", T0 + 1_000)), None);
        assert_eq!(engine.on_event(&print("SELL", 0.2, "dust", T0 + 1_100)), None);
        assert_eq!(engine.on_event(&print("BUY", 0.5, "break", T0 + 1_200)), None);
        assert_eq!(engine.on_event(&print("SELL", 0.4, "b", T0 + 1_300)), None);
        assert_eq!(engine.on_event(&print("SELL", 0.4, "c", T0 + 1_400)), None);
        assert_eq!(engine.on_event(&print("SELL", 0.4, "d", T0 + 1_500)), None);
        assert_eq!(engine.phase_name(), "seek_sell");
        assert_eq!(
            engine.on_event(&print("SELL", 0.7, "e", T0 + 1_600)),
            Some("BUY")
        );
        assert_eq!(engine.last_buy_diag().buy_hit_sol, 1.5);
    }

    #[test]
    fn clock_closes_at_max_entry_and_can_buy_the_first_print() {
        let mut late = bound(|_| {}, "");
        assert_eq!(
            late.on_event(&print("SELL", 0.5, "late", T0 + 20_001)),
            None
        );
        assert_eq!(late.phase_name(), "wait");
        assert!(!late.needs_pool_tape());

        let mut clock = bound(
            |cfg| {
                loose(cfg);
                cfg.buy_hit_count = 0;
                cfg.min_entry_s = 7.0;
                cfg.max_entry_s = 20.0;
            },
            "",
        );
        assert_eq!(clock.on_event(&print("SELL", 0.05, "early", T0 + 6_999)), None);
        assert_eq!(
            clock.on_event(&print("BUY", 9.0, "signal", T0 + 7_000)),
            Some("BUY")
        );
        assert_eq!(clock.last_buy_reason(), "clock");
        assert_eq!(clock.last_buy_diag().buy_hit_sol, 0.0);
        assert_eq!(clock.last_buy_diag().largest_buy_sol, 0.0);
    }

    #[test]
    fn sells_on_buy_cluster_take_profit_hold_time_or_target() {
        let mut cluster = bound(loose, "");
        assert_eq!(cluster.on_event(&print("SELL", 0.5, "a", T0 + 1_000)), None);
        assert_eq!(cluster.on_event(&print("SELL", 0.5, "b", T0 + 1_000)), None);
        assert_eq!(
            cluster.on_event(&print("SELL", 0.5, "c", T0 + 1_000)),
            Some("BUY")
        );
        let fill = 80e-9;
        cluster.on_buy_fill(fill, T0 + 2_000);
        assert_eq!(cluster.phase_name(), "hold");
        assert_eq!(cluster.on_event(&print("BUY", 2.0, "x", T0 + 3_000)), None);
        assert_eq!(cluster.on_event(&print("BUY", 2.0, "y", T0 + 3_000)), None);
        assert_eq!(
            cluster.on_event(&print("BUY", 1.0, "z", T0 + 3_000)),
            Some("SELL")
        );
        assert_eq!(cluster.last_sell_reason(), "sell_hit 5.00 SOL");

        let mut tp = bound(loose, "");
        assert_eq!(tp.on_event(&print("SELL", 0.5, "a", T0 + 1_000)), None);
        assert_eq!(tp.on_event(&print("SELL", 0.5, "b", T0 + 1_000)), None);
        assert_eq!(tp.on_event(&print("SELL", 0.5, "c", T0 + 1_000)), Some("BUY"));
        tp.on_buy_fill(fill, T0 + 2_000);
        let mut under = print("SELL", 0.05, "under", T0 + 3_000);
        under.price = fill * 1.14;
        assert_eq!(tp.on_event(&under), None);
        let mut took = print("SELL", 0.05, "tp", T0 + 3_000);
        took.price = fill * 1.16;
        assert_eq!(tp.on_event(&took), Some("SELL"));
        assert_eq!(tp.last_sell_reason(), "take_profit");
        tp.on_sell_fill();
        assert_eq!(tp.phase_name(), "wait");
        assert_eq!(tp.on_event(&print("SELL", 0.5, "again", T0 + 4_000)), None);

        let mut held = bound(loose, "");
        assert_eq!(held.on_event(&print("SELL", 0.5, "a", T0 + 1_000)), None);
        assert_eq!(held.on_event(&print("SELL", 0.5, "b", T0 + 1_000)), None);
        assert_eq!(held.on_event(&print("SELL", 0.5, "c", T0 + 1_000)), Some("BUY"));
        held.on_buy_fill(fill, T0 + 2_000);
        assert_eq!(held.on_clock(T0 + 2_000 + 29_999, fill), None);
        assert_eq!(held.on_clock(T0 + 2_000 + 30_000, fill), Some("SELL"));
        assert_eq!(held.last_sell_reason(), "hold_time");

        let mut target = bound(|_| {}, "");
        arm_default(&mut target);
        target.on_buy_fill(fill, T0 + 8_000);
        let partial = target_trade("SELL", 0.05, 100.0, "partial", T0 + 9_000, 100);
        assert_eq!(target.on_event(&partial), None);
        let dumped = target_trade("SELL", 0.05, 900.0, "max", T0 + 10_000, 101);
        assert_eq!(target.on_event(&dumped), Some("SELL"));
        assert_eq!(target.last_sell_reason(), "target_sell");
        assert_eq!(target.phase_name(), "idle");
    }

    #[test]
    fn later_cycle_restarts_after_the_bag_is_cleared() {
        let mut engine = bound(loose, "");
        let flat = target_trade("SELL", 1.0, 1_000.0, "flat", T0 + 1_000, 2);
        assert_eq!(engine.on_event(&flat), None);
        assert_eq!(engine.phase_name(), "idle");
        assert!(!engine.needs_pool_tape());
        let again = target_trade("BUY", 1.0, 800.0, "again", T0 + 30_000, 3);
        assert_eq!(engine.on_event(&again), None);
        assert_eq!(engine.phase_name(), "seek_sell");
        assert!(engine.needs_pool_tape());
        assert_eq!(
            engine.on_event(&print("SELL", 0.5, "a", T0 + 31_000)),
            None
        );
        assert_eq!(
            engine.on_event(&print("SELL", 0.5, "b", T0 + 31_000)),
            None
        );
        assert_eq!(
            engine.on_event(&print("SELL", 0.5, "c", T0 + 31_000)),
            Some("BUY")
        );
    }

    #[test]
    fn first_cycle_only_never_reopens() {
        let mut engine = bound(
            |cfg| {
                loose(cfg);
                cfg.first_cycle_only = true;
            },
            "",
        );
        let flat = target_trade("SELL", 1.0, 1_000.0, "flat", T0 + 1_000, 2);
        assert_eq!(engine.on_event(&flat), None);
        assert_eq!(engine.phase_name(), "done");
        let again = target_trade("BUY", 1.0, 800.0, "again", T0 + 30_000, 3);
        assert_eq!(engine.on_event(&again), None);
        assert_eq!(engine.phase_name(), "done");
    }

    #[test]
    fn repeat_entries_reseeks_on_the_same_clock() {
        let mut engine = bound(
            |cfg| {
                loose(cfg);
                cfg.repeat_entries = true;
                cfg.max_entry_s = 20.0;
            },
            "",
        );
        assert_eq!(engine.on_event(&print("SELL", 0.5, "a", T0 + 1_000)), None);
        assert_eq!(engine.on_event(&print("SELL", 0.5, "b", T0 + 1_000)), None);
        assert_eq!(
            engine.on_event(&print("SELL", 0.5, "c", T0 + 1_000)),
            Some("BUY")
        );
        engine.on_buy_fill(PX0, T0 + 2_000);
        let mut took = print("SELL", 0.05, "tp", T0 + 3_000);
        took.price = PX0 * 1.16;
        assert_eq!(engine.on_event(&took), Some("SELL"));
        engine.on_sell_fill();
        assert_eq!(engine.phase_name(), "seek_sell");
        assert_eq!(
            engine.on_event(&print("SELL", 0.5, "d", T0 + 4_000)),
            None
        );
        assert_eq!(
            engine.on_event(&print("SELL", 0.5, "e", T0 + 4_000)),
            None
        );
        assert_eq!(
            engine.on_event(&print("SELL", 0.5, "f", T0 + 4_000)),
            Some("BUY")
        );
    }

    #[test]
    fn second_buy_sells_a_hold_and_blocks_another_entry() {
        let mut engine = bound(
            |cfg| {
                loose(cfg);
                cfg.second_buy_exit = true;
            },
            "",
        );
        assert_eq!(engine.on_event(&print("SELL", 0.5, "a", T0 + 1_000)), None);
        assert_eq!(engine.on_event(&print("SELL", 0.5, "b", T0 + 1_000)), None);
        assert_eq!(
            engine.on_event(&print("SELL", 0.5, "c", T0 + 1_000)),
            Some("BUY")
        );
        engine.on_buy_fill(80e-9, T0 + 2_000);
        assert_eq!(
            engine.on_event(&target_trade("BUY", 0.0, 100.0, "zero", T0 + 3_000, 2)),
            None
        );
        assert_eq!(engine.phase_name(), "hold");
        assert_eq!(
            engine.on_event(&target_trade("BUY", 1.0, 100.0, "second", T0 + 4_000, 3)),
            Some("SELL")
        );
        assert_eq!(engine.last_sell_reason(), "second_buy");
        engine.on_sell_fill();
        assert_eq!(engine.phase_name(), "wait");
    }

    #[test]
    fn retries_failed_sell() {
        let mut engine = bound(loose, "");
        assert_eq!(engine.on_event(&print("SELL", 0.5, "a", T0 + 1_000)), None);
        assert_eq!(engine.on_event(&print("SELL", 0.5, "b", T0 + 1_000)), None);
        assert_eq!(
            engine.on_event(&print("SELL", 0.5, "c", T0 + 1_000)),
            Some("BUY")
        );
        engine.on_buy_fill(80e-9, T0 + 2_000);
        assert_eq!(engine.on_event(&print("BUY", 2.0, "x", T0 + 3_000)), None);
        assert_eq!(engine.on_event(&print("BUY", 2.0, "y", T0 + 3_000)), None);
        assert_eq!(
            engine.on_event(&print("BUY", 1.0, "z", T0 + 3_000)),
            Some("SELL")
        );
        engine.on_sell_failed(T0 + 3_000);
        assert_eq!(engine.phase_name(), "hold");
        assert_eq!(engine.on_clock(T0 + 4_000, 0.0), None);
        assert_eq!(engine.on_clock(T0 + 5_000, 0.0), Some("SELL"));
        assert_eq!(engine.last_sell_reason(), "sell_hit 5.00 SOL");
    }

    #[test]
    fn applies_missed_target_sell_after_fill() {
        let mut engine = bound(|_| {}, "");
        let during = target_trade("SELL", 1.0, 1_000.0, "during", T0 + 6_000, 4);
        assert_eq!(engine.on_event(&during), None);
        assert_eq!(engine.phase_name(), "idle");
        engine.on_buy_fill(80e-9, T0 + 6_000);
        assert_eq!(engine.phase_name(), "hold");
        engine.apply_missed_target_sell();
        assert_eq!(engine.phase_name(), "idle");
        assert_eq!(engine.last_sell_reason(), "target_sell");
    }
}
