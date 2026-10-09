//! strategy_v_022 — round-window sell-hit, then a buy-run exit.
//!
//! Faithful port of bot_target StrategyV022Engine. One instance per mint.

use crate::config::StrategyV022Config;
use crate::strategy::common::{
    cursor_of, is_max_sell, is_strictly_after, round, ChainCursor, MarketEvent, SELL_RETRY_MS,
};

pub const STRATEGY_NAME: &str = "strategy_v_022";

const PHASE_IDLE: u8 = 0;
const PHASE_SEEK_SELL: u8 = 1;
const PHASE_PENDING: u8 = 2;
const PHASE_HOLD: u8 = 3;
const PHASE_WAIT: u8 = 4;

const SIDE_BUY: u8 = 1;
const SIDE_SELL: u8 = 2;

const PHASE_NAMES: [&str; 5] = ["idle", "seek_sell", "pending", "hold", "wait"];

struct FlowState {
    phase: u8,
    streak_side: u8,
    prints: Vec<f64>,
    why: String,
    hits: u32,
    hit_sol: f64,
    exit_sol: f64,
    entry_px: f64,
    target_tokens: f64,
    opened_at: Option<ChainCursor>,
    closed_at: Option<ChainCursor>,
}

fn fresh_state() -> FlowState {
    FlowState {
        phase: PHASE_SEEK_SELL,
        streak_side: 0,
        prints: Vec::new(),
        why: String::new(),
        hits: 0,
        hit_sol: 0.0,
        exit_sol: 0.0,
        entry_px: 0.0,
        target_tokens: 0.0,
        opened_at: None,
        closed_at: None,
    }
}

fn clear_streak(st: &mut FlowState) {
    st.streak_side = 0;
    st.prints.clear();
}

#[derive(Clone, Debug)]
pub struct BuyDiag {
    pub buy_hit_sol: f64,
}

pub struct StrategyV022Engine {
    cfg: StrategyV022Config,
    own_wallet: String,
    st: FlowState,
    target_wallet: String,
    bound: bool,
    buy_reason: String,
    sell_reason: String,
    buy_diag: BuyDiag,
    exit_retry: bool,
    exit_retry_at_ms: u64,
}

impl StrategyV022Engine {
    pub fn new(cfg: StrategyV022Config, own_wallet: impl Into<String>) -> Self {
        let mut engine = Self {
            cfg,
            own_wallet: own_wallet.into(),
            st: fresh_state(),
            target_wallet: String::new(),
            bound: false,
            buy_reason: String::new(),
            sell_reason: String::new(),
            buy_diag: BuyDiag { buy_hit_sol: 0.0 },
            exit_retry: false,
            exit_retry_at_ms: 0,
        };
        engine.st.phase = PHASE_IDLE;
        engine
    }

    pub fn phase_name(&self) -> &'static str {
        PHASE_NAMES.get(self.st.phase as usize).copied().unwrap_or("idle")
    }

    pub fn needs_pool_tape(&self) -> bool {
        self.exit_retry
            || matches!(self.st.phase, PHASE_SEEK_SELL | PHASE_PENDING | PHASE_HOLD)
    }

    pub fn is_bound(&self) -> bool {
        self.bound
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

    pub fn bind_gate(&mut self, wallet: &str, gate_token_amount: f64) {
        self.target_wallet = wallet.to_string();
        self.st = fresh_state();
        self.st.target_tokens = gate_token_amount.max(0.0);
        self.bound = true;
        self.buy_reason.clear();
        self.sell_reason.clear();
        self.buy_diag = BuyDiag { buy_hit_sol: 0.0 };
        self.exit_retry = false;
        self.exit_retry_at_ms = 0;
    }

    pub fn on_buy_fill(&mut self, fill_px: f64) {
        if fill_px > 0.0 {
            self.st.entry_px = fill_px;
        }
        self.commit_buy(true);
    }

    pub fn on_buy_failed(&mut self) {
        self.commit_buy(false);
    }

    pub fn on_sell_fill(&mut self) {
        self.exit_retry = false;
        if self.st.phase != PHASE_HOLD {
            return;
        }
        self.st.phase = PHASE_WAIT;
        self.st.why = "external_exit".to_string();
        clear_streak(&mut self.st);
    }

    pub fn on_sell_failed(&mut self, now_ms: u64) {
        self.exit_retry = true;
        self.exit_retry_at_ms = now_ms + SELL_RETRY_MS;
        self.st.phase = PHASE_HOLD;
        clear_streak(&mut self.st);
    }

    pub fn on_clock(&mut self, now_ms: u64) -> Option<&'static str> {
        if !self.exit_retry || now_ms < self.exit_retry_at_ms {
            return None;
        }
        self.exit_retry = false;
        self.st.phase = PHASE_WAIT;
        clear_streak(&mut self.st);
        Some("SELL")
    }

    pub fn apply_missed_target_sell(&mut self) {
        if self.st.phase != PHASE_HOLD || !self.cfg.target_sell_exit {
            return;
        }
        self.st.phase = PHASE_IDLE;
        self.st.why = "target_sell".to_string();
        self.sell_reason = "target_sell".to_string();
        clear_streak(&mut self.st);
    }

    pub fn restore_hold(&mut self, entry_px: f64) {
        self.bound = true;
        self.st.phase = PHASE_HOLD;
        self.st.entry_px = entry_px;
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
            cursor_of(event),
        );
        match signal {
            Some("BUY") => {
                self.buy_reason = format!("buy_hit {:.2} SOL", self.st.hit_sol);
                self.buy_diag = BuyDiag {
                    buy_hit_sol: round(self.st.hit_sol, 4),
                };
                Some("BUY")
            }
            Some("SELL") => {
                self.sell_reason = match self.st.why.as_str() {
                    "sell_hit" => format!("sell_hit {:.2} SOL", self.st.exit_sol),
                    "stop" => "stop".to_string(),
                    "take_profit" => "take_profit".to_string(),
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

    fn flow_step(
        &mut self,
        side: u8,
        sol: f64,
        tokens: f64,
        px: f64,
        is_target: bool,
        is_own: bool,
        order: ChainCursor,
    ) -> Option<&'static str> {
        if is_target
            && side == SIDE_BUY
            && self.st.closed_at.is_some_and(|closed| !is_strictly_after(order, closed))
        {
            return None;
        }
        if is_target && side == SIDE_SELL && self.sell_already_passed(order) {
            return None;
        }

        if is_target && side == SIDE_BUY {
            self.st.target_tokens += tokens.max(0.0);
            self.st.opened_at = Some(order);
        }

        if is_target && side == SIDE_SELL {
            let max_sell = is_max_sell(self.st.target_tokens, tokens);
            self.st.target_tokens = (self.st.target_tokens - tokens.max(0.0)).max(0.0);
            if max_sell {
                if self.st.closed_at.is_none_or(|closed| is_strictly_after(order, closed)) {
                    self.st.closed_at = Some(order);
                }
                if self.st.phase == PHASE_HOLD && self.cfg.target_sell_exit {
                    self.st.phase = PHASE_IDLE;
                    self.st.why = "target_sell".to_string();
                    clear_streak(&mut self.st);
                    return Some("SELL");
                }
                if self.st.phase == PHASE_SEEK_SELL || self.st.phase == PHASE_PENDING {
                    self.st.phase = PHASE_IDLE;
                    self.st.hits = 0;
                    clear_streak(&mut self.st);
                    return None;
                }
                if self.st.phase == PHASE_WAIT {
                    self.st.phase = PHASE_IDLE;
                    return None;
                }
            }
        }

        if is_target && side == SIDE_BUY && self.st.phase == PHASE_IDLE {
            self.st.phase = PHASE_SEEK_SELL;
            self.st.hit_sol = 0.0;
            self.st.hits = 0;
            clear_streak(&mut self.st);
            return None;
        }

        if is_own {
            return self.risk_exit(px);
        }

        if sol <= self.cfg.dust_sol || (side != SIDE_BUY && side != SIDE_SELL) {
            return self.risk_exit(px);
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
        if self.st.prints.len() < need {
            return self.risk_exit(px);
        }

        let total: f64 = self.st.prints.iter().sum();
        let done = self.st.streak_side;
        if self.st.phase == PHASE_SEEK_SELL && done == SIDE_SELL && total >= self.cfg.buy_hit_min {
            self.st.hits += 1;
            self.st.prints.clear();
            if self.st.hits < self.cfg.buy_hit_round {
                return self.risk_exit(px);
            }
            let mcap = if px > 0.0 { px * 1_000_000_000.0 } else { 0.0 };
            if self.cfg.max_mc_sol > 0.0 && mcap > self.cfg.max_mc_sol {
                self.st.phase = PHASE_WAIT;
                self.st.hits = 0;
                clear_streak(&mut self.st);
                return None;
            }
            self.st.hit_sol = total;
            self.st.phase = PHASE_PENDING;
            self.st.why = "buy_hit".to_string();
            self.st.entry_px = px;
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
        // Sum short or wrong side — drop the finished round entirely (non-sliding).
        self.st.prints.clear();
        self.risk_exit(px)
    }

    fn sell_already_passed(&self, order: ChainCursor) -> bool {
        if self.st.opened_at.is_some_and(|opened| is_strictly_after(opened, order)) {
            return true;
        }
        if self.st.closed_at.is_some_and(|closed| is_strictly_after(closed, order)) {
            return true;
        }
        false
    }

    fn risk_exit(&mut self, px: f64) -> Option<&'static str> {
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
        if self.cfg.stop_loss > 0.0
            && self.st.entry_px > 0.0
            && px > 0.0
            && px / self.st.entry_px - 1.0 <= -self.cfg.stop_loss
        {
            self.st.phase = PHASE_WAIT;
            self.st.why = "stop".to_string();
            clear_streak(&mut self.st);
            return Some("SELL");
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::StrategyV022Config;

    fn cfg(overrides: impl FnOnce(&mut StrategyV022Config)) -> StrategyV022Config {
        let mut cfg = StrategyV022Config::default_v022();
        overrides(&mut cfg);
        cfg
    }

    fn print(side: &'static str, sol: f64, signature: &str) -> MarketEvent {
        MarketEvent {
            signature: signature.to_string(),
            slot: 1,
            transaction_index: None,
            event_index: None,
            timestamp_ms: 1_700_000_000_000,
            side,
            wallet: "other".to_string(),
            sol_amount: sol,
            token_amount: 0.0,
            price: 50e-9,
        }
    }

    fn bound(overrides: impl FnOnce(&mut StrategyV022Config), own: &str) -> StrategyV022Engine {
        let mut engine = StrategyV022Engine::new(cfg(overrides), own);
        engine.bind_gate("target", 0.0);
        engine
    }

    fn round_of_sells(
        engine: &mut StrategyV022Engine,
        tag: &str,
        price: Option<f64>,
        sol: f64,
    ) -> Option<&'static str> {
        for i in 1..=3 {
            let mut e = print("SELL", sol, &format!("{tag}-{i}"));
            if let Some(px) = price {
                e.price = px;
            }
            engine.on_event(&e);
        }
        let mut last = print("SELL", sol, &format!("{tag}-4"));
        if let Some(px) = price {
            last.price = px;
        }
        engine.on_event(&last)
    }

    fn arm(engine: &mut StrategyV022Engine) {
        for round in 0..8 {
            if round_of_sells(engine, &format!("arm-{round}"), None, 0.6) == Some("BUY") {
                return;
            }
        }
        panic!("arm did not reach a buy");
    }

    #[test]
    fn skips_first_qualifying_round_and_buys_second() {
        let mut engine = bound(|_| {}, "");
        assert_eq!(engine.phase_name(), "seek_sell");
        assert!(engine.needs_pool_tape());
        assert_eq!(engine.on_event(&print("SELL", 0.1, "dust")), None);
        assert_eq!(engine.on_event(&print("SELL", 0.4, "s1")), None);
        assert_eq!(engine.on_event(&print("BUY", 2.0, "break")), None);
        assert_eq!(engine.on_event(&print("SELL", 0.4, "s2")), None);
        assert_eq!(engine.on_event(&print("SELL", 0.3, "s3")), None);
        assert_eq!(engine.on_event(&print("SELL", 0.3, "s4")), None);
        assert_eq!(engine.on_event(&print("SELL", 0.3, "s5")), None);
        assert_eq!(engine.phase_name(), "seek_sell");
        assert_eq!(engine.on_event(&print("SELL", 0.4, "s6")), None);
        assert_eq!(engine.on_event(&print("SELL", 0.3, "s7")), None);
        assert_eq!(engine.on_event(&print("SELL", 0.3, "s8")), None);
        assert_eq!(engine.on_event(&print("SELL", 0.3, "s9")), Some("BUY"));
        assert_eq!(engine.phase_name(), "pending");
        assert!(engine.last_buy_reason().contains("buy_hit"));
        assert_eq!(engine.last_buy_diag().buy_hit_sol, 1.3);
    }

    #[test]
    fn waits_when_buy_round_above_mcap_cap() {
        let mut engine = bound(|_| {}, "");
        round_of_sells(&mut engine, "early", Some(100e-9), 0.6);
        assert_eq!(engine.phase_name(), "seek_sell");
        assert_eq!(round_of_sells(&mut engine, "cap", Some(100e-9), 0.6), None);
        assert_eq!(engine.phase_name(), "wait");
        let mut again = print("BUY", 1.0, "again");
        again.wallet = "target".to_string();
        assert_eq!(engine.on_event(&again), None);
        assert_eq!(engine.phase_name(), "wait");
        let mut flat = print("SELL", 1.0, "flat");
        flat.wallet = "target".to_string();
        flat.token_amount = 1.0;
        assert_eq!(engine.on_event(&flat), None);
        assert_eq!(engine.phase_name(), "idle");
        let mut next = print("BUY", 1.0, "next");
        next.wallet = "target".to_string();
        next.slot = 2;
        assert_eq!(engine.on_event(&next), None);
        assert_eq!(engine.phase_name(), "seek_sell");
    }

    #[test]
    fn sells_hold_on_cluster_tp_stop_or_target() {
        let mut cluster = bound(|_| {}, "");
        arm(&mut cluster);
        cluster.on_buy_fill(0.0);
        assert_eq!(cluster.phase_name(), "hold");
        cluster.on_event(&print("BUY", 2.0, "b1"));
        cluster.on_event(&print("BUY", 2.0, "b2"));
        assert_eq!(cluster.on_event(&print("BUY", 2.0, "b3")), Some("SELL"));
        assert!(cluster.last_sell_reason().contains("sell_hit"));
        assert_eq!(cluster.phase_name(), "wait");

        let mut tp = bound(|_| {}, "");
        arm(&mut tp);
        let fill = 50e-9 * 1.1;
        tp.on_buy_fill(fill);
        let mut under = print("SELL", 0.05, "under-fill");
        under.price = 50e-9 * 1.2;
        assert_eq!(tp.on_event(&under), None);
        let mut took = print("SELL", 0.05, "tp");
        took.price = fill * 1.2;
        assert_eq!(tp.on_event(&took), Some("SELL"));
        assert_eq!(tp.last_sell_reason(), "take_profit");

        let mut stop = bound(
            |cfg| {
                cfg.stop_loss = 0.1;
                cfg.take_profit = 0.0;
            },
            "",
        );
        arm(&mut stop);
        stop.on_buy_fill(0.0);
        let mut cut = print("SELL", 0.05, "sl");
        cut.price = 50e-9 * 0.8;
        assert_eq!(stop.on_event(&cut), Some("SELL"));
        assert_eq!(stop.last_sell_reason(), "stop");

        let mut target = bound(|cfg| cfg.take_profit = 0.0, "");
        arm(&mut target);
        target.on_buy_fill(0.0);
        let mut dumped = print("SELL", 0.05, "ts");
        dumped.wallet = "target".to_string();
        dumped.token_amount = 1.0;
        assert_eq!(target.on_event(&dumped), Some("SELL"));
        assert_eq!(target.last_sell_reason(), "target_sell");
        assert_eq!(target.phase_name(), "idle");
    }

    #[test]
    fn retries_failed_sell_after_cooldown() {
        let t0 = 1_700_000_000_000u64;
        let mut cluster = bound(|cfg| cfg.take_profit = 0.0, "");
        arm(&mut cluster);
        cluster.on_buy_fill(0.0);
        let mut b1 = print("BUY", 2.0, "b1");
        b1.timestamp_ms = t0;
        cluster.on_event(&b1);
        let mut b2 = print("BUY", 2.0, "b2");
        b2.timestamp_ms = t0;
        cluster.on_event(&b2);
        let mut b3 = print("BUY", 2.0, "b3");
        b3.timestamp_ms = t0;
        assert_eq!(cluster.on_event(&b3), Some("SELL"));
        assert_eq!(cluster.phase_name(), "wait");
        cluster.on_sell_failed(t0);
        assert_eq!(cluster.phase_name(), "hold");
        assert!(cluster.needs_pool_tape());
        let mut during = print("BUY", 2.0, "during");
        during.timestamp_ms = t0 + 1_000;
        assert_eq!(cluster.on_event(&during), None);
        assert_eq!(cluster.on_clock(t0 + 1_000), None);
        assert_eq!(cluster.phase_name(), "hold");
        assert_eq!(cluster.on_clock(t0 + 2_000), Some("SELL"));
        assert_eq!(cluster.phase_name(), "wait");
        assert!(cluster.last_sell_reason().contains("sell_hit"));
    }

    #[test]
    fn drops_short_sell_round_and_buys_second_full_round() {
        let mut engine = bound(|_| {}, "");
        assert_eq!(engine.on_event(&print("SELL", 0.2, "short-1")), None);
        assert_eq!(engine.on_event(&print("SELL", 0.2, "short-2")), None);
        assert_eq!(engine.on_event(&print("SELL", 0.2, "short-3")), None);
        assert_eq!(engine.on_event(&print("SELL", 0.2, "short-4")), None);
        assert_eq!(engine.on_event(&print("SELL", 0.4, "not-slid")), None);
        assert_eq!(engine.phase_name(), "seek_sell");
        assert_eq!(engine.on_event(&print("SELL", 0.4, "q1-2")), None);
        assert_eq!(engine.on_event(&print("SELL", 0.4, "q1-3")), None);
        assert_eq!(engine.on_event(&print("SELL", 0.4, "q1-4")), None);
        assert_eq!(round_of_sells(&mut engine, "q2", None, 0.4), Some("BUY"));
        assert_eq!(engine.last_buy_diag().buy_hit_sol, 1.6);
        assert_eq!(engine.phase_name(), "pending");
    }

    #[test]
    fn treats_zero_hit_count_as_window_of_one() {
        // TS overrides hit counts after load (bypassing clamps); engine treats 0 as 1.
        let mut raw = StrategyV022Config::default_v022();
        raw.buy_hit_count = 0;
        raw.sell_hit_count = 0;
        raw.buy_hit_min = 0.5;
        raw.sell_hit_min = 0.5;
        raw.buy_hit_round = 2;
        let mut engine = StrategyV022Engine::new(raw, "");
        engine.bind_gate("target", 0.0);
        assert_eq!(engine.on_event(&print("SELL", 0.6, "s1")), None);
        assert_eq!(engine.on_event(&print("SELL", 0.6, "s2")), Some("BUY"));
        engine.on_buy_fill(0.0);
        assert_eq!(engine.on_event(&print("BUY", 0.6, "b1")), Some("SELL"));
    }

    #[test]
    fn leaves_own_prints_out_of_runs() {
        let mut engine = bound(|cfg| cfg.buy_hit_round = 1, "me");
        engine.on_event(&print("SELL", 0.6, "s1"));
        let mut mine = print("BUY", 3.0, "my-buy");
        mine.wallet = "me".to_string();
        assert_eq!(engine.on_event(&mine), None);
        engine.on_event(&print("SELL", 0.6, "s2"));
        let mut my_sell = print("SELL", 2.0, "my-sell");
        my_sell.wallet = "me".to_string();
        assert_eq!(engine.on_event(&my_sell), None);
        engine.on_event(&print("SELL", 0.6, "s3"));
        assert_eq!(engine.on_event(&print("SELL", 0.6, "s4")), Some("BUY"));
        engine.on_buy_fill(0.0);
        engine.on_event(&print("BUY", 2.0, "b1"));
        let mut my_exit = print("BUY", 3.0, "my-exit-buy");
        my_exit.wallet = "me".to_string();
        assert_eq!(engine.on_event(&my_exit), None);
        engine.on_event(&print("BUY", 2.0, "b2"));
        assert_eq!(engine.on_event(&print("BUY", 2.0, "b3")), Some("SELL"));
        assert!(engine.last_sell_reason().contains("sell_hit"));
    }
}
