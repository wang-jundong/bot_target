use serde::{Deserialize, Serialize};

pub const LAMPORTS_PER_SOL: u64 = 1_000_000_000;
/// Live prices are lamports per raw token. Strategy prices are SOL per 1e6-raw-token unit.
pub const LIVE_PRICE_TO_STRATEGY: f64 = 1e-3;
pub const MARKET_CAP_MULTIPLIER: f64 = 1_000_000.0;

#[derive(Debug, Clone)]
pub struct PumpCurveSnapshot {
    pub virtual_quote_reserves: u128,
    pub virtual_token_reserves: u128,
    pub real_token_reserves: u128,
    pub creator: String,
    pub mayhem_mode: bool,
    pub quote_mint: Option<String>,
    pub protocol_fee_bps: u128,
    pub creator_fee_bps: u128,
    pub fee_recipient: Option<String>,
    pub cashback: bool,
}

#[derive(Debug, Clone)]
pub struct PumpSwapSnapshot {
    pub pool: String,
    pub base_mint: String,
    pub quote_mint: String,
    pub pool_base_token_account: String,
    pub pool_quote_token_account: String,
    pub base_token_program: String,
    pub quote_token_program: String,
    pub base_reserve: u128,
    pub quote_reserve: u128,
    pub virtual_quote_reserves: i128,
    pub lp_fee_bps: u128,
    pub protocol_fee_bps: u128,
    pub coin_creator_fee_bps: u128,
    pub coin_creator: String,
    pub protocol_fee_recipient: String,
    pub buyback_fee_recipient: String,
    pub cashback: bool,
}

#[derive(Debug, Clone)]
pub struct PoolTradeEvent {
    pub signature: String,
    pub slot: u64,
    pub transaction_index: Option<u64>,
    pub event_index: u32,
    pub timestamp_ms: u64,
    pub received_mono_ms: f64,
    pub mint: String,
    pub pool: String,
    pub program_id: String,
    pub trader: String,
    pub side: Side,
    pub sol_amount: u64,
    pub token_amount: u64,
    pub price: f64,
    pub curve_progress: Option<f64>,
    pub curve: Option<PumpCurveSnapshot>,
    pub swap: Option<PumpSwapSnapshot>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Buy,
    Sell,
}

impl Side {
    pub fn as_str(self) -> &'static str {
        match self {
            Side::Buy => "buy",
            Side::Sell => "sell",
        }
    }
}

pub fn event_key(signature: &str, event_index: u32) -> String {
    format!("{signature}:{event_index}")
}

pub fn compare_events(a: &PoolTradeEvent, b: &PoolTradeEvent) -> std::cmp::Ordering {
    a.slot
        .cmp(&b.slot)
        .then(a.transaction_index.unwrap_or(u64::MAX).cmp(&b.transaction_index.unwrap_or(u64::MAX)))
        .then(a.event_index.cmp(&b.event_index))
        .then(a.received_mono_ms.total_cmp(&b.received_mono_ms))
}

#[derive(Debug, Clone)]
pub struct TokenBalanceInfo {
    pub account_index: usize,
    pub mint: String,
    pub owner: String,
    pub program_id: String,
    pub amount: u64,
}

#[derive(Debug, Clone)]
pub struct CompiledIx {
    pub program_id_index: usize,
    pub accounts: Vec<usize>,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct ParsedTargetTransaction {
    pub signature: String,
    pub slot: u64,
    pub timestamp_ms: u64,
    pub account_keys: Vec<String>,
    pub program_ids: Vec<String>,
    pub logs: Vec<String>,
    pub instructions: Vec<CompiledIx>,
    pub inner_instructions: Vec<CompiledIx>,
    pub post_token_balances: Vec<TokenBalanceInfo>,
    pub failed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PoolDescriptor {
    pub mint: String,
    pub pool: String,
    pub program_id: String,
    pub venue: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_program: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quote_mint: Option<String>,
    /// PumpSwap vault ATAs — persisted so recover can re-register without a live swap cache.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pool_base_token_account: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pool_quote_token_account: Option<String>,
    pub relevant_accounts: Vec<String>,
}

pub fn pool_key(program_id: &str, pool: &str) -> String {
    format!("{program_id}:{pool}")
}

pub fn sol_to_lamports(value: &str) -> anyhow::Result<u64> {
    let value = value.trim();
    let ok = value
        .split_once('.')
        .map(|(whole, frac)| !whole.is_empty() && whole.chars().all(|c| c.is_ascii_digit()) && (1..=9).contains(&frac.len()) && frac.chars().all(|c| c.is_ascii_digit()))
        .unwrap_or_else(|| !value.is_empty() && value.chars().all(|c| c.is_ascii_digit()));
    if !ok {
        anyhow::bail!("SOL value must be a non-negative decimal with <=9 places");
    }
    let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
    let whole: u64 = whole.parse()?;
    let mut frac = fraction.to_string();
    while frac.len() < 9 {
        frac.push('0');
    }
    let fraction: u64 = if frac.is_empty() { 0 } else { frac.parse()? };
    Ok(whole * LAMPORTS_PER_SOL + fraction)
}

pub fn lamports_to_sol(lamports: u64) -> f64 {
    lamports as f64 / LAMPORTS_PER_SOL as f64
}

pub fn sol_to_lamports_f64(sol: f64) -> u64 {
    if !sol.is_finite() || sol <= 0.0 {
        return 0;
    }
    (sol * LAMPORTS_PER_SOL as f64).round() as u64
}

pub fn to_strategy_price(live_price: f64) -> f64 {
    if live_price > 0.0 { live_price * LIVE_PRICE_TO_STRATEGY } else { 0.0 }
}

pub fn mono_ms() -> f64 {
    use std::sync::OnceLock;
    use std::time::Instant;
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_secs_f64() * 1000.0
}
