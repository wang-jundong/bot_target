pub mod common_alt;
pub mod decoder;
pub mod ids;
mod jupiter;
pub mod quote;
pub mod route;
pub mod trade;

use crate::events::{PumpCurveSnapshot, PumpSwapSnapshot};
use crate::state::PreparedTrade;
use anyhow::Result;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

pub struct Venues {
    curves: RwLock<HashMap<String, PumpCurveSnapshot>>,
    pools: RwLock<HashMap<String, PumpSwapSnapshot>>,
    pub compute_unit_limit: u32,
    pub priority_fee_lamports: u64,
    pub tip_lamports: u64,
    routes: route::Book,
}

impl Venues {
    pub fn new(compute_unit_limit: u32, priority_fee_lamports: u64, tip_lamports: u64) -> Self {
        Self {
            curves: RwLock::new(HashMap::new()),
            pools: RwLock::new(HashMap::new()),
            compute_unit_limit,
            priority_fee_lamports,
            tip_lamports,
            routes: route::Book::new(),
        }
    }

    pub fn trades_native_sol(&self, mint: &str) -> bool {
        let curve = self
            .get_curve(mint)
            .map(|curve| ids::is_sol_quote(curve.quote_mint.as_deref()));
        let swap = self
            .get_swap(mint)
            .map(|swap| ids::is_sol_quote(Some(swap.quote_mint.as_str())));
        !matches!((curve, swap), (Some(false), _) | (_, Some(false)))
    }

    pub fn cached_quote_scale(&self, mint: &str, quote_mint: Option<&str>) -> Option<f64> {
        let mint = mint.parse().ok()?;
        let quote = quote_mint.and_then(|text| text.parse().ok());
        self.routes.cached_event_scale(&mint, quote.as_ref())
    }

    pub async fn quote_lamports_per_raw(&self, rpc: &RpcClient, mint: &str) -> Result<f64> {
        self.routes.lamports_per_quote(rpc, &mint.parse()?).await
    }

    /// Fetch the sell leg for the token amount the buy quote expects, so a
    /// sell that fires with the fill does not start Jupiter from cold.
    pub fn prime_sell(
        self: &Arc<Self>,
        rpc: Arc<RpcClient>,
        owner: Pubkey,
        mint: String,
        sell_slippage: u64,
    ) {
        let Ok(mint_key) = mint.parse::<Pubkey>() else {
            return;
        };
        let Some(tokens) = route::expected_buy(&owner, &mint_key) else {
            return;
        };
        let venues = Arc::clone(self);
        tokio::spawn(async move {
            route::warm_sell(&venues.routes, rpc, owner, &mint_key, tokens, sell_slippage).await;
        });
    }

    pub fn note_curve(&self, mint: &str, curve: PumpCurveSnapshot) {
        self.curves
            .write()
            .expect("curve map")
            .insert(mint.to_string(), curve);
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

    pub async fn build_buy(
        &self,
        rpc: &RpcClient,
        venue: &str,
        owner: &Pubkey,
        mint: &str,
        token_program: &Pubkey,
        lamports: u64,
        slippage_bps: u64,
        refresh_route: bool,
    ) -> Result<PreparedTrade> {
        if let Some(prepared) = self
            .build_routed(
                rpc,
                owner,
                mint,
                true,
                lamports,
                slippage_bps,
                refresh_route,
            )
            .await?
        {
            return Ok(prepared);
        }
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
        )
    }

    pub async fn build_sell(
        &self,
        rpc: &RpcClient,
        venue: &str,
        owner: &Pubkey,
        mint: &str,
        token_program: &Pubkey,
        token_amount: u64,
        slippage_bps: u64,
        refresh_route: bool,
    ) -> Result<PreparedTrade> {
        if let Some(prepared) = self
            .build_routed(
                rpc,
                owner,
                mint,
                false,
                token_amount,
                slippage_bps,
                refresh_route,
            )
            .await?
        {
            return Ok(prepared);
        }
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
        )
    }

    async fn build_routed(
        &self,
        rpc: &RpcClient,
        owner: &Pubkey,
        mint: &str,
        buy: bool,
        amount: u64,
        slippage_bps: u64,
        refresh_route: bool,
    ) -> Result<Option<PreparedTrade>> {
        if self.trades_native_sol(mint) {
            return Ok(None);
        }
        let curve = self.get_curve(mint);
        let swap = self.get_swap(mint);
        let prepared = route::prepare(
            &self.routes,
            rpc,
            owner,
            &mint.parse()?,
            buy,
            amount,
            slippage_bps,
            self.compute_unit_limit,
            self.priority_fee_lamports,
            self.tip_lamports,
            refresh_route,
            curve.as_ref(),
            swap.as_ref(),
        )
        .await?;
        Ok(Some(prepared))
    }

    /// Prepare the Jupiter SOL leg while the strategy is watching this mint.
    pub fn warm_quote(
        self: &Arc<Self>,
        rpc: Arc<RpcClient>,
        owner: Pubkey,
        mint: String,
        buy_lamports: u64,
        sell_tokens: u64,
        sell_slippage: u64,
    ) {
        if buy_lamports == 0 || !route::claim_warm(&owner, &mint) {
            return;
        }
        let venues = Arc::clone(self);
        tokio::spawn(async move {
            route::warm(
                &venues.routes,
                rpc,
                owner,
                &mint,
                buy_lamports,
                sell_tokens,
                sell_slippage,
            )
            .await;
        });
    }
}
