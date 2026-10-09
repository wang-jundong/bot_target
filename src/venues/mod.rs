pub mod decoder;
pub mod ids;
pub mod quote;
pub mod trade;

use crate::events::{PumpCurveSnapshot, PumpSwapSnapshot};
use crate::state::PreparedTrade;
use anyhow::Result;
use solana_sdk::pubkey::Pubkey;
use std::collections::HashMap;
use std::sync::RwLock;

pub struct Venues {
    curves: RwLock<HashMap<String, PumpCurveSnapshot>>,
    pools: RwLock<HashMap<String, PumpSwapSnapshot>>,
    pub compute_unit_limit: u32,
    pub priority_fee_lamports: u64,
    pub tip_lamports: u64,
}

impl Venues {
    pub fn new(compute_unit_limit: u32, priority_fee_lamports: u64, tip_lamports: u64) -> Self {
        Self {
            curves: RwLock::new(HashMap::new()),
            pools: RwLock::new(HashMap::new()),
            compute_unit_limit,
            priority_fee_lamports,
            tip_lamports,
        }
    }

    pub fn note_curve(&self, mint: &str, curve: PumpCurveSnapshot) {
        self.curves.write().expect("curve map").insert(mint.to_string(), curve);
    }

    pub fn note_swap(&self, mint: &str, swap: PumpSwapSnapshot) {
        self.pools.write().expect("pool map").insert(mint.to_string(), swap);
    }

    pub fn get_swap(&self, mint: &str) -> Option<PumpSwapSnapshot> {
        self.pools.read().expect("pool map").get(mint).cloned()
    }

    pub fn get_curve(&self, mint: &str) -> Option<PumpCurveSnapshot> {
        self.curves.read().expect("curve map").get(mint).cloned()
    }

    pub fn update_amm_vault(&self, mint: &str, is_base: bool, amount: u64) {
        let mut pools = self.pools.write().expect("pool map");
        let Some(swap) = pools.get_mut(mint) else { return };
        if is_base {
            swap.base_reserve = amount as u128;
        } else {
            swap.quote_reserve = amount as u128;
        }
    }

    pub fn build_buy(&self, venue: &str, owner: &Pubkey, mint: &str, token_program: &Pubkey, lamports: u64, slippage_bps: u64) -> Result<PreparedTrade> {
        if venue == "pumpswap" {
            let pools = self.pools.read().expect("pool map");
            let swap = pools.get(mint).ok_or_else(|| anyhow::anyhow!("no streamed pumpswap pool for {mint}"))?;
            return trade::build_amm_buy(owner, lamports, slippage_bps, swap, self.compute_unit_limit, self.priority_fee_lamports, self.tip_lamports);
        }
        let curves = self.curves.read().expect("curve map");
        let curve = curves.get(mint).ok_or_else(|| anyhow::anyhow!("no streamed curve for {mint}"))?;
        trade::build_pump_buy(owner, &mint.parse()?, token_program, lamports, slippage_bps, curve, self.compute_unit_limit, self.priority_fee_lamports, self.tip_lamports)
    }

    pub fn build_sell(&self, venue: &str, owner: &Pubkey, mint: &str, token_program: &Pubkey, token_amount: u64, slippage_bps: u64) -> Result<PreparedTrade> {
        if venue == "pumpswap" {
            let pools = self.pools.read().expect("pool map");
            let swap = pools.get(mint).ok_or_else(|| anyhow::anyhow!("no streamed pumpswap pool for {mint}"))?;
            return trade::build_amm_sell(owner, token_amount, slippage_bps, swap, self.compute_unit_limit, self.priority_fee_lamports, self.tip_lamports);
        }
        let curves = self.curves.read().expect("curve map");
        let curve = curves.get(mint).ok_or_else(|| anyhow::anyhow!("no streamed curve for {mint}"))?;
        trade::build_pump_sell(owner, &mint.parse()?, token_program, token_amount, slippage_bps, curve, self.compute_unit_limit, self.priority_fee_lamports, self.tip_lamports)
    }
}
