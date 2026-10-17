//! strategy_v_071 — clock entry inside a market-cap band, then a buy-run exit.
//!
//! Same flow as scalpingbot `strategy_v_071.py`, for
//! 3PwXa9BRA2sUdjtcGxs4bVLBau2dHTuZwtb6CbpTpdvA.
//! Knobs from `config/strategy_v_071.json` (2026-10-17).
//! A `min_entry_s`, `max_entry_s`, `min_mc_sol`, `max_mc_sol`, or `max_hold_s`
//! of 0 disables that bound. Max sell is 95% of the bag, matching that book.
//! One instance per mint.

use crate::config::StrategyV071Config;
use crate::strategy::common::{
    ChainCursor, MarketEvent, SELL_RETRY_MS, cursor_of, is_strictly_after, round,
};

pub const STRATEGY_NAME: &str = "strategy_v_071";

const PHASE_IDLE: u8 = 0;
const PHASE_SEEK_SELL: u8 = 1;
const PHASE_PENDING: u8 = 2;
const PHASE_HOLD: u8 = 3;
const PHASE_WAIT: u8 = 4;
const PHASE_DONE: u8 = 5;

const SIDE_BUY: u8 = 1;
const SIDE_SELL: u8 = 2;

const MAX_SELL_FRAC: f64 = 0.95;

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

fn is_max_sell(held: f64, sold: f64) -> bool {
    if sold <= 0.0 {
        return false;
    }
    if held <= 0.0 {
        return true;
    }
    sold >= held * MAX_SELL_FRAC
}

pub struct StrategyV071Engine {
    cfg: StrategyV071Config,
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

impl StrategyV071Engine {
    pub fn new(cfg: StrategyV071Config, own_wallet: impl Into<String>) -> Self {
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
                self.buy_reason = "clock".to_string();
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
        let mcap = px * 1_000_000_000.0;
        let chase = if self.st.px0 > 0.0 {
            px / self.st.px0 - 1.0
        } else {
            0.0
        };
        if (self.cfg.min_mc_sol > 0.0 && mcap < self.cfg.min_mc_sol)
            || (self.cfg.max_mc_sol > 0.0 && mcap > self.cfg.max_mc_sol)
        {
            return None;
        }
        self.st.hit_sol = 0.0;
        self.st.phase = PHASE_PENDING;
        self.st.why = "clock".to_string();
        self.st.entry_px = px;
        self.st.diag = BuyDiag {
            wall_s: round(age, 3),
            end_ret: round(chase, 4),
            mcap: round(mcap, 3),
            buy_hit_sol: 0.0,
        };
        clear_streak(&mut self.st);
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

        if self.st.phase == PHASE_SEEK_SELL {
            return self.clock_entry(px);
        }

        if is_own {
            return self.risk_exit(px, now_ms);
        }

        if sol <= self.cfg.dust_sol || (side != SIDE_BUY && side != SIDE_SELL) {
            return self.risk_exit(px, now_ms);
        }
        if self.st.phase != PHASE_HOLD {
            return None;
        }

        if self.st.streak_side != side {
            self.st.streak_side = side;
            self.st.prints.clear();
        }
        self.st.prints.push(sol);
        let need = self.cfg.sell_hit_count.max(1) as usize;
        if self.st.prints.len() > need {
            let extra = self.st.prints.len() - need;
            self.st.prints.drain(0..extra);
        }
        if self.st.prints.len() < need {
            return self.risk_exit(px, now_ms);
        }

        let total: f64 = self.st.prints.iter().sum();
        if self.st.streak_side == SIDE_BUY && total >= self.cfg.sell_hit_min {
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
    use crate::config::StrategyV071Config;

    const T0: u64 = 1_700_000_000_000;

    fn cfg(overrides: impl FnOnce(&mut StrategyV071Config)) -> StrategyV071Config {
        let mut cfg = StrategyV071Config::default_v071();
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

    fn bound(overrides: impl FnOnce(&mut StrategyV071Config), own: &str) -> StrategyV071Engine {
        let mut engine = StrategyV071Engine::new(cfg(overrides), own);
        engine.bind_gate("target", 1_000.0, 50e-9, 1.0, T0);
        engine
    }

    fn arm(engine: &mut StrategyV071Engine) {
        assert_eq!(
            engine.on_event(&print("BUY", 0.1, "arm", T0 + 2_000)),
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
        let loaded = StrategyV071Config::default_v071();
        assert_eq!(
            loaded.gate_wallet,
            "3PwXa9BRA2sUdjtcGxs4bVLBau2dHTuZwtb6CbpTpdvA"
        );
        assert_eq!(loaded.dust_sol, 0.8);
        assert_eq!(loaded.min_entry_s, 1.5);
        assert_eq!(loaded.max_entry_s, 45.0);
        assert_eq!(loaded.min_mc_sol, 25.0);
        assert_eq!(loaded.max_mc_sol, 60.0);
        assert_eq!(loaded.sell_hit_count, 5);
        assert_eq!(loaded.sell_hit_min, 8.0);
        assert_eq!(loaded.take_profit, 1.2);
        assert_eq!(loaded.max_hold_s, 0.0);
        assert!(!loaded.first_cycle_only);
        assert!(loaded.target_sell_exit);
        assert!(!loaded.second_buy_exit);
        assert!(loaded.repeat_entries);
        assert_eq!(loaded.size_sol, 0.5);
    }

    #[test]
    fn buys_first_print_inside_clock_and_mcap_band() {
        let mut engine = bound(|_| {}, "");
        assert_eq!(engine.phase_name(), "seek_sell");
        assert!(engine.needs_pool_tape());
        assert_eq!(
            engine.on_event(&print("SELL", 0.1, "early", T0 + 1_499)),
            None
        );
        assert_eq!(engine.phase_name(), "seek_sell");

        let mut cheap = print("BUY", 1.0, "cheap", T0 + 2_000);
        cheap.price = 24e-9;
        assert_eq!(engine.on_event(&cheap), None);
        assert_eq!(engine.phase_name(), "seek_sell");

        let mut rich = print("BUY", 1.0, "rich", T0 + 3_000);
        rich.price = 61e-9;
        assert_eq!(engine.on_event(&rich), None);

        assert_eq!(
            engine.on_event(&print("SELL", 0.05, "in", T0 + 4_000)),
            Some("BUY")
        );
        assert_eq!(engine.phase_name(), "pending");
        assert_eq!(engine.last_buy_reason(), "clock");
        assert_eq!(engine.last_buy_diag().wall_s, 4.0);
        assert_eq!(engine.last_buy_diag().mcap, 50.0);
        assert_eq!(engine.last_buy_diag().end_ret, 0.0);
        assert_eq!(engine.last_buy_diag().buy_hit_sol, 0.0);
    }

    #[test]
    fn clock_edges_are_inclusive_and_late_print_expires() {
        let mut early_edge = bound(|_| {}, "");
        assert_eq!(
            early_edge.on_event(&print("BUY", 0.1, "edge", T0 + 1_500)),
            Some("BUY")
        );

        let mut late_edge = bound(|_| {}, "");
        assert_eq!(
            late_edge.on_event(&print("BUY", 0.1, "late-edge", T0 + 45_000)),
            Some("BUY")
        );

        let mut expired = bound(|_| {}, "");
        assert_eq!(
            expired.on_event(&print("BUY", 0.1, "late", T0 + 45_001)),
            None
        );
        assert_eq!(expired.phase_name(), "wait");
        assert!(!expired.needs_pool_tape());

        let mut open = bound(|cfg| cfg.max_entry_s = 0.0, "");
        assert_eq!(
            open.on_event(&print("BUY", 0.1, "open", T0 + 120_000)),
            Some("BUY")
        );
    }

    #[test]
    fn sells_hold_on_buy_run_tp_hold_time_or_target() {
        let mut cluster = bound(|cfg| cfg.take_profit = 0.0, "");
        arm(&mut cluster);
        cluster.on_buy_fill(50e-9, T0 + 2_000);
        assert_eq!(cluster.phase_name(), "hold");
        for i in 0..5 {
            assert_eq!(
                cluster.on_event(&print("BUY", 1.0, &format!("short-{i}"), T0 + 3_000)),
                None
            );
        }
        assert_eq!(
            cluster.on_event(&print("BUY", 0.2, "dust", T0 + 3_100)),
            None
        );
        assert_eq!(
            cluster.on_event(&print("BUY", 4.0, "slide", T0 + 4_000)),
            Some("SELL")
        );
        assert!(cluster.last_sell_reason().contains("sell_hit"));
        assert_eq!(cluster.phase_name(), "wait");

        let mut tp = bound(|_| {}, "");
        arm(&mut tp);
        let fill = 50e-9;
        tp.on_buy_fill(fill, T0 + 2_000);
        let mut under = print("SELL", 0.05, "under", T0 + 3_000);
        under.price = fill * 2.19;
        assert_eq!(tp.on_event(&under), None);
        let mut took = print("SELL", 0.05, "tp", T0 + 3_000);
        took.price = fill * 2.2;
        assert_eq!(tp.on_event(&took), Some("SELL"));
        assert_eq!(tp.last_sell_reason(), "take_profit");

        let mut held = bound(|cfg| cfg.take_profit = 0.0, "");
        arm(&mut held);
        held.on_buy_fill(fill, T0 + 2_000);
        assert_eq!(held.on_clock(T0 + 2_000 + 90_000, fill), None);
        assert_eq!(held.phase_name(), "hold");

        let mut capped = bound(
            |cfg| {
                cfg.take_profit = 0.0;
                cfg.max_hold_s = 45.0;
            },
            "",
        );
        arm(&mut capped);
        capped.on_buy_fill(fill, T0 + 2_000);
        assert_eq!(capped.on_clock(T0 + 2_000 + 44_999, fill), None);
        assert_eq!(capped.on_clock(T0 + 2_000 + 45_000, fill), Some("SELL"));
        assert_eq!(capped.last_sell_reason(), "hold_time");

        let mut target = bound(|cfg| cfg.take_profit = 0.0, "");
        arm(&mut target);
        target.on_buy_fill(fill, T0 + 2_000);
        assert_eq!(
            target.on_event(&target_trade("SELL", 0.05, 900.0, "partial", T0 + 3_000, 2)),
            None
        );
        assert_eq!(
            target.on_event(&target_trade("SELL", 0.05, 100.0, "max", T0 + 4_000, 3)),
            Some("SELL")
        );
        assert_eq!(target.last_sell_reason(), "target_sell");
        assert_eq!(target.phase_name(), "idle");
    }

    #[test]
    fn own_buy_does_not_complete_the_sell_run() {
        let mut engine = bound(|cfg| cfg.take_profit = 0.0, "me");
        arm(&mut engine);
        engine.on_buy_fill(50e-9, T0 + 2_000);
        for i in 0..4 {
            assert_eq!(
                engine.on_event(&print("BUY", 2.0, &format!("b-{i}"), T0 + 3_000)),
                None
            );
        }
        let mut own = print("BUY", 10.0, "own", T0 + 4_000);
        own.wallet = "me".to_string();
        assert_eq!(engine.on_event(&own), None);
        assert_eq!(engine.phase_name(), "hold");
        assert_eq!(
            engine.on_event(&print("BUY", 2.0, "fifth", T0 + 5_000)),
            Some("SELL")
        );
    }

    #[test]
    fn second_buy_sells_a_hold_and_blocks_another_entry() {
        let mut engine = bound(
            |cfg| {
                cfg.second_buy_exit = true;
                cfg.take_profit = 0.0;
            },
            "",
        );
        arm(&mut engine);
        engine.on_buy_fill(50e-9, T0 + 2_000);
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
        assert_eq!(engine.phase_name(), "wait");
        engine.on_sell_fill();
        assert_eq!(engine.phase_name(), "wait");
        assert_eq!(
            engine.on_event(&print("BUY", 1.0, "still", T0 + 5_000)),
            None
        );

        assert_eq!(
            engine.on_event(&target_trade("SELL", 1.0, 1_200.0, "flat", T0 + 6_000, 4)),
            None
        );
        assert_eq!(engine.phase_name(), "idle");
        assert_eq!(
            engine.on_event(&target_trade("BUY", 1.0, 1_000.0, "next", T0 + 7_000, 5)),
            None
        );
        assert_eq!(engine.phase_name(), "seek_sell");
        assert_eq!(
            engine.on_event(&print("BUY", 0.1, "again", T0 + 9_000)),
            Some("BUY")
        );
    }

    #[test]
    fn second_buy_ends_the_search_before_entry() {
        let mut engine = bound(|cfg| cfg.second_buy_exit = true, "");
        assert_eq!(
            engine.on_event(&target_trade("BUY", 1.0, 500.0, "second", T0 + 2_000, 2)),
            None
        );
        assert_eq!(engine.phase_name(), "wait");
        assert_eq!(
            engine.on_event(&print("BUY", 1.0, "too-late", T0 + 3_000)),
            None
        );
    }

    #[test]
    fn repeat_entries_reseeks_until_the_clock_or_max_sell() {
        let mut engine = bound(|_| {}, "");
        arm(&mut engine);
        engine.on_buy_fill(50e-9, T0 + 2_000);
        let mut tp = print("SELL", 0.05, "tp", T0 + 8_000);
        tp.price = 50e-9 * 2.2;
        assert_eq!(engine.on_event(&tp), Some("SELL"));
        engine.on_sell_fill();
        assert_eq!(engine.phase_name(), "seek_sell");
        assert_eq!(
            engine.on_event(&print("BUY", 0.1, "again", T0 + 10_000)),
            Some("BUY")
        );

        let mut late = bound(|_| {}, "");
        arm(&mut late);
        late.on_buy_fill(50e-9, T0 + 2_000);
        let mut late_tp = print("SELL", 0.05, "late-tp", T0 + 46_000);
        late_tp.price = 50e-9 * 2.2;
        assert_eq!(late.on_event(&late_tp), Some("SELL"));
        late.on_sell_fill();
        assert_eq!(late.phase_name(), "wait");
    }

    #[test]
    fn failed_buy_reseeks_on_the_same_clock() {
        let mut engine = bound(|_| {}, "");
        arm(&mut engine);
        engine.on_buy_failed();
        assert_eq!(engine.phase_name(), "seek_sell");
        assert_eq!(
            engine.on_event(&print("BUY", 0.1, "retry", T0 + 3_000)),
            Some("BUY")
        );

        let mut once = bound(|cfg| cfg.repeat_entries = false, "");
        arm(&mut once);
        once.on_buy_failed();
        assert_eq!(once.phase_name(), "wait");
    }

    #[test]
    fn first_cycle_only_never_reopens() {
        let mut engine = bound(|cfg| cfg.first_cycle_only = true, "");
        assert_eq!(
            engine.on_event(&target_trade("SELL", 1.0, 1_000.0, "flat", T0 + 1_000, 2)),
            None
        );
        assert_eq!(engine.phase_name(), "done");
        assert!(!engine.needs_pool_tape());
        assert_eq!(
            engine.on_event(&target_trade("BUY", 1.0, 1_000.0, "again", T0 + 30_000, 3)),
            None
        );
        assert_eq!(engine.phase_name(), "done");
    }

    #[test]
    fn retries_failed_sell() {
        let mut engine = bound(|cfg| cfg.take_profit = 0.0, "");
        arm(&mut engine);
        engine.on_buy_fill(50e-9, T0 + 2_000);
        for i in 0..4 {
            assert_eq!(
                engine.on_event(&print("BUY", 2.0, &format!("exit-{i}"), T0 + 3_000)),
                None
            );
        }
        assert_eq!(
            engine.on_event(&print("BUY", 2.0, "exit-4", T0 + 4_000)),
            Some("SELL")
        );
        engine.on_sell_failed(T0 + 4_000);
        assert_eq!(engine.phase_name(), "hold");
        assert_eq!(engine.on_clock(T0 + 5_000, 0.0), None);
        assert_eq!(engine.on_clock(T0 + 6_000, 0.0), Some("SELL"));
        assert!(engine.last_sell_reason().contains("sell_hit"));
    }

    #[test]
    fn applies_missed_target_sell_after_fill() {
        let mut engine = bound(|_| {}, "");
        arm(&mut engine);
        assert_eq!(
            engine.on_event(&target_trade("SELL", 1.0, 1_000.0, "during", T0 + 3_000, 2)),
            None
        );
        assert_eq!(engine.phase_name(), "idle");
        engine.on_buy_fill(50e-9, T0 + 3_000);
        assert_eq!(engine.phase_name(), "hold");
        engine.apply_missed_target_sell();
        assert_eq!(engine.phase_name(), "idle");
        assert_eq!(engine.last_sell_reason(), "target_sell");
    }
}
