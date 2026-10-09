use crate::bindings as settings;
use crate::events::sol_to_lamports;
use crate::security::decrypt_trading_private_key;
use anyhow::{bail, Context, Result};
use solana_sdk::signature::Keypair;
use solana_sdk::signer::Signer;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StrategyName {
    V011,
    V022,
    V031,
}

impl StrategyName {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::V011 => "strategy_v_011",
            Self::V022 => "strategy_v_022",
            Self::V031 => "strategy_v_031",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            "strategy_v_011" => Some(Self::V011),
            "strategy_v_022" => Some(Self::V022),
            "strategy_v_031" => Some(Self::V031),
            _ => None,
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
    /// Same knobs as `bot_target` `src/config/strategyV031.ts`.
    pub fn default_v031() -> Self {
        let mut cfg = Self {
            gate_wallet: "3VUNtVtjjx5ckUojT7UocJ5fbuAJRsNUXNfTBnPte9vC".to_string(),
            dust_sol: 0.1,
            min_entry_s: 4.0,
            max_entry_s: 20.0,
            buy_hit_count: 4,
            buy_hit_min: 1.0,
            sell_hit_count: 4,
            sell_hit_min: 4.0,
            take_profit: 0.25,
            max_hold_s: 85.0,
            target_sell_exit: true,
            first_cycle_only: true,
            repeat_entries: false,
            size_sol: 0.4,
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

pub struct StrategyPlan {
    pub name: StrategyName,
    pub target_wallet: String,
    pub buy_amount_lamports: u64,
    pub max_entry_market_cap_sol: f64,
    pub keypair: Keypair,
    pub target_sell_exit: bool,
    pub first_cycle_only: bool,
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
    let mut strategy_plans = Vec::with_capacity(active.len());
    for name in &active {
        strategy_plans.push(build_plan(*name, &strategy_v011, &strategy_v022, &strategy_v031)?);
    }
    let strategy_label = active.iter().map(|n| n.as_str()).collect::<Vec<_>>().join(", ");

    Ok(AppConfig {
        strategy_label,
        strategy_plans,
        strategy_v011,
        strategy_v022,
        strategy_v031,
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
    Ok(StrategyPlan {
        name,
        target_wallet: target_wallet.trim().to_string(),
        buy_amount_lamports,
        max_entry_market_cap_sol: max_mc,
        keypair: resolve_trading_keypair(name)?,
        target_sell_exit,
        first_cycle_only,
    })
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
    };
    let private_key = if !encrypted.trim().is_empty() {
        decrypt_trading_private_key(encrypted.trim(), std::path::Path::new(settings::WALLET_KEY_FILE))?
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
    let secret = bs58::decode(&private_key)
        .into_vec()
        .context(format!("{} trading private key is not base58", name.as_str()))?;
    if secret.len() != 64 {
        bail!("{} trading private key must decode to 64 bytes", name.as_str());
    }
    Keypair::try_from(secret.as_slice()).context(format!("{} trading private key", name.as_str()))
}

pub fn authenticated_helius_rpc_url(rpc_url: &str, api_key: &str) -> String {
    if let Ok(mut url) = reqwest::Url::parse(rpc_url) {
        if url.host_str().unwrap_or("").ends_with("helius-rpc.com") && url.query_pairs().all(|(k, _)| k != "api-key") {
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
pub fn journal_record_matches_strategy(record_strategy: Option<&str>, slot_name: StrategyName, active: &[StrategyName]) -> bool {
    match record_strategy {
        Some(value) => StrategyName::parse(value) == Some(slot_name),
        None => active.first().copied() == Some(slot_name),
    }
}
