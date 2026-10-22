//! strategy_v_031 — sliding sell-hit inside a time gate.
//!
//! Same flow as scalpingbot `strategy_v_031.py` (2026-10-22). Market cap is
//! the signal price times 1e9. A window outside the market-cap, largest-buy,
//! or price-ratio bounds slides and the search continues. A zero bound
//! disables that gate. The largest buy is the biggest other-wallet buy since
//! this cycle's target buy. The signal print is not part of that max, and a
//! buy at or below the dust still counts. `round_trip_window_s` is a trailing
//! holder window, not a hold clock. Our own prints are not other-wallet flow.
//! One instance per mint.

use std::collections::HashMap;

use crate::config::StrategyV031Config;
use crate::strategy::common::{
    ChainCursor, MarketEvent, SELL_RETRY_MS, cursor_of, is_max_sell, is_strictly_after, round,
};

pub const STRATEGY_NAME: &str = "strategy_v_031";

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
    rt_buy_ms: HashMap<String, u64>,
    rt_sell_ms: HashMap<String, u64>,
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
        rt_buy_ms: HashMap::new(),
        rt_sell_ms: HashMap::new(),
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

pub struct StrategyV031Engine {
    cfg: StrategyV031Config,
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

impl StrategyV031Engine {
    pub fn new(cfg: StrategyV031Config, own_wallet: impl Into<String>) -> Self {
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

    pub fn bind_gate(&mut self, wallet: &str, gate_token_amount: f64, price: f64, now_ms: u64) {
        self.target_wallet = wallet.to_string();
        self.st = fresh_state();
        self.st.target_tokens = gate_token_amount.max(0.0);
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
            &event.wallet,
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
                    "round_trip" => format!("round_trip {}", self.round_trip_count()),
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
        if !p.repeat_entries || self.st.phase != PHASE_WAIT {
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

    fn note_round_trip(
        &mut self,
        wallet: &str,
        side: u8,
        sol: f64,
        tokens: f64,
        is_target: bool,
        is_own: bool,
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
            self.st.rt_buy_ms.insert(wallet.to_string(), self.st.now_ms);
            return;
        }
        if side == SIDE_SELL && sol > 0.0 && tokens > 0.0 {
            self.st.rt_sell_ms.insert(wallet.to_string(), self.st.now_ms);
        }
    }

    fn round_trip_count(&self) -> usize {
        if self.cfg.round_trip_window_s <= 0.0 {
            return self
                .st
                .rt_buy_ms
                .keys()
                .filter(|wallet| self.st.rt_sell_ms.contains_key(*wallet))
                .count();
        }
        let span = self.cfg.round_trip_window_s * 1000.0;
        let now = self.st.now_ms as f64;
        self.st
            .rt_buy_ms
            .iter()
            .filter(|(wallet, buy_ms)| {
                let Some(sell_ms) = self.st.rt_sell_ms.get(*wallet) else {
                    return false;
                };
                now - **buy_ms as f64 <= span && now - *sell_ms as f64 <= span
            })
            .count()
    }

    fn round_trip_exit(&self) -> bool {
        if self.cfg.round_trip_window_s <= 0.0 {
            return false;
        }
        if self.age_s(self.st.now_ms) < self.cfg.round_trip_window_s {
            return false;
        }
        let n = self.round_trip_count();
        let under =
            self.cfg.min_round_trip_holders > 0 && n < self.cfg.min_round_trip_holders as usize;
        let over =
            self.cfg.max_round_trip_holders > 0 && n >= self.cfg.max_round_trip_holders as usize;
        under || over
    }

    fn flow_step(
        &mut self,
        side: u8,
        sol: f64,
        tokens: f64,
        px: f64,
        is_target: bool,
        is_own: bool,
        wallet: &str,
        now_ms: u64,
        order: ChainCursor,
    ) -> Option<&'static str> {
        self.st.now_ms = now_ms;
        self.note_round_trip(wallet, side, sol, tokens, is_target, is_own);
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
            self.st.rt_buy_ms.clear();
            self.st.rt_sell_ms.clear();
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
                || self.largest_buy_blocks()
                || self.price_ratio_blocks(px)
            {
                self.st.prints.remove(0);
                return None;
            }
            let price_ratio = if self.st.px0 > 0.0 && px > 0.0 {
                round(px / self.st.px0, 4)
            } else {
                0.0
            };
            self.st.hit_sol = total;
            self.st.phase = PHASE_PENDING;
            self.st.why = "buy_hit".to_string();
            self.st.entry_px = px;
            self.st.diag = BuyDiag {
                wall_s: round(age, 3),
                end_ret: round(chase, 4),
                mcap: round(mcap, 3),
                buy_hit_sol: round(total, 4),
                largest_buy_sol: round(self.st.largest_buy_sol, 4),
                price_ratio,
                round_trip_holders: self.round_trip_count() as u32,
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
        if self.round_trip_exit() {
            self.st.phase = PHASE_WAIT;
            self.st.why = "round_trip".to_string();
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
    use crate::config::StrategyV031Config;

    const T0: u64 = 1_700_000_000_000;

    fn cfg(overrides: impl FnOnce(&mut StrategyV031Config)) -> StrategyV031Config {
        let mut cfg = StrategyV031Config::default_v031();
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
            price: 50e-9,
        }
    }

    fn bound(overrides: impl FnOnce(&mut StrategyV031Config), own: &str) -> StrategyV031Engine {
        let mut engine = StrategyV031Engine::new(cfg(overrides), own);
        engine.bind_gate("target", 1_000.0, 50e-9, T0);
        engine
    }

    fn sells(
        engine: &mut StrategyV031Engine,
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
        engine: &mut StrategyV031Engine,
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

    fn arm(engine: &mut StrategyV031Engine) {
        assert_eq!(sells(engine, "arm", T0 + 5_000, 0.3), Some("BUY"));
    }

    #[test]
    fn loads_scalping_knobs() {
        let loaded = StrategyV031Config::default_v031();
        assert_eq!(
            loaded.gate_wallet,
            "3VUNtVtjjx5ckUojT7UocJ5fbuAJRsNUXNfTBnPte9vC"
        );
        assert_eq!(loaded.min_entry_s, 4.0);
        assert_eq!(loaded.max_entry_s, 16.0);
        assert_eq!(loaded.buy_hit_count, 4);
        assert_eq!(loaded.buy_hit_min, 1.0);
        assert_eq!(loaded.min_mc_sol, 40.0);
        assert_eq!(loaded.max_mc_sol, 0.0);
        assert_eq!(loaded.min_largest_buy_sol, 0.0);
        assert_eq!(loaded.max_largest_buy_sol, 5.0);
        assert_eq!(loaded.min_price_ratio, 0.0);
        assert_eq!(loaded.max_price_ratio, 0.0);
        assert_eq!(loaded.round_trip_window_s, 0.0);
        assert_eq!(loaded.min_round_trip_holders, 0);
        assert_eq!(loaded.max_round_trip_holders, 0);
        assert_eq!(loaded.sell_hit_count, 5);
        assert_eq!(loaded.sell_hit_min, 4.0);
        assert_eq!(loaded.take_profit, 0.4);
        assert_eq!(loaded.max_hold_s, 45.0);
        assert!(!loaded.first_cycle_only);
        assert!(!loaded.repeat_entries);
        assert_eq!(loaded.size_sol, 1.0);
    }

    #[test]
    fn buys_sliding_sell_window_once_clock_open() {
        let mut engine = bound(|_| {}, "");
        assert_eq!(engine.phase_name(), "seek_sell");
        assert!(engine.needs_pool_tape());
        assert_eq!(sells(&mut engine, "early", T0 + 1_000, 0.3), None);
        assert_eq!(engine.phase_name(), "seek_sell");
        assert_eq!(
            engine.on_event(&print("SELL", 0.3, "open", T0 + 5_000)),
            Some("BUY")
        );
        assert_eq!(engine.phase_name(), "pending");
        assert!(engine.last_buy_reason().contains("buy_hit"));
        assert_eq!(engine.last_buy_diag().buy_hit_sol, 1.2);
        assert_eq!(engine.last_buy_diag().wall_s, 5.0);
        assert_eq!(engine.last_buy_diag().end_ret, 0.0);
        assert_eq!(engine.last_buy_diag().mcap, 50.0);
        assert_eq!(engine.last_buy_diag().largest_buy_sol, 0.0);
        assert_eq!(engine.last_buy_diag().price_ratio, 1.0);
        assert_eq!(engine.last_buy_diag().round_trip_holders, 0);
    }

    #[test]
    fn largest_buy_above_five_sol_slides_the_window() {
        let mut capped = bound(|_| {}, "");
        let mut whale = print("BUY", 5.01, "whale", T0 + 1_000);
        whale.token_amount = 10.0;
        assert_eq!(capped.on_event(&whale), None);
        assert_eq!(sells(&mut capped, "blocked", T0 + 5_000, 0.3), None);
        assert_eq!(capped.phase_name(), "seek_sell");

        let mut equal = bound(|_| {}, "");
        let mut at_cap = print("BUY", 5.0, "at-cap", T0 + 1_000);
        at_cap.token_amount = 10.0;
        assert_eq!(equal.on_event(&at_cap), None);
        assert_eq!(sells(&mut equal, "ok", T0 + 5_000, 0.3), Some("BUY"));
        assert_eq!(equal.last_buy_diag().largest_buy_sol, 5.0);

        let mut dusty = bound(|cfg| cfg.min_largest_buy_sol = 0.05, "");
        assert_eq!(dusty.on_event(&print("BUY", 0.05, "dust", T0 + 1_000)), None);
        assert_eq!(sells(&mut dusty, "dust-ok", T0 + 5_000, 0.3), Some("BUY"));
        assert_eq!(dusty.last_buy_diag().largest_buy_sol, 0.05);

        let mut target = bound(|_| {}, "");
        assert_eq!(
            target.on_event(&{
                let mut bag = print("BUY", 9.0, "bag", T0 + 1_000);
                bag.wallet = "target".to_string();
                bag.token_amount = 100.0;
                bag.slot = 2;
                bag
            }),
            None
        );
        assert_eq!(sells(&mut target, "no-other", T0 + 5_000, 0.3), Some("BUY"));
        assert_eq!(target.last_buy_diag().largest_buy_sol, 0.0);

        let mut own = bound(|_| {}, "me");
        let mut mine = print("BUY", 9.0, "mine", T0 + 1_000);
        mine.wallet = "me".to_string();
        assert_eq!(own.on_event(&mine), None);
        assert_eq!(sells(&mut own, "own", T0 + 5_000, 0.3), Some("BUY"));
        assert_eq!(own.last_buy_diag().largest_buy_sol, 0.0);
    }

    #[test]
    fn price_ratio_bounds_are_inclusive() {
        let mut engine = bound(
            |cfg| {
                cfg.min_price_ratio = 1.0;
                cfg.max_price_ratio = 1.6;
            },
            "",
        );
        for i in 0..4 {
            let mut hot = print("SELL", 0.3, &format!("hot-{i}"), T0 + 5_000);
            hot.price = 50e-9 * 1.61;
            assert_eq!(engine.on_event(&hot), None);
        }
        assert_eq!(engine.phase_name(), "seek_sell");
        let mut cap = print("SELL", 0.3, "cap", T0 + 6_000);
        cap.price = 50e-9 * 1.6;
        assert_eq!(engine.on_event(&cap), Some("BUY"));
        assert_eq!(engine.last_buy_diag().price_ratio, 1.6);
    }

    #[test]
    fn round_trip_window_sells_when_too_few_holders_remain() {
        let mut quiet = bound(
            |cfg| {
                cfg.take_profit = 0.0;
                cfg.max_hold_s = 0.0;
                cfg.round_trip_window_s = 10.0;
                cfg.min_round_trip_holders = 1;
            },
            "me",
        );
        arm(&mut quiet);
        quiet.on_buy_fill(50e-9, T0 + 5_000);
        assert_eq!(quiet.on_clock(T0 + 9_999, 50e-9), None);
        assert_eq!(quiet.on_clock(T0 + 10_000, 50e-9), Some("SELL"));
        assert_eq!(quiet.last_sell_reason(), "round_trip 0");

        let mut one = bound(
            |cfg| {
                cfg.take_profit = 0.0;
                cfg.max_hold_s = 0.0;
                cfg.round_trip_window_s = 10.0;
                cfg.min_round_trip_holders = 1;
            },
            "me",
        );
        arm(&mut one);
        one.on_buy_fill(50e-9, T0 + 5_000);
        let mut bought = print("BUY", 0.2, "holder-buy", T0 + 6_000);
        bought.wallet = "holder".to_string();
        bought.token_amount = 10.0;
        assert_eq!(one.on_event(&bought), None);
        let mut sold = print("SELL", 0.2, "holder-sell", T0 + 7_000);
        sold.wallet = "holder".to_string();
        sold.token_amount = 4.0;
        assert_eq!(one.on_event(&sold), None);
        assert_eq!(one.on_clock(T0 + 16_000, 50e-9), None);
        assert_eq!(one.on_clock(T0 + 16_001, 50e-9), Some("SELL"));
        assert_eq!(one.last_sell_reason(), "round_trip 0");

        let mut crowded = bound(
            |cfg| {
                cfg.take_profit = 0.0;
                cfg.max_hold_s = 0.0;
                cfg.round_trip_window_s = 10.0;
                cfg.max_round_trip_holders = 1;
            },
            "me",
        );
        arm(&mut crowded);
        crowded.on_buy_fill(50e-9, T0 + 5_000);
        let mut first = print("SELL", 0.2, "first", T0 + 11_000);
        first.wallet = "holder".to_string();
        first.token_amount = 1.0;
        assert_eq!(crowded.on_event(&first), None);
        let mut second = print("BUY", 0.2, "second", T0 + 12_000);
        second.wallet = "holder".to_string();
        second.token_amount = 1.0;
        assert_eq!(crowded.on_event(&second), Some("SELL"));
        assert_eq!(crowded.last_sell_reason(), "round_trip 1");
    }

    #[test]
    fn slides_window_outside_mcap_band() {
        let mut low = bound(|_| {}, "");
        for i in 0..4 {
            let mut cheap = print("SELL", 0.3, &format!("cheap-{i}"), T0 + 5_000);
            cheap.price = 39e-9;
            assert_eq!(low.on_event(&cheap), None);
        }
        assert_eq!(low.phase_name(), "seek_sell");
        let mut floor = print("SELL", 0.3, "floor", T0 + 6_000);
        floor.price = 40e-9;
        assert_eq!(low.on_event(&floor), Some("BUY"));
        assert_eq!(low.last_buy_diag().mcap, 40.0);

        let mut rich = bound(|_| {}, "");
        for i in 0..4 {
            let mut high = print("SELL", 0.3, &format!("rich-{i}"), T0 + 5_000);
            high.price = 500e-9;
            let signal = rich.on_event(&high);
            if i < 3 {
                assert_eq!(signal, None);
            } else {
                assert_eq!(signal, Some("BUY"));
            }
        }
        assert_eq!(rich.last_buy_diag().mcap, 500.0);

        let mut capped = bound(|cfg| cfg.max_mc_sol = 100.0, "");
        for i in 0..4 {
            let mut high = print("SELL", 0.3, &format!("cap-{i}"), T0 + 5_000);
            high.price = 101e-9;
            assert_eq!(capped.on_event(&high), None);
        }
        assert_eq!(capped.phase_name(), "seek_sell");
        assert_eq!(
            capped.on_event(&print("SELL", 0.3, "under-cap", T0 + 6_000)),
            Some("BUY")
        );
        assert_eq!(capped.last_buy_diag().mcap, 50.0);
    }

    #[test]
    fn slides_short_sell_window_until_sum_qualifies() {
        let mut engine = bound(|_| {}, "");
        assert_eq!(sells(&mut engine, "short", T0 + 5_000, 0.2), None);
        assert_eq!(engine.phase_name(), "seek_sell");
        assert_eq!(
            engine.on_event(&print("SELL", 0.4, "slide", T0 + 6_000)),
            Some("BUY")
        );
        assert_eq!(engine.last_buy_diag().buy_hit_sol, 1.0);
    }

    #[test]
    fn ends_cycle_when_window_finishes_after_clock() {
        let mut engine = bound(|_| {}, "");
        assert_eq!(sells(&mut engine, "late", T0 + 16_001, 0.3), None);
        assert_eq!(engine.phase_name(), "wait");
        assert!(!engine.needs_pool_tape());

        let mut edge = bound(|_| {}, "");
        assert_eq!(sells(&mut edge, "edge", T0 + 16_000, 0.3), Some("BUY"));
    }

    #[test]
    fn sells_hold_on_cluster_tp_hold_time_or_target() {
        let mut cluster = bound(|cfg| cfg.take_profit = 0.0, "");
        arm(&mut cluster);
        cluster.on_buy_fill(50e-9, T0 + 5_000);
        assert_eq!(cluster.phase_name(), "hold");
        assert_eq!(buys(&mut cluster, "short", T0 + 6_000, 0.5, 4), None);
        assert_eq!(
            cluster.on_event(&print("BUY", 2.5, "slide", T0 + 7_000)),
            Some("SELL")
        );
        assert!(cluster.last_sell_reason().contains("sell_hit"));

        let mut tp = bound(|_| {}, "");
        arm(&mut tp);
        let fill = 50e-9;
        tp.on_buy_fill(fill, T0 + 5_000);
        let mut under = print("SELL", 0.05, "under", T0 + 6_000);
        under.price = fill * 1.39;
        assert_eq!(tp.on_event(&under), None);
        let mut took = print("SELL", 0.05, "tp", T0 + 6_000);
        took.price = fill * 1.41;
        assert_eq!(tp.on_event(&took), Some("SELL"));
        assert_eq!(tp.last_sell_reason(), "take_profit");

        let mut held = bound(|cfg| cfg.take_profit = 0.0, "");
        arm(&mut held);
        held.on_buy_fill(fill, T0 + 5_000);
        assert_eq!(held.on_clock(T0 + 5_000 + 44_999, fill), None);
        assert_eq!(held.on_clock(T0 + 5_000 + 45_000, fill), Some("SELL"));
        assert_eq!(held.last_sell_reason(), "hold_time");

        let mut target = bound(|cfg| cfg.take_profit = 0.0, "");
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
        assert_eq!(target.phase_name(), "idle");
    }

    #[test]
    fn first_cycle_only_never_reopens() {
        let mut engine = bound(|cfg| cfg.first_cycle_only = true, "");
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
        let mut engine = bound(
            |cfg| {
                cfg.repeat_entries = true;
                cfg.take_profit = 0.25;
            },
            "",
        );
        arm(&mut engine);
        engine.on_buy_fill(50e-9, T0 + 5_000);
        let mut tp = print("SELL", 0.05, "tp", T0 + 8_000);
        tp.price = 50e-9 * 1.25;
        assert_eq!(engine.on_event(&tp), Some("SELL"));
        assert_eq!(engine.phase_name(), "wait");
        engine.on_sell_fill();
        assert_eq!(engine.phase_name(), "seek_sell");
        assert_eq!(sells(&mut engine, "again", T0 + 10_000, 0.3), Some("BUY"));
    }

    #[test]
    fn retries_failed_sell() {
        let mut engine = bound(|cfg| cfg.take_profit = 0.0, "");
        arm(&mut engine);
        engine.on_buy_fill(50e-9, T0 + 5_000);
        assert_eq!(buys(&mut engine, "exit", T0 + 6_000, 1.0, 5), Some("SELL"));
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
        assert_eq!(engine.phase_name(), "idle");
        engine.on_buy_fill(50e-9, T0 + 6_000);
        assert_eq!(engine.phase_name(), "hold");
        engine.apply_missed_target_sell();
        assert_eq!(engine.phase_name(), "idle");
        assert_eq!(engine.last_sell_reason(), "target_sell");
    }
}
