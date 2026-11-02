pub mod decoder;
pub mod ids;
pub mod launchpad;
pub mod quote;
pub mod trade;

use crate::events::{PumpCurveSnapshot, PumpSwapSnapshot};
use crate::venues::launchpad::LaunchpadPoolState;
use crate::state::PreparedTrade;
use anyhow::Result;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::RwLock;

pub struct Venues {
    curves: RwLock<HashMap<String, PumpCurveSnapshot>>,
    /// Bonding curves whose account `complete` flag has been seen.
    completed: RwLock<HashSet<String>>,
    pools: RwLock<HashMap<String, PumpSwapSnapshot>>,
    launchpads: RwLock<HashMap<String, LaunchpadPoolState>>,
    /// Curve buys and sells append the buyback pair only while this is set.
    buyback_enabled: AtomicBool,
    pub compute_unit_limit: u32,
    pub priority_fee_lamports: u64,
    pub tip_lamports: u64,
}

impl Venues {
    pub fn new(compute_unit_limit: u32, priority_fee_lamports: u64, tip_lamports: u64) -> Self {
        Self {
            curves: RwLock::new(HashMap::new()),
            completed: RwLock::new(HashSet::new()),
            pools: RwLock::new(HashMap::new()),
            launchpads: RwLock::new(HashMap::new()),
            // Kept on until the Global account is read, matching the previous
            // always-on accounts. A successful read replaces it.
            buyback_enabled: AtomicBool::new(true),
            compute_unit_limit,
            priority_fee_lamports,
            tip_lamports,
        }
    }

    pub fn buyback_enabled(&self) -> bool {
        self.buyback_enabled.load(Ordering::Relaxed)
    }

    pub fn set_buyback(&self, enabled: bool) {
        let previous = self.buyback_enabled.swap(enabled, Ordering::Relaxed);
        if previous != enabled {
            tracing::info!(buyback = enabled, "[01 STARTUP] Pump buyback accounts");
        }
    }

    /// Read `Global.buyback_basis_points` and store whether curve instructions
    /// must carry the buyback account pair.
    pub async fn refresh_buyback(&self, rpc: &RpcClient) -> Result<()> {
        let account = rpc.get_account(&ids::pump_global()).await?;
        let enabled = ids::parse_pump_buyback(&account.data)
            .ok_or_else(|| anyhow::anyhow!("pump global buyback flag missing"))?;
        self.set_buyback(enabled);
        Ok(())
    }

    pub fn note_curve(&self, mint: &str, curve: PumpCurveSnapshot) {
        self.curves
            .write()
            .expect("curve map")
            .insert(mint.to_string(), curve);
    }

    pub fn mark_curve_live(&self, mint: &str) {
        self.completed.write().expect("completed curves").remove(mint);
    }

    pub fn mark_curve_complete(&self, mint: &str) {
        self.completed
            .write()
            .expect("completed curves")
            .insert(mint.to_string());
    }

    pub fn is_curve_complete(&self, mint: &str) -> bool {
        self.completed
            .read()
            .expect("completed curves")
            .contains(mint)
    }

    pub fn note_swap(&self, mint: &str, swap: PumpSwapSnapshot) {
        self.pools
            .write()
            .expect("pool map")
            .insert(mint.to_string(), swap);
    }

    pub fn get_swap(&self, mint: &str) -> Option<PumpSwapSnapshot> {
        self.pools.read().expect("pool map").get(mint).cloned()
    }

    pub fn get_curve(&self, mint: &str) -> Option<PumpCurveSnapshot> {
        self.curves.read().expect("curve map").get(mint).cloned()
    }

    pub fn note_launchpad(&self, mint: &str, pool: LaunchpadPoolState) {
        self.launchpads
            .write()
            .expect("launchpad map")
            .insert(mint.to_string(), pool);
    }

    pub fn get_launchpad(&self, mint: &str) -> Option<LaunchpadPoolState> {
        self.launchpads
            .read()
            .expect("launchpad map")
            .get(mint)
            .copied()
    }

    pub fn update_amm_vault(&self, mint: &str, is_base: bool, amount: u64) {
        let mut pools = self.pools.write().expect("pool map");
        let Some(swap) = pools.get_mut(mint) else {
            return;
        };
        if is_base {
            swap.base_reserve = amount as u128;
        } else {
            swap.quote_reserve = amount as u128;
        }
    }

    pub fn build_buy(
        &self,
        venue: &str,
        owner: &Pubkey,
        mint: &str,
        token_program: &Pubkey,
        lamports: u64,
        slippage_bps: u64,
    ) -> Result<PreparedTrade> {
        if venue == "pumpswap" {
            let pools = self.pools.read().expect("pool map");
            let swap = pools
                .get(mint)
                .ok_or_else(|| anyhow::anyhow!("no streamed pumpswap pool for {mint}"))?;
            return trade::build_amm_buy(
                owner,
                lamports,
                slippage_bps,
                swap,
                self.compute_unit_limit,
                self.priority_fee_lamports,
                self.tip_lamports,
            );
        }
        let curves = self.curves.read().expect("curve map");
        let curve = curves
            .get(mint)
            .ok_or_else(|| anyhow::anyhow!("no streamed curve for {mint}"))?;
        trade::build_pump_buy(
            owner,
            &mint.parse()?,
            token_program,
            lamports,
            slippage_bps,
            curve,
            self.compute_unit_limit,
            self.priority_fee_lamports,
            self.tip_lamports,
            self.buyback_enabled(),
        )
    }

    pub fn build_sell(
        &self,
        venue: &str,
        owner: &Pubkey,
        mint: &str,
        token_program: &Pubkey,
        token_amount: u64,
        slippage_bps: u64,
    ) -> Result<PreparedTrade> {
        if venue == "pumpswap" {
            let pools = self.pools.read().expect("pool map");
            let swap = pools
                .get(mint)
                .ok_or_else(|| anyhow::anyhow!("no streamed pumpswap pool for {mint}"))?;
            return trade::build_amm_sell(
                owner,
                token_amount,
                slippage_bps,
                swap,
                self.compute_unit_limit,
                self.priority_fee_lamports,
                self.tip_lamports,
            );
        }
        let curves = self.curves.read().expect("curve map");
        let curve = curves
            .get(mint)
            .ok_or_else(|| anyhow::anyhow!("no streamed curve for {mint}"))?;
        trade::build_pump_sell(
            owner,
            &mint.parse()?,
            token_program,
            token_amount,
            slippage_bps,
            curve,
            self.compute_unit_limit,
            self.priority_fee_lamports,
            self.tip_lamports,
            self.buyback_enabled(),
        )
    }
}
