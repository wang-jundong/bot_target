use crate::bindings as settings;
use crate::events::sol_to_lamports;
use crate::security::decrypt_trading_private_key;
use anyhow::{Context, Result, bail};
use solana_sdk::signature::Keypair;
use solana_sdk::signer::Signer;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StrategyName {
    V011,
    V022,
    V031,
    V051,
    V061,
    V071,
    V081,
    V101,
    V121,
    V131,
}

impl StrategyName {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::V011 => "strategy_v_011",
            Self::V022 => "strategy_v_022",
            Self::V031 => "strategy_v_031",
            Self::V051 => "strategy_v_051",
            Self::V061 => "strategy_v_061",
            Self::V071 => "strategy_v_071",
            Self::V081 => "strategy_v_081",
            Self::V101 => "strategy_v_101",
            Self::V121 => "strategy_v_121",
            Self::V131 => "strategy_v_131",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            "strategy_v_011" => Some(Self::V011),
            "strategy_v_022" => Some(Self::V022),
            "strategy_v_031" => Some(Self::V031),
            "strategy_v_051" => Some(Self::V051),
            "strategy_v_061" => Some(Self::V061),
            "strategy_v_071" => Some(Self::V071),
            "strategy_v_081" => Some(Self::V081),
            "strategy_v_101" => Some(Self::V101),
            "strategy_v_121" => Some(Self::V121),
            "strategy_v_131" => Some(Self::V131),
            _ => None,
        }
    }
}

/// Which quote pairs a strategy is allowed to open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotePairs {
    Sol,
    NonSol,
    Both,
}

impl QuotePairs {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sol => "sol",
            Self::NonSol => "non_sol",
            Self::Both => "both",
        }
    }

    /// Empty and `sol` keep the strategy on SOL pairs.
    pub fn parse(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "" | "sol" => Ok(Self::Sol),
            "non_sol" => Ok(Self::NonSol),
            "both" => Ok(Self::Both),
            other => bail!("quote pairs must be sol, non_sol, or both (got {other})"),
        }
    }

    pub fn allows(self, sol_pair: bool) -> bool {
        match self {
            Self::Sol => sol_pair,
            Self::NonSol => !sol_pair,
            Self::Both => true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct StrategyV011Config {
    pub gate_wallet: String,
    pub timer_ms: u64,
    pub min_entry_s: f64,
    pub max_entry_s: f64,
    pub max_chase: f64,
    pub take_profit: f64,
    pub dust_sol: f64,
    pub sell_hit_count: u32,
    pub sell_hit_min: f64,
    pub target_sell_exit: bool,
    pub size_sol: f64,
}

impl StrategyV011Config {
    /// Same knobs as `bot_target` `src/config/strategyV011.ts`.
    pub fn default_v011() -> Self {
        let mut cfg = Self {
            gate_wallet: "4DdrfiDHpmx55i4SPssxVzS9ZaKLb8qr45NKY9Er9nNh".to_string(),
            timer_ms: 200,
            min_entry_s: 4.5,
            max_entry_s: 12.0,
            max_chase: 0.20,
            take_profit: 0.10,
            dust_sol: 0.1,
            sell_hit_count: 4,
            sell_hit_min: 2.5,
            target_sell_exit: true,
            size_sol: 0.4,
        };
        if cfg.sell_hit_count < 1 {
            cfg.sell_hit_count = 1;
        }
        cfg
    }
}

#[derive(Debug, Clone)]
pub struct StrategyV022Config {
    pub gate_wallet: String,
    pub dust_sol: f64,
    pub buy_hit_count: u32,
    pub buy_hit_min: f64,
    pub buy_hit_round: u32,
    pub sell_hit_count: u32,
    pub sell_hit_min: f64,
    pub max_mc_sol: f64,
    pub stop_loss: f64,
    pub take_profit: f64,
    pub target_sell_exit: bool,
    pub size_sol: f64,
}

impl StrategyV022Config {
    /// Same knobs as `bot_target` `src/config/strategyV022.ts`.
    pub fn default_v022() -> Self {
        let mut cfg = Self {
            gate_wallet: "FqamE7xrahg7FEWoByrx1o8SeyHt44rpmE6ZQfT7zrve".to_string(),
            dust_sol: 0.1,
            buy_hit_count: 4,
            buy_hit_min: 1.0,
            buy_hit_round: 2,
            sell_hit_count: 3,
            sell_hit_min: 6.0,
            max_mc_sol: 90.0,
            stop_loss: 0.0,
            take_profit: 0.15,
            target_sell_exit: true,
            size_sol: 0.5,
        };
        if cfg.buy_hit_count < 1 {
            cfg.buy_hit_count = 1;
        }
        if cfg.buy_hit_round < 1 {
            cfg.buy_hit_round = 1;
        }
        if cfg.sell_hit_count < 1 {
            cfg.sell_hit_count = 1;
        }
        cfg
    }
}

#[derive(Debug, Clone)]
pub struct StrategyV031Config {
    pub gate_wallet: String,
    pub dust_sol: f64,
    pub min_entry_s: f64,
    pub max_entry_s: f64,
    pub buy_hit_count: u32,
    pub buy_hit_min: f64,
    pub min_mc_sol: f64,
    pub max_mc_sol: f64,
    pub min_largest_buy_sol: f64,
    pub max_largest_buy_sol: f64,
    pub min_price_ratio: f64,
    pub max_price_ratio: f64,
    pub round_trip_window_s: f64,
    pub min_round_trip_holders: u32,
    pub max_round_trip_holders: u32,
    pub sell_hit_count: u32,
    pub sell_hit_min: f64,
    pub take_profit: f64,
    pub max_hold_s: f64,
    pub target_sell_exit: bool,
    pub first_cycle_only: bool,
    pub repeat_entries: bool,
    pub size_sol: f64,
}

impl StrategyV031Config {
    /// Knobs from scalpingbot `config/strategy_v_031.json` (2026-10-22).
    /// `max_mc_sol` is 0, so only the 40 SOL floor applies. The live book
    /// blocks an entry once another wallet has bought more than 5 SOL.
    /// Zero `min_mc_sol`, `max_mc_sol`, `min_largest_buy_sol`,
    /// `max_largest_buy_sol`, `min_price_ratio`, or `max_price_ratio` disables
    /// that bound. A zero `round_trip_window_s`, or both holder bounds at 0,
    /// turns the holder sell off. `size_sol` stays at the live position size,
    /// not the backtest size.
    pub fn default_v031() -> Self {
        let mut cfg = Self {
            gate_wallet: "3VUNtVtjjx5ckUojT7UocJ5fbuAJRsNUXNfTBnPte9vC".to_string(),
            dust_sol: 0.1,
            min_entry_s: 4.0,
            max_entry_s: 16.0,
            buy_hit_count: 4,
            buy_hit_min: 1.0,
            min_mc_sol: 40.0,
            max_mc_sol: 0.0,
            min_largest_buy_sol: 0.0,
            max_largest_buy_sol: 5.0,
            min_price_ratio: 0.0,
            max_price_ratio: 0.0,
            round_trip_window_s: 0.0,
            min_round_trip_holders: 0,
            max_round_trip_holders: 0,
            sell_hit_count: 5,
            sell_hit_min: 4.0,
            take_profit: 0.4,
            max_hold_s: 45.0,
            target_sell_exit: true,
            first_cycle_only: false,
            repeat_entries: false,
            size_sol: 1.0,
        };
        if cfg.buy_hit_count < 1 {
            cfg.buy_hit_count = 1;
        }
        if cfg.sell_hit_count < 1 {
            cfg.sell_hit_count = 1;
        }
        if cfg.round_trip_window_s <= 0.0
            || (cfg.min_round_trip_holders == 0 && cfg.max_round_trip_holders == 0)
        {
            cfg.round_trip_window_s = 0.0;
            cfg.min_round_trip_holders = 0;
            cfg.max_round_trip_holders = 0;
        }
        cfg
    }
}

#[derive(Debug, Clone)]
pub struct StrategyV051Config {
    pub gate_wallet: String,
    pub dust_sol: f64,
    pub min_entry_s: f64,
    pub max_entry_s: f64,
    pub buy_hit_count: u32,
    pub buy_hit_min: f64,
    pub min_mc_sol: f64,
    pub max_mc_sol: f64,
    pub sell_hit_count: u32,
    pub sell_hit_min: f64,
    pub take_profit: f64,
    pub max_hold_s: f64,
    pub target_sell_exit: bool,
    pub first_cycle_only: bool,
    pub repeat_entries: bool,
    pub size_sol: f64,
}

impl StrategyV051Config {
    /// Knobs from scalpingbot `config/strategy_v_051.json` (2026-10-21 walk-forward).
    /// Zero `min_entry_s`, `max_entry_s`, `min_mc_sol`, `max_mc_sol`, and
    /// `max_hold_s` disable those bounds. `size_sol` is the live position size.
    pub fn default_v051() -> Self {
        let mut cfg = Self {
            gate_wallet: "HSeCG7T2KCTuVAARGXZuxBNZg6pas1EdJdjn7Xyy1ENB".to_string(),
            dust_sol: 0.3,
            min_entry_s: 0.0,
            max_entry_s: 8.0,
            buy_hit_count: 3,
            buy_hit_min: 1.0,
            min_mc_sol: 40.0,
            max_mc_sol: 100.0,
            sell_hit_count: 5,
            sell_hit_min: 4.0,
            take_profit: 0.3,
            max_hold_s: 30.0,
            target_sell_exit: true,
            first_cycle_only: true,
            repeat_entries: true,
            size_sol: 1.0,
        };
        if cfg.buy_hit_count < 1 {
            cfg.buy_hit_count = 1;
        }
        if cfg.sell_hit_count < 1 {
            cfg.sell_hit_count = 1;
        }
        cfg
    }
}

#[derive(Debug, Clone)]
pub struct StrategyV061Config {
    pub gate_wallet: String,
    pub dust_sol: f64,
    pub min_entry_s: f64,
    pub max_entry_s: f64,
    pub max_chase: f64,
    pub min_mc_sol: f64,
    pub max_mc_sol: f64,
    pub sell_hit_count: u32,
    pub sell_hit_min: f64,
    pub take_profit: f64,
    pub max_hold_s: f64,
    pub target_sell_exit: bool,
    pub second_buy_exit: bool,
    pub first_cycle_only: bool,
    pub repeat_entries: bool,
    pub size_sol: f64,
}

impl StrategyV061Config {
    /// Knobs from scalpingbot `config/strategy_v_061.json` (2026-10-19).
    /// Zero `min_entry_s`, `max_entry_s`, `max_chase`, `min_mc_sol`,
    /// `max_mc_sol`, and `max_hold_s` disable those bounds. `size_sol` matches
    /// that config.
    pub fn default_v061() -> Self {
        let mut cfg = Self {
            gate_wallet: "6qudAN2kV8mtCcYJxb5QQ6Vr15itdHHdeVbYm99NKMhy".to_string(),
            dust_sol: 0.3,
            min_entry_s: 6.0,
            max_entry_s: 8.0,
            max_chase: 0.3,
            min_mc_sol: 0.0,
            max_mc_sol: 220.0,
            sell_hit_count: 5,
            sell_hit_min: 6.0,
            take_profit: 0.4,
            max_hold_s: 30.0,
            target_sell_exit: true,
            second_buy_exit: false,
            first_cycle_only: false,
            repeat_entries: false,
            size_sol: 0.3,
        };
        if cfg.sell_hit_count < 1 {
            cfg.sell_hit_count = 1;
        }
        cfg
    }
}

#[derive(Debug, Clone)]
pub struct StrategyV071Config {
    pub gate_wallet: String,
    pub dust_sol: f64,
    pub min_entry_s: f64,
    pub max_entry_s: f64,
    pub min_mc_sol: f64,
    pub max_mc_sol: f64,
    pub sell_hit_count: u32,
    pub sell_hit_min: f64,
    pub take_profit: f64,
    pub max_hold_s: f64,
    pub target_sell_exit: bool,
    pub second_buy_exit: bool,
    pub first_cycle_only: bool,
    pub repeat_entries: bool,
    pub size_sol: f64,
}

impl StrategyV071Config {
    /// Knobs from scalpingbot `config/strategy_v_071.json` (2026-10-26 update).
    /// The entry clock is 3s through 90s. `min_mc_sol` is 0, so only the 70 SOL
    /// cap applies. Zero `min_entry_s`, `max_entry_s`, `min_mc_sol`, `max_mc_sol`,
    /// and `max_hold_s` disable those bounds. `size_sol` is the live position size.
    pub fn default_v071() -> Self {
        let mut cfg = Self {
            gate_wallet: "3PwXa9BRA2sUdjtcGxs4bVLBau2dHTuZwtb6CbpTpdvA".to_string(),
            dust_sol: 0.8,
            min_entry_s: 3.0,
            max_entry_s: 90.0,
            min_mc_sol: 0.0,
            max_mc_sol: 70.0,
            sell_hit_count: 5,
            sell_hit_min: 8.0,
            take_profit: 1.2,
            max_hold_s: 0.0,
            target_sell_exit: true,
            second_buy_exit: false,
            first_cycle_only: false,
            repeat_entries: true,
            size_sol: 0.5,
        };
        if cfg.sell_hit_count < 1 {
            cfg.sell_hit_count = 1;
        }
        cfg
    }
}

#[derive(Debug, Clone)]
pub struct StrategyV081Config {
    pub gate_wallet: String,
    pub dust_sol: f64,
    pub min_entry_s: f64,
    pub max_entry_s: f64,
    pub min_price_ratio: f64,
    pub max_price_ratio: f64,
    pub buy_hit_count: u32,
    pub buy_hit_min: f64,
    pub min_mc_sol: f64,
    pub max_mc_sol: f64,
    pub min_largest_buy_sol: f64,
    pub max_largest_buy_sol: f64,
    pub sell_hit_count: u32,
    pub sell_hit_min: f64,
    pub take_profit: f64,
    pub max_hold_s: f64,
    pub hold_sell_s: f64,
    pub min_largest_sell_sol: f64,
    pub max_largest_sell_sol: f64,
    pub round_trip_s: f64,
    pub min_round_trip_holders: u32,
    pub max_round_trip_holders: u32,
    pub target_sell_exit: bool,
    pub second_buy_exit: bool,
    pub first_cycle_only: bool,
    pub repeat_entries: bool,
    pub max_trades_per_mint: u32,
    pub size_sol: f64,
}

impl StrategyV081Config {
    /// Knobs from scalpingbot `config/strategy_v_081.json` (2026-10-22 update).
    /// `buy_hit_count` of 0 is clock entry. Zero `min_entry_s`, `max_entry_s`,
    /// `min_mc_sol`, `max_mc_sol`, `min_largest_buy_sol`, `max_largest_buy_sol`,
    /// `min_price_ratio`, `max_price_ratio`, `hold_sell_s`, `min_largest_sell_sol`,
    /// `max_largest_sell_sol`, `round_trip_s`, `min_round_trip_holders`,
    /// `max_round_trip_holders`, `max_trades_per_mint`, and `max_hold_s` disable
    /// those bounds. `hold_sell_s` is 0, so the stored largest-sell bounds are
    /// off. `size_sol` matches that config.
    pub fn default_v081() -> Self {
        let mut cfg = Self {
            gate_wallet: "ardinRsN1mNYVeoJWTBsWeYeXvuR9UUDGMsCDKpb6AT".to_string(),
            dust_sol: 0.3,
            min_entry_s: 0.5,
            max_entry_s: 8.0,
            min_price_ratio: 1.0,
            max_price_ratio: 1.5,
            buy_hit_count: 0,
            buy_hit_min: 0.0,
            min_mc_sol: 0.0,
            max_mc_sol: 80.0,
            min_largest_buy_sol: 0.0,
            max_largest_buy_sol: 6.0,
            sell_hit_count: 3,
            sell_hit_min: 5.0,
            take_profit: 0.4,
            max_hold_s: 30.0,
            hold_sell_s: 0.0,
            min_largest_sell_sol: 2.0,
            max_largest_sell_sol: 5.0,
            round_trip_s: 10.0,
            min_round_trip_holders: 15,
            max_round_trip_holders: 0,
            target_sell_exit: true,
            second_buy_exit: false,
            first_cycle_only: false,
            repeat_entries: true,
            max_trades_per_mint: 1,
            size_sol: 0.3,
        };
        if cfg.sell_hit_count < 1 {
            cfg.sell_hit_count = 1;
        }
        if cfg.round_trip_s <= 0.0
            || (cfg.min_round_trip_holders == 0 && cfg.max_round_trip_holders == 0)
        {
            cfg.round_trip_s = 0.0;
            cfg.min_round_trip_holders = 0;
            cfg.max_round_trip_holders = 0;
        }
        cfg
    }
}

#[derive(Debug, Clone)]
pub struct StrategyV101Config {
    pub gate_wallet: String,
    pub dust_sol: f64,
    pub min_entry_s: f64,
    pub max_entry_s: f64,
    pub min_price_ratio: f64,
    pub max_price_ratio: f64,
    pub buy_hit_count: u32,
    pub buy_hit_min: f64,
    pub min_mc_sol: f64,
    pub max_mc_sol: f64,
    pub min_largest_buy_sol: f64,
    pub max_largest_buy_sol: f64,
    pub sell_hit_count: u32,
    pub sell_hit_min: f64,
    pub take_profit: f64,
    pub max_hold_s: f64,
    pub target_sell_exit: bool,
    pub second_buy_exit: bool,
    pub first_cycle_only: bool,
    pub repeat_entries: bool,
    pub size_sol: f64,
}

#[derive(Debug, Clone)]
pub struct StrategyV121Config {
    pub gate_wallet: String,
    pub dust_sol: f64,
    pub min_entry_s: f64,
    pub max_entry_s: f64,
    pub buy_hit_count: u32,
    pub buy_hit_min: f64,
    pub min_mc_sol: f64,
    pub max_mc_sol: f64,
    pub min_largest_buy_sol: f64,
    pub max_largest_buy_sol: f64,
    pub min_price_ratio: f64,
    pub max_price_ratio: f64,
    pub min_buy_round_trip_holders: u32,
    pub max_buy_round_trip_holders: u32,
    pub sell_hit_count: u32,
    pub sell_hit_min: f64,
    pub take_profit: f64,
    pub max_hold_s: f64,
    pub target_sell_exit: bool,
    pub second_buy_exit: bool,
    pub first_cycle_only: bool,
    pub repeat_entries: bool,
    pub size_sol: f64,
}

impl StrategyV121Config {
    /// Knobs from scalpingbot `config/strategy_v_121.json` (2026-10-24).
    /// `buy_hit_count` of 0 buys the first print inside the clock. Zero
    /// `min_entry_s`, `max_entry_s`, `min_price_ratio`, `max_price_ratio`,
    /// `min_mc_sol`, `max_mc_sol`, `min_largest_buy_sol`, `max_largest_buy_sol`,
    /// `min_buy_round_trip_holders`, `max_buy_round_trip_holders`, and
    /// `max_hold_s` disable those bounds. `size_sol` matches that config.
    pub fn default_v121() -> Self {
        let mut cfg = Self {
            gate_wallet: "CyaE1VxvBrahnPWkqm5VsdCvyS2QmNht2UFrKJHga54o".to_string(),
            dust_sol: 0.2,
            min_entry_s: 7.0,
            max_entry_s: 20.0,
            buy_hit_count: 3,
            buy_hit_min: 1.5,
            min_mc_sol: 0.0,
            max_mc_sol: 80.0,
            min_largest_buy_sol: 4.0,
            max_largest_buy_sol: 0.0,
            min_price_ratio: 1.0,
            max_price_ratio: 2.0,
            min_buy_round_trip_holders: 10,
            max_buy_round_trip_holders: 30,
            sell_hit_count: 3,
            sell_hit_min: 5.0,
            take_profit: 0.15,
            max_hold_s: 30.0,
            target_sell_exit: true,
            second_buy_exit: false,
            first_cycle_only: false,
            repeat_entries: false,
            size_sol: 0.3,
        };
        if cfg.sell_hit_count < 1 {
            cfg.sell_hit_count = 1;
        }
        if cfg.min_largest_buy_sol < 0.0 {
            cfg.min_largest_buy_sol = 0.0;
        }
        if cfg.max_largest_buy_sol < 0.0 {
            cfg.max_largest_buy_sol = 0.0;
        }
        if cfg.min_price_ratio < 0.0 {
            cfg.min_price_ratio = 0.0;
        }
        if cfg.max_price_ratio < 0.0 {
            cfg.max_price_ratio = 0.0;
        }
        cfg
    }
}

#[derive(Debug, Clone)]
pub struct StrategyV131Config {
    pub gate_wallet: String,
    pub dust_sol: f64,
    pub min_entry_s: f64,
    pub max_entry_s: f64,
    pub buy_hit_count: u32,
    pub buy_hit_min: f64,
    pub min_mc_sol: f64,
    pub max_mc_sol: f64,
    pub min_largest_buy_sol: f64,
    pub max_largest_buy_sol: f64,
    pub min_price_ratio: f64,
    pub max_price_ratio: f64,
    pub min_buy_round_trip_holders: u32,
    pub max_buy_round_trip_holders: u32,
    pub sell_hit_count: u32,
    pub sell_hit_min: f64,
    pub take_profit: f64,
    pub max_hold_s: f64,
    pub target_sell_exit: bool,
    pub second_buy_exit: bool,
    pub first_cycle_only: bool,
    pub repeat_entries: bool,
    pub size_sol: f64,
}

impl StrategyV131Config {
    /// Knobs from scalpingbot `config/strategy_v_131.json` (2026-10-26).
    /// `buy_hit_count` of 0 buys the first print inside the clock. Zero
    /// `min_entry_s`, `max_entry_s`, `min_price_ratio`, `max_price_ratio`,
    /// `min_mc_sol`, `max_mc_sol`, `min_largest_buy_sol`, `max_largest_buy_sol`,
    /// `min_buy_round_trip_holders`, `max_buy_round_trip_holders`, and
    /// `max_hold_s` disable those bounds. `size_sol` matches that config.
    pub fn default_v131() -> Self {
        let mut cfg = Self {
            gate_wallet: "4vw54BmAogeRV3vPKWyFet5yf8DTLcREzdSzx4rw9Ud9".to_string(),
            dust_sol: 0.1,
            min_entry_s: 0.0,
            max_entry_s: 12.0,
            buy_hit_count: 2,
            buy_hit_min: 0.5,
            min_mc_sol: 0.0,
            max_mc_sol: 90.0,
            min_largest_buy_sol: 1.0,
            max_largest_buy_sol: 12.0,
            min_price_ratio: 0.7,
            max_price_ratio: 0.0,
            min_buy_round_trip_holders: 0,
            max_buy_round_trip_holders: 0,
            sell_hit_count: 2,
            sell_hit_min: 5.0,
            take_profit: 0.25,
            max_hold_s: 60.0,
            target_sell_exit: true,
            second_buy_exit: false,
            first_cycle_only: false,
            repeat_entries: false,
            size_sol: 0.3,
        };
        if cfg.sell_hit_count < 1 {
            cfg.sell_hit_count = 1;
        }
        if cfg.min_largest_buy_sol < 0.0 {
            cfg.min_largest_buy_sol = 0.0;
        }
        if cfg.max_largest_buy_sol < 0.0 {
            cfg.max_largest_buy_sol = 0.0;
        }
        if cfg.min_price_ratio < 0.0 {
            cfg.min_price_ratio = 0.0;
        }
        if cfg.max_price_ratio < 0.0 {
            cfg.max_price_ratio = 0.0;
        }
        cfg
    }
}

impl StrategyV101Config {
    /// Knobs from scalpingbot `config/strategy_v_101.json` (2026-10-23).
    /// `buy_hit_count` of 0 buys the first print inside the clock. Zero
    /// `min_entry_s`, `max_entry_s`, `min_price_ratio`, `max_price_ratio`,
    /// `min_mc_sol`, `max_mc_sol`, `min_largest_buy_sol`, `max_largest_buy_sol`,
    /// and `max_hold_s` disable those bounds. `size_sol` matches that config.
    pub fn default_v101() -> Self {
        let mut cfg = Self {
            gate_wallet: "BNnN2MqfWLvgThYBsv6v8JQaYZXYKYahC5YCy27Ct1cX".to_string(),
            dust_sol: 0.2,
            min_entry_s: 2.0,
            max_entry_s: 60.0,
            min_price_ratio: 1.1,
            max_price_ratio: 1.4,
            buy_hit_count: 0,
            buy_hit_min: 0.5,
            min_mc_sol: 0.0,
            max_mc_sol: 0.0,
            min_largest_buy_sol: 0.0,
            max_largest_buy_sol: 8.0,
            sell_hit_count: 2,
            sell_hit_min: 6.0,
            take_profit: 2.0,
            max_hold_s: 0.0,
            target_sell_exit: true,
            second_buy_exit: false,
            first_cycle_only: true,
            repeat_entries: true,
            size_sol: 0.3,
        };
        if cfg.sell_hit_count < 1 {
            cfg.sell_hit_count = 1;
        }
        cfg
    }
}

pub struct StrategyPlan {
    pub name: StrategyName,
    pub target_wallet: String,
    pub buy_amount_lamports: u64,
    pub max_entry_market_cap_sol: f64,
    pub keypair: Keypair,
    pub target_sell_exit: bool,
    pub first_cycle_only: bool,
    pub quote_pairs: QuotePairs,
}

impl StrategyPlan {
    pub fn pubkey_b58(&self) -> String {
        self.keypair.pubkey().to_string()
    }
}

pub struct AppConfig {
    pub strategy_label: String,
    pub strategy_plans: Vec<StrategyPlan>,
    pub strategy_v011: StrategyV011Config,
    pub strategy_v022: StrategyV022Config,
    pub strategy_v031: StrategyV031Config,
    pub strategy_v051: StrategyV051Config,
    pub strategy_v061: StrategyV061Config,
    pub strategy_v071: StrategyV071Config,
    pub strategy_v081: StrategyV081Config,
    pub strategy_v101: StrategyV101Config,
    pub strategy_v121: StrategyV121Config,
    pub strategy_v131: StrategyV131Config,
    pub blocked_mints: String,
    pub vibe_grpc_endpoint: String,
    pub vibe_grpc_token: String,
    pub helius_rpc_url: String,
    pub helius_sender_url: String,
    pub helius_sender_swqos_only: bool,
    pub buy_slippage_bps: u64,
    pub sell_slippage_bps: u64,
    pub priority_fee_lamports: u64,
    pub helius_tip_lamports: u64,
    pub compute_unit_limit: u32,
    pub event_retention_sec: f64,
    pub blockhash_refresh_ms: u64,
    pub log_level: String,
    pub recovery_path: PathBuf,
    pub pnl_path: PathBuf,
}

/// Load settings from `src/bindings.rs` (gitignored).
pub fn load_config() -> Result<AppConfig> {
    if settings::EXECUTION_MODE != "live" {
        bail!("EXECUTION_MODE must be live");
    }
    let active = selected_strategy_names()?;
    if active.is_empty() {
        bail!("ACTIVE_STRATEGIES must list at least one strategy");
    }
    let vibe_endpoint = require_str("VIBE_GRPC_ENDPOINT", settings::VIBE_GRPC_ENDPOINT)?;
    let vibe_token = require_str("VIBE_GRPC_TOKEN", settings::VIBE_GRPC_TOKEN)?;
    let api_key = require_str("HELIUS_API_KEY", settings::HELIUS_API_KEY)?;
    let rpc = require_str("HELIUS_RPC_URL", settings::HELIUS_RPC_URL)?;
    let sender = require_str("HELIUS_SENDER_URL", settings::HELIUS_SENDER_URL)?;
    let swqos = settings::HELIUS_SENDER_SWQOS_ONLY;
    if settings::PRIORITY_FEE_LAMPORTS < 1 {
        bail!("PRIORITY_FEE_LAMPORTS must be >= 1");
    }
    if !swqos && settings::HELIUS_TIP_LAMPORTS < 1_000_000 {
        bail!("Sender Max requires at least 1000000 tip lamports");
    }
    if settings::COMPUTE_UNIT_LIMIT < 1 {
        bail!("COMPUTE_UNIT_LIMIT must be >= 1");
    }
    if settings::BLOCKHASH_REFRESH_MS < 1000 {
        bail!("BLOCKHASH_REFRESH_MS must be >= 1000");
    }

    let strategy_v011 = StrategyV011Config::default_v011();
    let strategy_v022 = StrategyV022Config::default_v022();
    let strategy_v031 = StrategyV031Config::default_v031();
    let strategy_v051 = StrategyV051Config::default_v051();
    let strategy_v061 = StrategyV061Config::default_v061();
    let strategy_v071 = StrategyV071Config::default_v071();
    let strategy_v081 = StrategyV081Config::default_v081();
    let strategy_v101 = StrategyV101Config::default_v101();
    let strategy_v121 = StrategyV121Config::default_v121();
    let strategy_v131 = StrategyV131Config::default_v131();
    let mut strategy_plans = Vec::with_capacity(active.len());
    for name in &active {
        strategy_plans.push(build_plan(
            *name,
            &strategy_v011,
            &strategy_v022,
            &strategy_v031,
            &strategy_v051,
            &strategy_v061,
            &strategy_v071,
            &strategy_v081,
            &strategy_v101,
            &strategy_v121,
            &strategy_v131,
        )?);
    }
    let strategy_label = active
        .iter()
        .map(|n| n.as_str())
        .collect::<Vec<_>>()
        .join(", ");

    Ok(AppConfig {
        strategy_label,
        strategy_plans,
        strategy_v011,
        strategy_v022,
        strategy_v031,
        strategy_v051,
        strategy_v061,
        strategy_v071,
        strategy_v081,
        strategy_v101,
        strategy_v121,
        strategy_v131,
        blocked_mints: settings::BLOCKED_MINTS.to_string(),
        vibe_grpc_endpoint: vibe_endpoint,
        vibe_grpc_token: vibe_token,
        helius_rpc_url: authenticated_helius_rpc_url(&rpc, &api_key),
        helius_sender_url: sender,
        helius_sender_swqos_only: swqos,
        buy_slippage_bps: settings::BUY_SLIPPAGE_BPS,
        sell_slippage_bps: settings::SELL_SLIPPAGE_BPS,
        priority_fee_lamports: settings::PRIORITY_FEE_LAMPORTS,
        helius_tip_lamports: settings::HELIUS_TIP_LAMPORTS,
        compute_unit_limit: settings::COMPUTE_UNIT_LIMIT as u32,
        event_retention_sec: settings::EVENT_RETENTION_SEC,
        blockhash_refresh_ms: settings::BLOCKHASH_REFRESH_MS,
        log_level: settings::LOG_LEVEL.to_string(),
        recovery_path: PathBuf::from(settings::RECOVERY_PATH),
        pnl_path: PathBuf::from(settings::PNL_PATH),
    })
}

fn selected_strategy_names() -> Result<Vec<StrategyName>> {
    let mut names = Vec::new();
    for raw in settings::ACTIVE_STRATEGIES {
        let Some(name) = StrategyName::parse(raw) else {
            bail!("unknown strategy in ACTIVE_STRATEGIES: {raw}");
        };
        if names.contains(&name) {
            bail!("duplicate strategy in ACTIVE_STRATEGIES: {raw}");
        }
        names.push(name);
    }
    Ok(names)
}

fn build_plan(
    name: StrategyName,
    v011: &StrategyV011Config,
    v022: &StrategyV022Config,
    v031: &StrategyV031Config,
    v051: &StrategyV051Config,
    v061: &StrategyV061Config,
    v071: &StrategyV071Config,
    v081: &StrategyV081Config,
    v101: &StrategyV101Config,
    v121: &StrategyV121Config,
    v131: &StrategyV131Config,
) -> Result<StrategyPlan> {
    let (target_wallet, size_sol, max_mc, target_sell_exit, first_cycle_only) = match name {
        StrategyName::V011 => (
            v011.gate_wallet.clone(),
            v011.size_sol,
            0.0,
            v011.target_sell_exit,
            false,
        ),
        StrategyName::V022 => (
            v022.gate_wallet.clone(),
            v022.size_sol,
            v022.max_mc_sol,
            v022.target_sell_exit,
            false,
        ),
        StrategyName::V031 => (
            v031.gate_wallet.clone(),
            v031.size_sol,
            0.0,
            v031.target_sell_exit,
            v031.first_cycle_only,
        ),
        StrategyName::V051 => (
            v051.gate_wallet.clone(),
            v051.size_sol,
            0.0,
            v051.target_sell_exit,
            v051.first_cycle_only,
        ),
        StrategyName::V061 => (
            v061.gate_wallet.clone(),
            v061.size_sol,
            0.0,
            v061.target_sell_exit,
            v061.first_cycle_only,
        ),
        StrategyName::V071 => (
            v071.gate_wallet.clone(),
            v071.size_sol,
            0.0,
            v071.target_sell_exit,
            v071.first_cycle_only,
        ),
        StrategyName::V081 => (
            v081.gate_wallet.clone(),
            v081.size_sol,
            0.0,
            v081.target_sell_exit,
            v081.first_cycle_only,
        ),
        StrategyName::V101 => (
            v101.gate_wallet.clone(),
            v101.size_sol,
            0.0,
            v101.target_sell_exit,
            v101.first_cycle_only,
        ),
        StrategyName::V121 => (
            v121.gate_wallet.clone(),
            v121.size_sol,
            0.0,
            v121.target_sell_exit,
            v121.first_cycle_only,
        ),
        StrategyName::V131 => (
            v131.gate_wallet.clone(),
            v131.size_sol,
            0.0,
            v131.target_sell_exit,
            v131.first_cycle_only,
        ),
    };
    if target_wallet.trim().len() < 32 {
        bail!("{} gate_wallet is missing", name.as_str());
    }
    let buy_amount_lamports = sol_to_lamports(&format!("{size_sol:.9}"))?;
    if buy_amount_lamports == 0 {
        bail!("{} size must be positive", name.as_str());
    }
    if name == StrategyName::V022 && max_mc <= 0.0 {
        bail!("{} max market cap must be positive", name.as_str());
    }
    let quote_pairs = QuotePairs::parse(quote_pairs_setting(name))
        .with_context(|| format!("{} QUOTE_PAIRS", name.as_str()))?;
    Ok(StrategyPlan {
        name,
        target_wallet: target_wallet.trim().to_string(),
        buy_amount_lamports,
        max_entry_market_cap_sol: max_mc,
        keypair: resolve_trading_keypair(name)?,
        target_sell_exit,
        first_cycle_only,
        quote_pairs,
    })
}

fn quote_pairs_setting(name: StrategyName) -> &'static str {
    match name {
        StrategyName::V011 => settings::STRATEGY_V_011_QUOTE_PAIRS,
        StrategyName::V022 => settings::STRATEGY_V_022_QUOTE_PAIRS,
        StrategyName::V031 => settings::STRATEGY_V_031_QUOTE_PAIRS,
        StrategyName::V051 => settings::STRATEGY_V_051_QUOTE_PAIRS,
        StrategyName::V061 => settings::STRATEGY_V_061_QUOTE_PAIRS,
        StrategyName::V071 => settings::STRATEGY_V_071_QUOTE_PAIRS,
        StrategyName::V081 => settings::STRATEGY_V_081_QUOTE_PAIRS,
        StrategyName::V101 => settings::STRATEGY_V_101_QUOTE_PAIRS,
        StrategyName::V121 => settings::STRATEGY_V_121_QUOTE_PAIRS,
        StrategyName::V131 => settings::STRATEGY_V_131_QUOTE_PAIRS,
    }
}

fn resolve_trading_keypair(name: StrategyName) -> Result<Keypair> {
    let (encrypted, plain) = match name {
        StrategyName::V011 => (
            settings::STRATEGY_V_011_PRIVATE_KEY_ENCRYPTED,
            settings::STRATEGY_V_011_PRIVATE_KEY_BASE58,
        ),
        StrategyName::V022 => (
            settings::STRATEGY_V_022_PRIVATE_KEY_ENCRYPTED,
            settings::STRATEGY_V_022_PRIVATE_KEY_BASE58,
        ),
        StrategyName::V031 => (
            settings::STRATEGY_V_031_PRIVATE_KEY_ENCRYPTED,
            settings::STRATEGY_V_031_PRIVATE_KEY_BASE58,
        ),
        StrategyName::V051 => (
            settings::STRATEGY_V_051_PRIVATE_KEY_ENCRYPTED,
            settings::STRATEGY_V_051_PRIVATE_KEY_BASE58,
        ),
        StrategyName::V061 => (
            settings::STRATEGY_V_061_PRIVATE_KEY_ENCRYPTED,
            settings::STRATEGY_V_061_PRIVATE_KEY_BASE58,
        ),
        StrategyName::V071 => (
            settings::STRATEGY_V_071_PRIVATE_KEY_ENCRYPTED,
            settings::STRATEGY_V_071_PRIVATE_KEY_BASE58,
        ),
        StrategyName::V081 => (
            settings::STRATEGY_V_081_PRIVATE_KEY_ENCRYPTED,
            settings::STRATEGY_V_081_PRIVATE_KEY_BASE58,
        ),
        StrategyName::V101 => (
            settings::STRATEGY_V_101_PRIVATE_KEY_ENCRYPTED,
            settings::STRATEGY_V_101_PRIVATE_KEY_BASE58,
        ),
        StrategyName::V121 => (
            settings::STRATEGY_V_121_PRIVATE_KEY_ENCRYPTED,
            settings::STRATEGY_V_121_PRIVATE_KEY_BASE58,
        ),
        StrategyName::V131 => (
            settings::STRATEGY_V_131_PRIVATE_KEY_ENCRYPTED,
            settings::STRATEGY_V_131_PRIVATE_KEY_BASE58,
        ),
    };
    let private_key = if !encrypted.trim().is_empty() {
        decrypt_trading_private_key(
            encrypted.trim(),
            std::path::Path::new(settings::WALLET_KEY_FILE),
        )?
        .trim()
        .to_string()
    } else if !plain.trim().is_empty() {
        plain.trim().to_string()
    } else {
        bail!(
            "{} has no trading wallet; set STRATEGY_V_0XX_PRIVATE_KEY_ENCRYPTED in src/bindings.rs",
            name.as_str()
        );
    };
    let secret = bs58::decode(&private_key).into_vec().context(format!(
        "{} trading private key is not base58",
        name.as_str()
    ))?;
    if secret.len() != 64 {
        bail!(
            "{} trading private key must decode to 64 bytes",
            name.as_str()
        );
    }
    Keypair::try_from(secret.as_slice()).context(format!("{} trading private key", name.as_str()))
}

pub fn authenticated_helius_rpc_url(rpc_url: &str, api_key: &str) -> String {
    if let Ok(mut url) = reqwest::Url::parse(rpc_url) {
        if url.host_str().unwrap_or("").ends_with("helius-rpc.com")
            && url.query_pairs().all(|(k, _)| k != "api-key")
        {
            url.query_pairs_mut().append_pair("api-key", api_key);
        }
        return url.to_string();
    }
    rpc_url.to_string()
}

fn require_str(name: &str, value: &str) -> Result<String> {
    let value = value.trim();
    if value.is_empty() {
        bail!("{name} is required — edit src/bindings.rs");
    }
    Ok(value.to_string())
}

/// Untagged journal records belong to the first active strategy.
pub fn journal_record_matches_strategy(
    record_strategy: Option<&str>,
    slot_name: StrategyName,
    active: &[StrategyName],
) -> bool {
    match record_strategy {
        Some(value) => StrategyName::parse(value) == Some(slot_name),
        None => active.first().copied() == Some(slot_name),
    }
}

#[cfg(test)]
mod tests {
    use super::QuotePairs;

    #[test]
    fn quote_pairs_parse_and_filter() {
        assert_eq!(QuotePairs::parse("").unwrap(), QuotePairs::Sol);
        assert_eq!(QuotePairs::parse("SOL").unwrap(), QuotePairs::Sol);
        assert_eq!(QuotePairs::parse("non_sol").unwrap(), QuotePairs::NonSol);
        assert_eq!(QuotePairs::parse("both").unwrap(), QuotePairs::Both);
        assert!(QuotePairs::parse("everything").is_err());
        assert!(QuotePairs::Sol.allows(true));
        assert!(!QuotePairs::Sol.allows(false));
        assert!(!QuotePairs::NonSol.allows(true));
        assert!(QuotePairs::NonSol.allows(false));
        assert!(QuotePairs::Both.allows(true));
        assert!(QuotePairs::Both.allows(false));
    }
}
