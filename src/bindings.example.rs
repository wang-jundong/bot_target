//! Template for local secrets & runtime settings.
//!
//! Setup:
//!   cp src/bindings.example.rs src/bindings.rs
//! then fill in real values. `src/bindings.rs` is gitignored.
//!
//! Gate wallets and position sizes come from the strategy configs in `src/config.rs`
//! (ported from bot_target). Each active strategy needs its own trading wallet.

#![allow(dead_code)]

pub const EXECUTION_MODE: &str = "live";

/// Strategies this process runs. Each keeps its own position and signs with its own wallet.
pub const ACTIVE_STRATEGIES: &[&str] = &["strategy_v_022", "strategy_v_031"];

pub const BLOCKED_MINTS: &str = "";

pub const VIBE_GRPC_ENDPOINT: &str = "https://elite.grpc.solanavibestation.com";
pub const VIBE_GRPC_TOKEN: &str = "";

pub const HELIUS_API_KEY: &str = "";
pub const HELIUS_RPC_URL: &str = "";
pub const HELIUS_SENDER_URL: &str = "https://sender.helius-rpc.com/fast";
pub const HELIUS_SENDER_SWQOS_ONLY: bool = true;

/// Fernet+XOR key file (used when any `*_PRIVATE_KEY_ENCRYPTED` is set).
pub const WALLET_KEY_FILE: &str = "./secrets/solana.key";

pub const STRATEGY_V_011_PRIVATE_KEY_ENCRYPTED: &str = "";
pub const STRATEGY_V_011_PRIVATE_KEY_BASE58: &str = "";
pub const STRATEGY_V_022_PRIVATE_KEY_ENCRYPTED: &str = "";
pub const STRATEGY_V_022_PRIVATE_KEY_BASE58: &str = "";
pub const STRATEGY_V_031_PRIVATE_KEY_ENCRYPTED: &str = "";
pub const STRATEGY_V_031_PRIVATE_KEY_BASE58: &str = "";

pub const BUY_SLIPPAGE_BPS: u64 = 1200;
pub const SELL_SLIPPAGE_BPS: u64 = 5000;
pub const PRIORITY_FEE_LAMPORTS: u64 = 100_000;
pub const HELIUS_TIP_LAMPORTS: u64 = 5_000;
pub const COMPUTE_UNIT_LIMIT: u64 = 400_000;

pub const EVENT_RETENTION_SEC: f64 = 180.0;
pub const BLOCKHASH_REFRESH_MS: u64 = 10_000;
pub const LOG_LEVEL: &str = "info";
pub const RECOVERY_PATH: &str = "./recovery/lifecycle.jsonl";
pub const PNL_PATH: &str = "./logs/pnl.jsonl";
