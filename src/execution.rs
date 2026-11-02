use crate::alt_quote::{ABANDON_RECLAIM_DELAY, AltQuote, RECLAIM_DELAY, too_large};
use crate::events::mono_ms;
use crate::helius::{
    HeliusSender, HeliusSenderUnavailable, is_empty_sell_error, is_non_retryable_buy_error,
};
use crate::journal::{ClosedTradePnlInput, PnlJournal, RecoveryJournal, create_pnl_record};
use crate::state::{Lifecycle, TokenInner, is_holding};
use crate::venues::Venues;
use crate::venues::decoder::fill_from_logs;
use crate::venues::launchpad::{self, LaunchpadPoolState};
use crate::venues::trade::{
    build_amm_sell, build_pump_sell_alt, compile_trade, compile_v0_ixs, compile_v1_ixs,
    pump_alt_buy_instructions, transaction_wire_bytes,
};
use anyhow::{Context, Result, anyhow};
use base64::Engine;
use serde_json::{Value, json};
use solana_account_decoder_client_types::UiAccountData;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_client::rpc_config::{RpcSendTransactionConfig, RpcTransactionConfig};
use solana_client::rpc_request::TokenAccountsFilter;
use solana_commitment_config::CommitmentConfig;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signature};
use solana_sdk::signer::Signer;
use solana_sdk::transaction::VersionedTransaction;
use solana_transaction_status_client_types::{
    EncodedConfirmedTransactionWithStatusMeta, TransactionConfirmationStatus,
    UiTransactionEncoding, UiTransactionTokenBalance, option_serializer::OptionSerializer,
};
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

pub struct BlockhashCache {
    client: Arc<RpcClient>,
    refresh_ms: u64,
    current: Mutex<Option<(solana_sdk::hash::Hash, Instant)>>,
}

impl BlockhashCache {
    pub fn new(client: Arc<RpcClient>, refresh_ms: u64) -> Self {
        Self {
            client,
            refresh_ms,
            current: Mutex::new(None),
        }
    }

    pub async fn refresh(&self) -> Result<()> {
        let blockhash = self.client.get_latest_blockhash().await?;
        *self.current.lock().await = Some((blockhash, Instant::now()));
        Ok(())
    }

    pub fn spawn(self: &Arc<Self>) {
        let cache = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                if let Err(err) = cache.refresh().await {
                    tracing::error!(error = %err, "[01 STARTUP] Blockhash refresh failed");
                }
                tokio::time::sleep(Duration::from_millis(cache.refresh_ms)).await;
            }
        });
    }

    pub async fn get(&self) -> Result<solana_sdk::hash::Hash> {
        // A hash is valid for about a minute. The background refresh keeps it fresh.
        // Blocking the sell on a refresh RPC whenever the cache is 20s old was the send delay.
        if let Some((hash, at)) = *self.current.lock().await {
            if cached_blockhash_is_usable(at.elapsed()) {
                return Ok(hash);
            }
        }
        self.refresh().await?;
        self.current
            .lock()
            .await
            .ok_or_else(|| anyhow!("blockhash unavailable"))
            .map(|(hash, _)| hash)
    }
}

fn cached_blockhash_is_usable(age: Duration) -> bool {
    age <= Duration::from_secs(60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sell_uses_a_cached_blockhash_without_refreshing() {
        assert!(cached_blockhash_is_usable(Duration::from_secs(20)));
        assert!(cached_blockhash_is_usable(Duration::from_secs(60)));
        assert!(!cached_blockhash_is_usable(Duration::from_secs(61)));
    }
}

pub struct LiveExecution {
    pub rpc: Arc<RpcClient>,
    pub wallet: Arc<Keypair>,
    pub strategy: String,
    pub buy_lamports: u64,
    pub buy_slippage_bps: u64,
    pub sell_slippage_bps: u64,
    pub blockhashes: Arc<BlockhashCache>,
    pub sender: HeliusSender,
    pub journal: Arc<RecoveryJournal>,
    pub pnl: Arc<PnlJournal>,
    pub venues: Arc<Venues>,
    pub quotes: Arc<AltQuote>,
}

impl LiveExecution {
    pub async fn send_buy(self: &Arc<Self>, token: &Mutex<TokenInner>) -> Result<()> {
        let venue = token.lock().await.descriptor.venue.clone();
        if venue == "launchlab" {
            if self.alt_non_sol_quote(token).await.is_some() {
                return self.send_alt_launchpad_buy(token).await;
            }
            return self.send_sol_launchpad_buy(token).await;
        }
        if self.alt_non_sol_quote(token).await.is_some() {
            return self.send_alt_pump_buy(token).await;
        }
        let mut last_error = String::from("buy failed");
        for attempt in 1..=3 {
            let prepared = {
                let mut state = token.lock().await;
                if attempt > 1 {
                    if let Some(price) = state.prices.current_mark_price {
                        state.prices.expected_entry_price = Some(price);
                    }
                }
                let slippage = state.buy_slippage_bps.unwrap_or(self.buy_slippage_bps);
                let lamports = state.buy_lamports.unwrap_or(self.buy_lamports);
                let token_program_owned = state
                    .descriptor
                    .token_program
                    .clone()
                    .unwrap_or_else(|| crate::venues::ids::TOKEN_PROGRAM.to_string());
                let token_program = token_program_owned.parse()?;
                let venue = state.descriptor.venue.clone();
                let mint = state.descriptor.mint.clone();
                if attempt > 1 || state.prepared_buy.is_none() {
                    state.prepared_buy = Some(self.venues.build_buy(
                        &venue,
                        &self.wallet.pubkey(),
                        &mint,
                        &token_program,
                        lamports,
                        slippage,
                    )?);
                }
                state.prepared_buy.clone()
            };
            let Some(prepared) = prepared else {
                last_error = "buy was not prepared".to_string();
                continue;
            };
            match self
                .submit_and_confirm(token, &prepared, true, attempt, None)
                .await
            {
                Ok((_signature, token_amount, sol_amount, price)) => {
                    self.accept_buy_fill(token, token_amount, sol_amount, price)
                        .await?;
                    return Ok(());
                }
                Err(err) => {
                    last_error = err.to_string();
                    let venue = token.lock().await.descriptor.venue.clone();
                    let mint = token.lock().await.descriptor.mint.clone();
                    tracing::warn!(attempt, mint = %mint, error = %last_error, "[05 ENTRY] Buy attempt failed");
                    if is_non_retryable_buy_error(&last_error, &venue) {
                        let mut state = token.lock().await;
                        self.reject_buy(&mut state, "buy_slippage_rejected", &last_error)
                            .await;
                        return Ok(());
                    }
                    // Drop stale prepared tx so next attempt rebuilds from latest geyser reserves.
                    token.lock().await.prepared_buy = None;
                }
            }
        }
        let mut state = token.lock().await;
        self.reject_buy(&mut state, "buy_failed", &last_error).await;
        Ok(())
    }

    async fn alt_non_sol_quote(&self, token: &Mutex<TokenInner>) -> Option<Pubkey> {
        let state = token.lock().await;
        if state.descriptor.venue != "pump" && state.descriptor.venue != "launchlab" {
            return None;
        }
        let quote = state.descriptor.quote_mint.as_deref()?;
        if crate::venues::ids::is_sol_quote(Some(quote)) {
            return None;
        }
        quote.parse().ok()
    }

    /// Plugin buy: Jupiter SOL→quote and `buy_exact_quote_in_v2` in one v1 transaction.
    async fn send_alt_pump_buy(self: &Arc<Self>, token: &Mutex<TokenInner>) -> Result<()> {
        let mut last_error = String::from("alt-quote buy failed");
        for attempt in 1..=3 {
            let (slippage, lamports, token_program, mint, venue) = {
                let mut state = token.lock().await;
                if attempt > 1 {
                    if let Some(price) = state.prices.current_mark_price {
                        state.prices.expected_entry_price = Some(price);
                    }
                }
                let token_program = state
                    .descriptor
                    .token_program
                    .clone()
                    .unwrap_or_else(|| crate::venues::ids::TOKEN_PROGRAM.to_string());
                (
                    state.buy_slippage_bps.unwrap_or(self.buy_slippage_bps),
                    state.buy_lamports.unwrap_or(self.buy_lamports),
                    token_program,
                    state.descriptor.mint.clone(),
                    state.descriptor.venue.clone(),
                )
            };
            let Some(quote_mint) = self.alt_non_sol_quote(token).await else {
                last_error = "alt-quote mint missing".to_string();
                break;
            };
            let Some(curve) = self.venues.get_curve(&mint) else {
                last_error = format!("no streamed curve for {mint}");
                tracing::warn!(attempt, mint = %mint, error = %last_error, "[05 ENTRY] Buy attempt failed");
                continue;
            };
            let owner = self.wallet.pubkey();
            let token_program = match token_program.parse::<Pubkey>() {
                Ok(program) => program,
                Err(err) => {
                    last_error = err.to_string();
                    continue;
                }
            };
            let legs = match self
                .quotes
                .buy_legs(
                    &owner,
                    &mint.parse()?,
                    &token_program,
                    &quote_mint,
                    lamports,
                    slippage,
                    &curve,
                    self.venues.compute_unit_limit,
                )
                .await
            {
                Ok(legs) => legs,
                Err(err) => {
                    last_error = err.to_string();
                    tracing::warn!(attempt, mint = %mint, error = %last_error, "[05 ENTRY] Buy attempt failed");
                    if is_non_retryable_buy_error(&last_error, &venue) {
                        let mut state = token.lock().await;
                        self.reject_buy(&mut state, "buy_slippage_rejected", &last_error)
                            .await;
                        return Ok(());
                    }
                    continue;
                }
            };
            tracing::info!(
                mint = %mint,
                quote = %quote_mint,
                spend_lamports = lamports,
                quote_atoms = legs.quote_atoms,
                "[05 ENTRY] Alt-quote buy: Jupiter SOL→quote + curve"
            );
            let blockhash = self.blockhashes.get().await?;
            let combo = compile_v1_ixs(
                &owner,
                &AltQuote::combo_instructions(&legs),
                blockhash,
                &self.wallet,
                legs.compute_unit_limit,
                self.venues.priority_fee_lamports,
                self.venues.tip_lamports,
            );
            let v1_failed = combo.is_err();
            let submitted = match combo {
                Ok(tx) => self.submit_compiled(token, tx, true, attempt, None).await,
                Err(err) => Err(err),
            };
            let submitted = match submitted {
                Ok(fill) => Ok(fill),
                Err(err) if v1_failed || combo_needs_split(&err) => {
                    tracing::info!(
                        mint = %mint,
                        error = %err,
                        "[05 ENTRY] Alt-quote combo too large; sending SOL→quote then curve"
                    );
                    match self.send_alt_buy_legs(token, &legs, attempt).await {
                        Ok(fill) => Ok(fill),
                        Err(err) => {
                            last_error = err.to_string();
                            tracing::warn!(attempt, mint = %mint, error = %last_error, "[05 ENTRY] Buy attempt failed");
                            break;
                        }
                    }
                }
                Err(err) => Err(err),
            };
            match submitted {
                Ok((_signature, _log_tokens, _log_quote, _raw_price)) => {
                    token.lock().await.alt_refill_signature = None;
                    self.finish_alt_buy(token, &legs, lamports).await?;
                    return Ok(());
                }
                Err(err) => {
                    last_error = err.to_string();
                    tracing::warn!(attempt, mint = %mint, error = %last_error, "[05 ENTRY] Buy attempt failed");
                    if is_non_retryable_buy_error(&last_error, &venue) {
                        let mut state = token.lock().await;
                        self.reject_buy(&mut state, "buy_slippage_rejected", &last_error)
                            .await;
                        return Ok(());
                    }
                }
            }
        }
        let mut state = token.lock().await;
        self.reject_buy(&mut state, "buy_failed", &last_error).await;
        Ok(())
    }

    /// SOL-quoted LaunchLab: wrap the SOL budget and spend it with `buy_exact_in`.
    async fn send_sol_launchpad_buy(self: &Arc<Self>, token: &Mutex<TokenInner>) -> Result<()> {
        let mut last_error = String::from("launchlab buy failed");
        for attempt in 1..=3 {
            let (slippage, lamports, token_program, mint, pool_key) = {
                let mut state = token.lock().await;
                if attempt > 1 {
                    if let Some(price) = state.prices.current_mark_price {
                        state.prices.expected_entry_price = Some(price);
                    }
                }
                let token_program = state
                    .descriptor
                    .token_program
                    .clone()
                    .unwrap_or_else(|| crate::venues::ids::TOKEN_PROGRAM.to_string());
                (
                    state.buy_slippage_bps.unwrap_or(self.buy_slippage_bps),
                    state.buy_lamports.unwrap_or(self.buy_lamports),
                    token_program,
                    state.descriptor.mint.clone(),
                    state.descriptor.pool.clone(),
                )
            };
            let token_program = match token_program.parse::<Pubkey>() {
                Ok(program) => program,
                Err(err) => {
                    last_error = err.to_string();
                    continue;
                }
            };
            let pool = match self.launchpad_pool(&mint, &pool_key).await {
                Ok(pool) => pool,
                Err(err) => {
                    last_error = err.to_string();
                    tracing::warn!(attempt, mint = %mint, error = %last_error, "[05 ENTRY] Buy attempt failed");
                    continue;
                }
            };
            if pool.completed() {
                let mut state = token.lock().await;
                self.reject_buy(&mut state, "launchlab_complete", "launchlab pool is complete")
                    .await;
                return Ok(());
            }
            let quote_program = *crate::venues::ids::TOKEN_PROGRAM;
            let instructions = match launchpad::sol_buy_instructions(
                &self.wallet.pubkey(),
                &pool,
                &token_program,
                &quote_program,
                lamports,
                slippage,
            ) {
                Ok(ixs) => ixs,
                Err(err) => {
                    last_error = err.to_string();
                    tracing::warn!(attempt, mint = %mint, error = %last_error, "[05 ENTRY] Buy attempt failed");
                    continue;
                }
            };
            let prepared = crate::state::PreparedTrade {
                instructions,
                payer: self.wallet.pubkey(),
                compute_unit_limit: self.venues.compute_unit_limit,
                priority_fee_lamports: self.venues.priority_fee_lamports,
                tip_lamports: self.venues.tip_lamports,
            };
            match self
                .submit_and_confirm(token, &prepared, true, attempt, None)
                .await
            {
                Ok((_signature, token_amount, _parsed_sol, _)) => {
                    let price = if token_amount == 0 {
                        0.0
                    } else {
                        lamports as f64 / token_amount as f64
                    };
                    self.accept_buy_fill(token, token_amount, lamports, price)
                        .await?;
                    return Ok(());
                }
                Err(err) => {
                    last_error = err.to_string();
                    tracing::warn!(attempt, mint = %mint, error = %last_error, "[05 ENTRY] Buy attempt failed");
                    if is_non_retryable_buy_error(&last_error, "launchlab") {
                        let mut state = token.lock().await;
                        self.reject_buy(&mut state, "buy_slippage_rejected", &last_error)
                            .await;
                        return Ok(());
                    }
                }
            }
        }
        let mut state = token.lock().await;
        self.reject_buy(&mut state, "buy_failed", &last_error).await;
        Ok(())
    }

    /// Plugin buy on LaunchLab: Jupiter SOL→quote and `buy_exact_in` in one v1 transaction.
    async fn send_alt_launchpad_buy(self: &Arc<Self>, token: &Mutex<TokenInner>) -> Result<()> {
        let mut last_error = String::from("launchlab buy failed");
        for attempt in 1..=3 {
            let (slippage, lamports, token_program, mint, pool_key, venue) = {
                let mut state = token.lock().await;
                if attempt > 1 {
                    if let Some(price) = state.prices.current_mark_price {
                        state.prices.expected_entry_price = Some(price);
                    }
                }
                let token_program = state
                    .descriptor
                    .token_program
                    .clone()
                    .unwrap_or_else(|| crate::venues::ids::TOKEN_PROGRAM.to_string());
                (
                    state.buy_slippage_bps.unwrap_or(self.buy_slippage_bps),
                    state.buy_lamports.unwrap_or(self.buy_lamports),
                    token_program,
                    state.descriptor.mint.clone(),
                    state.descriptor.pool.clone(),
                    state.descriptor.venue.clone(),
                )
            };
            let Some(quote_mint) = self.alt_non_sol_quote(token).await else {
                last_error = "alt-quote mint missing".to_string();
                break;
            };
            let owner = self.wallet.pubkey();
            let token_program = match token_program.parse::<Pubkey>() {
                Ok(program) => program,
                Err(err) => {
                    last_error = err.to_string();
                    continue;
                }
            };
            let pool = match self.launchpad_pool(&mint, &pool_key).await {
                Ok(pool) => pool,
                Err(err) => {
                    last_error = err.to_string();
                    tracing::warn!(attempt, mint = %mint, error = %last_error, "[05 ENTRY] Buy attempt failed");
                    continue;
                }
            };
            let (jupiter, quote_program, quote_atoms) = match self
                .quotes
                .funded_quote(&owner, &quote_mint, lamports)
                .await
            {
                Ok(funded) => funded,
                Err(err) => {
                    last_error = err.to_string();
                    tracing::warn!(attempt, mint = %mint, error = %last_error, "[05 ENTRY] Buy attempt failed");
                    if is_non_retryable_buy_error(&last_error, &venue) {
                        let mut state = token.lock().await;
                        self.reject_buy(&mut state, "buy_slippage_rejected", &last_error)
                            .await;
                        return Ok(());
                    }
                    continue;
                }
            };
            let expected_tokens = launchpad::quote_buy_tokens(&pool, quote_atoms);
            let curve = match launchpad::buy_instructions(
                &owner,
                &pool,
                &token_program,
                &quote_program,
                quote_atoms,
                slippage,
            ) {
                Ok(ixs) => ixs,
                Err(err) => {
                    last_error = err.to_string();
                    tracing::warn!(attempt, mint = %mint, error = %last_error, "[05 ENTRY] Buy attempt failed");
                    continue;
                }
            };
            let legs = crate::alt_quote::AltBuyLegs {
                mint: mint.parse()?,
                token_program,
                quote_mint,
                quote_program,
                quote_atoms,
                curve_slippage_bps: slippage,
                jupiter,
                curve,
                compute_unit_limit: self
                    .venues
                    .compute_unit_limit
                    .max(crate::alt_quote::ALT_COMPUTE_UNIT_LIMIT),
                launchpad: true,
                expected_tokens,
            };
            tracing::info!(
                mint = %mint,
                quote = %quote_mint,
                spend_lamports = lamports,
                quote_atoms = legs.quote_atoms,
                "[05 ENTRY] Alt-quote buy: Jupiter SOL→quote + LaunchLab"
            );
            let blockhash = self.blockhashes.get().await?;
            let combo = compile_v1_ixs(
                &owner,
                &AltQuote::combo_instructions(&legs),
                blockhash,
                &self.wallet,
                legs.compute_unit_limit,
                self.venues.priority_fee_lamports,
                self.venues.tip_lamports,
            );
            let v1_failed = combo.is_err();
            let submitted = match combo {
                Ok(tx) => self.submit_compiled(token, tx, true, attempt, None).await,
                Err(err) => Err(err),
            };
            let submitted = match submitted {
                Ok(fill) => Ok(fill),
                Err(err) if v1_failed || combo_needs_split(&err) => {
                    tracing::info!(
                        mint = %mint,
                        error = %err,
                        "[05 ENTRY] LaunchLab combo too large; sending SOL→quote then curve"
                    );
                    match self.send_alt_buy_legs(token, &legs, attempt).await {
                        Ok(fill) => Ok(fill),
                        Err(err) => {
                            last_error = err.to_string();
                            tracing::warn!(attempt, mint = %mint, error = %last_error, "[05 ENTRY] Buy attempt failed");
                            break;
                        }
                    }
                }
                Err(err) => Err(err),
            };
            match submitted {
                Ok((_signature, _log_tokens, _log_quote, _raw_price)) => {
                    token.lock().await.alt_refill_signature = None;
                    self.finish_alt_buy(token, &legs, lamports).await?;
                    return Ok(());
                }
                Err(err) => {
                    last_error = err.to_string();
                    tracing::warn!(attempt, mint = %mint, error = %last_error, "[05 ENTRY] Buy attempt failed");
                    if is_non_retryable_buy_error(&last_error, &venue) {
                        let mut state = token.lock().await;
                        self.reject_buy(&mut state, "buy_slippage_rejected", &last_error)
                            .await;
                        return Ok(());
                    }
                }
            }
        }
        let mut state = token.lock().await;
        self.reject_buy(&mut state, "buy_failed", &last_error).await;
        Ok(())
    }

    async fn launchpad_pool(&self, mint: &str, pool_key: &str) -> Result<LaunchpadPoolState> {
        if let Some(pool) = self.venues.get_launchpad(mint) {
            return Ok(pool);
        }
        let key: Pubkey = pool_key.parse()?;
        let account = self.rpc.get_account(&key).await?;
        let state = LaunchpadPoolState::from_account_data(&key, &account.data)?;
        if state.completed() {
            self.venues.mark_curve_complete(mint);
        }
        self.venues.note_launchpad(mint, state);
        Ok(state)
    }

    async fn send_alt_buy_legs(
        &self,
        token: &Mutex<TokenInner>,
        legs: &crate::alt_quote::AltBuyLegs,
        attempt: u32,
    ) -> Result<(String, u64, u64, f64)> {
        let owner = self.wallet.pubkey();
        let blockhash = self.blockhashes.get().await?;
        let budget = "ComputeBudget111111111111111111111111111111"
            .parse()
            .expect("compute budget program");
        let jupiter_ixs: Vec<_> = legs
            .jupiter
            .instructions
            .iter()
            .filter(|ix| ix.program_id != budget)
            .cloned()
            .collect();
        let jupiter = compile_v0_ixs(
            &owner,
            &jupiter_ixs,
            &legs.jupiter.alts,
            blockhash,
            &self.wallet,
            legs.compute_unit_limit,
            self.venues.priority_fee_lamports,
            self.venues.tip_lamports,
        )?;
        let jupiter_sig = self.land_tx(jupiter).await.context("SOL→quote leg")?;
        token.lock().await.alt_refill_signature = Some(jupiter_sig.to_string());
        let mut last = anyhow!("curve buy failed");
        for curve_attempt in 1..=3 {
            let curve_ixs = if legs.launchpad {
                match self.venues.get_launchpad(&legs.mint.to_string()) {
                    Some(pool) => launchpad::buy_instructions(
                        &owner,
                        &pool,
                        &legs.token_program,
                        &legs.quote_program,
                        legs.quote_atoms,
                        legs.curve_slippage_bps,
                    )
                    .unwrap_or_else(|_| legs.curve.clone()),
                    None => legs.curve.clone(),
                }
            } else {
                match self.venues.get_curve(&legs.mint.to_string()) {
                    Some(curve) => pump_alt_buy_instructions(
                        &owner,
                        &legs.mint,
                        &legs.token_program,
                        &legs.quote_mint,
                        &legs.quote_program,
                        legs.quote_atoms,
                        legs.curve_slippage_bps,
                        &curve,
                    )
                    .unwrap_or_else(|_| legs.curve.clone()),
                    None => legs.curve.clone(),
                }
            };
            let blockhash = self.blockhashes.get().await?;
            let curve = match compile_v0_ixs(
                &owner,
                &curve_ixs,
                &[],
                blockhash,
                &self.wallet,
                legs.compute_unit_limit,
                self.venues.priority_fee_lamports,
                self.venues.tip_lamports,
            ) {
                Ok(tx) => tx,
                Err(err) => {
                    last = err;
                    tokio::time::sleep(Duration::from_millis(400)).await;
                    continue;
                }
            };
            match self.submit_compiled(token, curve, true, attempt, None).await {
                Ok(fill) => return Ok(fill),
                Err(err) => {
                    tracing::warn!(
                        curve_attempt,
                        error = %err,
                        "[05 ENTRY] Curve leg failed after SOL→quote"
                    );
                    last = err;
                    tokio::time::sleep(Duration::from_millis(400)).await;
                }
            }
        }
        Err(last).context("SOL→quote landed; curve buy failed")
    }

    async fn accept_buy_fill(
        &self,
        token: &Mutex<TokenInner>,
        token_amount: u64,
        sol_amount: u64,
        price: f64,
    ) -> Result<()> {
        let (buy_processed, buy_confirmed) = {
            let mut state = token.lock().await;
            state.actual_token_amount = Some(token_amount);
            state.actual_entry_sol_amount = Some(sol_amount);
            state.had_own_fill = true;
            state.prices.actual_entry_fill_price = Some(price);
            state.entry_processed_ms = Some(now_ms());
            state.transition(Lifecycle::BuyProcessed)?;
            state.transition(Lifecycle::PositionActiveUnconfirmed)?;
            let buy_processed = self.record_value(&state, "buy_processed", json!({ "fill": { "tokenAmount": token_amount.to_string(), "solAmount": sol_amount.to_string(), "price": price } }));
            state.transition(Lifecycle::BuyConfirmed)?;
            state.transition(Lifecycle::PositionActiveConfirmed)?;
            let buy_confirmed = self.record_value(&state, "buy_confirmed", json!({}));
            let strategy_price = crate::events::to_strategy_price(price);
            let missed = state.engine.on_buy_fill(
                if strategy_price > 0.0 {
                    strategy_price
                } else {
                    0.0
                },
                now_ms(),
            );
            if missed.is_some() {
                state.sell_after_fill = true;
            } else if state.sell_after_fill {
                state.engine.apply_missed_target_sell();
            }
            (buy_processed, buy_confirmed)
        };
        tracing::info!(value = %buy_processed, "{}", record_message("buy_processed"));
        tracing::info!(value = %buy_confirmed, "{}", record_message("buy_confirmed"));
        let journal = Arc::clone(&self.journal);
        tokio::spawn(async move {
            journal.record_critical(buy_processed).await;
            journal.record_critical(buy_confirmed).await;
        });
        Ok(())
    }

    /// Full alt-quote sell uses the token account now. A failed read keeps the stored count.
    async fn position_sell_amount(&self, token: &Mutex<TokenInner>) -> Option<u64> {
        let (tracked, mint, token_program, alt) = {
            let state = token.lock().await;
            let tracked = state.actual_token_amount?;
            let alt = (state.descriptor.venue == "pump" || state.descriptor.venue == "launchlab")
                && !crate::venues::ids::is_sol_quote(state.descriptor.quote_mint.as_deref());
            (
                tracked,
                state.descriptor.mint.clone(),
                state
                    .descriptor
                    .token_program
                    .clone()
                    .unwrap_or_else(|| crate::venues::ids::TOKEN_PROGRAM.to_string()),
                alt,
            )
        };
        if !alt {
            return Some(tracked);
        }
        let (Ok(mint), Ok(token_program)) = (mint.parse(), token_program.parse()) else {
            return Some(tracked);
        };
        match self
            .quotes
            .token_account_amount(&self.wallet.pubkey(), &mint, &token_program)
            .await
        {
            Ok(balance) => Some(balance),
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    %mint,
                    tracked,
                    "[08 EXIT] Token account unread; selling the stored token count"
                );
                Some(tracked)
            }
        }
    }

    /// Position size is the token-account balance. A failed read uses the curve's
    /// expected output. Take-profit uses that count against the quote the curve
    /// was told to spend. The closed-trade cost is the SOL sent into Jupiter.
    async fn finish_alt_buy(
        &self,
        token: &Mutex<TokenInner>,
        legs: &crate::alt_quote::AltBuyLegs,
        budget_lamports: u64,
    ) -> Result<()> {
        let token_amount = match self
            .quotes
            .token_account_amount(&self.wallet.pubkey(), &legs.mint, &legs.token_program)
            .await
        {
            Ok(balance) => balance,
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    mint = %legs.mint,
                    expected = legs.expected_tokens,
                    "[05 ENTRY] Token account unread; using the quoted token amount"
                );
                legs.expected_tokens
            }
        };
        let (curve_sol, price) = self
            .alt_entry_fill(
                &legs.quote_mint,
                legs.quote_atoms,
                budget_lamports,
                token_amount,
            )
            .await;
        tracing::info!(
            mint = %legs.mint,
            token_amount,
            curve_sol,
            cost_sol = budget_lamports,
            "[05 ENTRY] Alt-quote fill: token account size, curve SOL for take-profit, Jupiter budget for the close"
        );
        self.accept_buy_fill(token, token_amount, budget_lamports, price)
            .await
    }

    /// Take-profit baseline is the quote the curve was told to spend, in SOL.
    /// A missing price falls back to the SOL budget sent into Jupiter.
    async fn alt_entry_fill(
        &self,
        quote_mint: &Pubkey,
        quote_atoms: u64,
        budget_lamports: u64,
        token_amount: u64,
    ) -> (u64, f64) {
        let sol_amount = match self
            .quotes
            .curve_spend_lamports(quote_mint, quote_atoms)
            .await
        {
            Ok(lamports) => lamports,
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    quote = %quote_mint,
                    budget_lamports,
                    "[05 ENTRY] Alt-quote entry fell back to the SOL budget"
                );
                budget_lamports
            }
        };
        let price = if token_amount == 0 {
            0.0
        } else {
            sol_amount as f64 / token_amount as f64
        };
        (sol_amount, price)
    }

    async fn alt_curve_migrated(&self, token: &Mutex<TokenInner>) -> bool {
        let state = token.lock().await;
        if state.descriptor.venue != "pump" && state.descriptor.venue != "launchlab" {
            return false;
        }
        let Some(quote) = state.descriptor.quote_mint.as_deref() else {
            return false;
        };
        if crate::venues::ids::is_sol_quote(Some(quote)) {
            return false;
        }
        self.venues.is_curve_complete(&state.descriptor.mint)
    }

    /// A SOL bonding curve that has completed can no longer sell on the curve.
    async fn sol_curve_migrated(&self, token: &Mutex<TokenInner>) -> bool {
        let state = token.lock().await;
        if state.descriptor.venue != "pump" {
            return false;
        }
        if !crate::venues::ids::is_sol_quote(state.descriptor.quote_mint.as_deref()) {
            return false;
        }
        self.venues.is_curve_complete(&state.descriptor.mint)
    }

    /// Sell a completed SOL curve on its canonical wrapped-SOL PumpSwap pool.
    async fn send_migrated_pump_sell(
        self: &Arc<Self>,
        token: &Mutex<TokenInner>,
        reason: &str,
    ) -> Result<()> {
        let mut last_error = String::from("migrated pumpswap sell failed");
        for attempt in 1..=3 {
            let Some(amount) = self.position_sell_amount(token).await else {
                let mut state = token.lock().await;
                self.sell_failed(&mut state, reason, "position token amount is unknown")
                    .await;
                return Ok(());
            };
            if amount == 0 {
                let mut state = token.lock().await;
                self.close_empty_position(&mut state, reason, "position token amount is zero");
                return Ok(());
            }
            let (mint, token_program_s, slippage) = {
                let mut state = token.lock().await;
                if let Err(err) = state.begin_sell_attempt() {
                    last_error = err.to_string();
                    tracing::warn!(attempt, error = %last_error, "[08 EXIT] Sell attempt failed");
                    continue;
                }
                (
                    state.descriptor.mint.clone(),
                    state
                        .descriptor
                        .token_program
                        .clone()
                        .unwrap_or_else(|| crate::venues::ids::TOKEN_PROGRAM.to_string()),
                    state.sell_slippage_bps.unwrap_or(self.sell_slippage_bps),
                )
            };
            let base_program = match token_program_s.parse::<Pubkey>() {
                Ok(program) => program,
                Err(err) => {
                    last_error = err.to_string();
                    continue;
                }
            };
            let swap = match self.migrated_wsol_swap(&mint, &base_program).await {
                Ok(swap) => swap,
                Err(err) => {
                    last_error = err.to_string();
                    tracing::warn!(attempt, mint = %mint, error = %last_error, "[08 EXIT] PumpSwap pool not ready");
                    tokio::time::sleep(Duration::from_millis(400)).await;
                    continue;
                }
            };
            tracing::info!(
                mint = %mint,
                pool = %swap.pool,
                amount,
                "[08 EXIT] Curve complete, selling on PumpSwap"
            );
            let prepared = match build_amm_sell(
                &self.wallet.pubkey(),
                amount,
                slippage,
                &swap,
                self.venues.compute_unit_limit,
                self.venues.priority_fee_lamports,
                self.venues.tip_lamports,
            ) {
                Ok(prepared) => prepared,
                Err(err) => {
                    last_error = err.to_string();
                    tracing::warn!(attempt, error = %last_error, "[08 EXIT] Sell attempt failed");
                    continue;
                }
            };
            match self
                .submit_and_confirm(token, &prepared, false, attempt, Some(reason))
                .await
            {
                Ok((_signature, token_amount, sol_amount, price)) => {
                    self.finish_sell(token, reason, token_amount, sol_amount, price)
                        .await?;
                    return Ok(());
                }
                Err(err) => {
                    last_error = err.to_string();
                    token.lock().await.prepared_sell = None;
                    tracing::warn!(attempt, mint = %mint, error = %last_error, "[08 EXIT] Sell attempt failed");
                }
            }
        }
        let mut state = token.lock().await;
        self.sell_failed(&mut state, reason, &last_error).await;
        Ok(())
    }

    async fn migrated_wsol_swap(
        &self,
        mint: &str,
        base_program: &Pubkey,
    ) -> Result<crate::events::PumpSwapSnapshot> {
        let mint_key: Pubkey = mint.parse()?;
        let pool = crate::venues::ids::canonical_pool_pda(&mint_key, &crate::venues::ids::WSOL_MINT);
        let pool_account = self.rpc.get_account(&pool).await?;
        let parsed = crate::venues::ids::parse_amm_pool(&pool_account.data)
            .ok_or_else(|| anyhow!("unable to parse pumpswap pool for {mint}"))?;
        let global_account = self
            .rpc
            .get_account(&crate::venues::ids::amm_global_config())
            .await?;
        let global = crate::venues::ids::parse_amm_global_config(&global_account.data)
            .ok_or_else(|| anyhow!("unable to parse pumpswap global config"))?;
        let base_reserve = self
            .account_token_amount(&parsed.pool_base_token_account)
            .await?;
        let quote_reserve = self
            .account_token_amount(&parsed.pool_quote_token_account)
            .await?;
        let protocol = global
            .protocol_fee_recipients
            .into_iter()
            .find(|key| *key != Pubkey::default())
            .unwrap_or(global.protocol_fee_recipients[0]);
        let buyback = crate::venues::ids::PUMP_BUYBACK_FEE_RECIPIENTS
            .first()
            .copied()
            .unwrap_or_default();
        let swap = crate::events::PumpSwapSnapshot {
            pool: pool.to_string(),
            base_mint: parsed.base_mint.to_string(),
            quote_mint: parsed.quote_mint.to_string(),
            pool_base_token_account: parsed.pool_base_token_account.to_string(),
            pool_quote_token_account: parsed.pool_quote_token_account.to_string(),
            base_token_program: base_program.to_string(),
            quote_token_program: crate::venues::ids::TOKEN_PROGRAM.to_string(),
            base_reserve: base_reserve as u128,
            quote_reserve: quote_reserve as u128,
            virtual_quote_reserves: parsed.virtual_quote_reserves,
            lp_fee_bps: global.lp_fee_bps as u128,
            protocol_fee_bps: global.protocol_fee_bps as u128,
            coin_creator_fee_bps: global.coin_creator_fee_bps as u128,
            coin_creator: parsed.coin_creator.to_string(),
            protocol_fee_recipient: protocol.to_string(),
            buyback_fee_recipient: buyback.to_string(),
            cashback: parsed.cashback,
        };
        self.venues.note_swap(mint, swap.clone());
        Ok(swap)
    }

    async fn account_token_amount(&self, account: &Pubkey) -> Result<u64> {
        let data = self.rpc.get_account_data(account).await?;
        if data.len() < 72 {
            return Err(anyhow!("token account {account} is too short"));
        }
        Ok(u64::from_le_bytes(data[64..72].try_into()?))
    }

    /// SOL-quoted LaunchLab sell. A completed pool leaves through Jupiter.
    async fn send_sol_launchpad_sell(
        self: &Arc<Self>,
        token: &Mutex<TokenInner>,
        reason: &str,
    ) -> Result<()> {
        let mut last_error = String::from("launchlab sell failed");
        for attempt in 1..=3 {
            let Some(amount) = self.position_sell_amount(token).await else {
                let mut state = token.lock().await;
                self.sell_failed(&mut state, reason, "position token amount is unknown")
                    .await;
                return Ok(());
            };
            if amount == 0 {
                let mut state = token.lock().await;
                self.close_empty_position(&mut state, reason, "position token amount is zero");
                return Ok(());
            }
            let (slippage, token_program_s, mint, pool_key) = {
                let state = token.lock().await;
                (
                    state.sell_slippage_bps.unwrap_or(self.sell_slippage_bps),
                    state
                        .descriptor
                        .token_program
                        .clone()
                        .unwrap_or_else(|| crate::venues::ids::TOKEN_PROGRAM.to_string()),
                    state.descriptor.mint.clone(),
                    state.descriptor.pool.clone(),
                )
            };
            let token_program = match token_program_s.parse::<Pubkey>() {
                Ok(program) => program,
                Err(err) => {
                    last_error = err.to_string();
                    continue;
                }
            };
            let pool = match self.launchpad_pool(&mint, &pool_key).await {
                Ok(pool) => pool,
                Err(err) => {
                    last_error = err.to_string();
                    tracing::warn!(attempt, mint = %mint, error = %last_error, "[08 EXIT] Sell attempt failed");
                    continue;
                }
            };
            if pool.completed() {
                self.venues.mark_curve_complete(&mint);
                tracing::info!(mint = %mint, "[08 EXIT] LaunchLab pool complete, Jupiter mint→SOL");
                return self.send_alt_migrated_sell(token, reason).await;
            }
            let quote_program = *crate::venues::ids::TOKEN_PROGRAM;
            let instructions = match launchpad::sell_instructions(
                &self.wallet.pubkey(),
                &pool,
                &token_program,
                &quote_program,
                amount,
                slippage,
            ) {
                Ok(ixs) => ixs,
                Err(err) => {
                    last_error = err.to_string();
                    tracing::warn!(attempt, error = %last_error, "[08 EXIT] Sell attempt failed");
                    continue;
                }
            };
            let prepared = crate::state::PreparedTrade {
                instructions,
                payer: self.wallet.pubkey(),
                compute_unit_limit: self.venues.compute_unit_limit,
                priority_fee_lamports: self.venues.priority_fee_lamports,
                tip_lamports: self.venues.tip_lamports,
            };
            {
                let mut state = token.lock().await;
                if let Err(err) = state.begin_sell_attempt() {
                    last_error = err.to_string();
                    tracing::warn!(attempt, error = %last_error, "[08 EXIT] Sell attempt failed");
                    continue;
                }
                state.prepared_sell = Some(prepared.clone());
            }
            tracing::info!(mint = %mint, amount, "[08 EXIT] LaunchLab sell: mint→SOL");
            match self
                .submit_and_confirm(token, &prepared, false, attempt, Some(reason))
                .await
            {
                Ok((_signature, token_amount, sol_amount, price)) => {
                    self.spawn_unwrap_wsol();
                    self.finish_sell(token, reason, token_amount, sol_amount, price)
                        .await?;
                    return Ok(());
                }
                Err(err) => {
                    last_error = err.to_string();
                    token.lock().await.prepared_sell = None;
                    tracing::warn!(attempt, mint = %mint, error = %last_error, "[08 EXIT] Sell attempt failed");
                }
            }
        }
        let mut state = token.lock().await;
        self.sell_failed(&mut state, reason, &last_error).await;
        Ok(())
    }

    /// Curve already migrated: sell mint→SOL on Jupiter, same as the plugin fallback.
    async fn send_alt_migrated_sell(
        self: &Arc<Self>,
        token: &Mutex<TokenInner>,
        reason: &str,
    ) -> Result<()> {
        let mut last_error = String::from("migrated sell failed");
        for attempt in 1..=3 {
            let Some(amount) = self.position_sell_amount(token).await else {
                let mut state = token.lock().await;
                self.sell_failed(&mut state, reason, "position token amount is unknown")
                    .await;
                return Ok(());
            };
            if amount == 0 {
                let mut state = token.lock().await;
                self.close_empty_position(&mut state, reason, "position token amount is zero");
                return Ok(());
            }
            let (mint, quote_mint) = {
                let mut state = token.lock().await;
                if let Err(err) = state.begin_sell_attempt() {
                    last_error = err.to_string();
                    tracing::warn!(attempt, error = %last_error, "[08 EXIT] Sell attempt failed");
                    continue;
                }
                (
                    state.descriptor.mint.clone(),
                    state.descriptor.quote_mint.clone(),
                )
            };
            let mint_key: Pubkey = match mint.parse() {
                Ok(mint) => mint,
                Err(err) => {
                    last_error = err.to_string();
                    continue;
                }
            };
            tracing::info!(
                mint = %mint,
                amount,
                "[08 EXIT] Alt-quote sell: curve complete, Jupiter mint→SOL"
            );
            let build = match self
                .quotes
                .flatten_mint_to_sol(&self.wallet.pubkey(), &mint_key, amount)
                .await
            {
                Ok(build) => build,
                Err(err) => {
                    last_error = err.to_string();
                    tracing::warn!(attempt, mint = %mint, error = %last_error, "[08 EXIT] Sell attempt failed");
                    continue;
                }
            };
            let budget = "ComputeBudget111111111111111111111111111111"
                .parse()
                .expect("compute budget program");
            let instructions: Vec<_> = build
                .instructions
                .iter()
                .filter(|ix| ix.program_id != budget)
                .cloned()
                .collect();
            let blockhash = self.blockhashes.get().await?;
            let tx = match compile_v0_ixs(
                &self.wallet.pubkey(),
                &instructions,
                &build.alts,
                blockhash,
                &self.wallet,
                self.venues
                    .compute_unit_limit
                    .max(crate::alt_quote::ALT_COMPUTE_UNIT_LIMIT),
                self.venues.priority_fee_lamports,
                self.venues.tip_lamports,
            ) {
                Ok(tx) => tx,
                Err(err) => {
                    last_error = err.to_string();
                    tracing::warn!(attempt, error = %last_error, "[08 EXIT] Sell attempt failed");
                    continue;
                }
            };
            match self
                .submit_compiled(token, tx, false, attempt, Some(reason))
                .await
            {
                Ok((_signature, token_amount, parsed_sol, parsed_price)) => {
                    let sol_amount = if build.out_amount > 0 {
                        build.out_amount
                    } else {
                        parsed_sol
                    };
                    let price = if token_amount > 0 {
                        sol_amount as f64 / token_amount as f64
                    } else {
                        parsed_price
                    };
                    if let Some(quote) = quote_mint.and_then(|q| q.parse::<Pubkey>().ok()) {
                        let quote_s = quote.to_string();
                        if crate::venues::ids::is_sol_quote(Some(&quote_s)) {
                            self.spawn_unwrap_wsol();
                        } else {
                            self.spawn_quote_reclaim(quote);
                        }
                    }
                    self.finish_sell(token, reason, token_amount, sol_amount, price)
                        .await?;
                    return Ok(());
                }
                Err(err) => {
                    last_error = err.to_string();
                    token.lock().await.prepared_sell = None;
                    tracing::warn!(attempt, mint = %mint, error = %last_error, "[08 EXIT] Sell attempt failed");
                }
            }
        }
        let mut state = token.lock().await;
        self.sell_failed(&mut state, reason, &last_error).await;
        Ok(())
    }

    pub async fn send_sell(
        self: &Arc<Self>,
        token: &Mutex<TokenInner>,
        reason: &str,
    ) -> Result<()> {
        if self.alt_curve_migrated(token).await {
            return self.send_alt_migrated_sell(token, reason).await;
        }
        if self.sol_curve_migrated(token).await {
            return self.send_migrated_pump_sell(token, reason).await;
        }
        if token.lock().await.descriptor.venue == "launchlab"
            && self.alt_non_sol_quote(token).await.is_none()
        {
            return self.send_sol_launchpad_sell(token, reason).await;
        }
        let mut last_error = String::from("sell failed");
        let mut stop_retry = false;
        for attempt in 1..=3 {
            let Some(amount) = self.position_sell_amount(token).await else {
                let mut state = token.lock().await;
                self.sell_failed(&mut state, reason, "position token amount is unknown")
                    .await;
                return Ok(());
            };
            if amount == 0 {
                let mut state = token.lock().await;
                self.close_empty_position(&mut state, reason, "position token amount is zero");
                return Ok(());
            }
            let (slippage, token_program_s, venue, mint) = {
                let state = token.lock().await;
                (
                    state.sell_slippage_bps.unwrap_or(self.sell_slippage_bps),
                    state
                        .descriptor
                        .token_program
                        .clone()
                        .unwrap_or_else(|| crate::venues::ids::TOKEN_PROGRAM.to_string()),
                    state.descriptor.venue.clone(),
                    state.descriptor.mint.clone(),
                )
            };
            let token_program: Pubkey = match token_program_s.parse() {
                Ok(program) => program,
                Err(err) => {
                    last_error = err.to_string();
                    tracing::warn!(attempt, error = %last_error, "[08 EXIT] Sell attempt failed");
                    continue;
                }
            };
            let alt_quote = self.alt_non_sol_quote(token).await;
            let mut expected_quote = 0u64;
            let built = if venue == "launchlab" {
                let Some(quote_mint) = alt_quote else {
                    last_error = "launchlab quote mint missing".to_string();
                    continue;
                };
                let quote_program = match self.quotes.token_program(&quote_mint).await {
                    Ok(program) => program,
                    Err(err) => {
                        last_error = err.to_string();
                        tracing::warn!(attempt, error = %last_error, "[08 EXIT] Sell attempt failed");
                        continue;
                    }
                };
                let pool_key = token.lock().await.descriptor.pool.clone();
                let pool = match self.launchpad_pool(&mint, &pool_key).await {
                    Ok(pool) => pool,
                    Err(err) => {
                        last_error = err.to_string();
                        tracing::warn!(attempt, error = %last_error, "[08 EXIT] Sell attempt failed");
                        continue;
                    }
                };
                if pool.completed() {
                    self.venues.mark_curve_complete(&mint);
                    return self.send_alt_migrated_sell(token, reason).await;
                }
                expected_quote = launchpad::quote_sell_quote(&pool, amount).max(1);
                tracing::info!(
                    mint = %mint,
                    quote = %quote_mint,
                    "[08 EXIT] Alt-quote sell: LaunchLab mint→quote"
                );
                launchpad::sell_instructions(
                    &self.wallet.pubkey(),
                    &pool,
                    &token_program,
                    &quote_program,
                    amount,
                    slippage,
                )
                .map(|instructions| crate::state::PreparedTrade {
                    instructions,
                    payer: self.wallet.pubkey(),
                    compute_unit_limit: self.venues.compute_unit_limit,
                    priority_fee_lamports: self.venues.priority_fee_lamports,
                    tip_lamports: self.venues.tip_lamports,
                })
            } else if let Some(quote_mint) = alt_quote {
                let quote_program = match self.quotes.token_program(&quote_mint).await {
                    Ok(program) => program,
                    Err(err) => {
                        last_error = err.to_string();
                        tracing::warn!(attempt, error = %last_error, "[08 EXIT] Sell attempt failed");
                        continue;
                    }
                };
                let Some(curve) = self.venues.get_curve(&mint) else {
                    last_error = format!("no streamed curve for {mint}");
                    tracing::warn!(attempt, error = %last_error, "[08 EXIT] Sell attempt failed");
                    continue;
                };
                expected_quote =
                    (crate::venues::quote::quote_sell_sol(amount as u128, &curve) as u64).max(1);
                tracing::info!(
                    mint = %mint,
                    quote = %quote_mint,
                    "[08 EXIT] Alt-quote sell: curve mint→quote"
                );
                build_pump_sell_alt(
                    &self.wallet.pubkey(),
                    &match mint.parse() {
                        Ok(mint) => mint,
                        Err(err) => {
                            last_error = err.to_string();
                            continue;
                        }
                    },
                    &token_program,
                    &quote_mint,
                    &quote_program,
                    amount,
                    slippage,
                    &curve,
                    self.venues.compute_unit_limit,
                    self.venues.priority_fee_lamports,
                    self.venues.tip_lamports,
                )
            } else {
                self.venues.build_sell(
                    &venue,
                    &self.wallet.pubkey(),
                    &mint,
                    &token_program,
                    amount,
                    slippage,
                )
            };
            let prepared = {
                let mut state = token.lock().await;
                match built {
                    Ok(built) => state.prepared_sell = Some(built),
                    Err(err) => {
                        last_error = err.to_string();
                        state.prepared_sell = None;
                        tracing::warn!(attempt, error = %last_error, "[08 EXIT] Sell attempt failed");
                        continue;
                    }
                }
                if let Err(err) = state.begin_sell_attempt() {
                    last_error = err.to_string();
                    tracing::warn!(attempt, error = %last_error, "[08 EXIT] Sell attempt failed");
                    continue;
                }
                state.prepared_sell.clone()
            };
            let Some(prepared) = prepared else { continue };
            match self
                .submit_and_confirm(token, &prepared, false, attempt, Some(reason))
                .await
            {
                Ok((_signature, token_amount, mut sol_amount, mut price)) => {
                    if let Some(quote_mint) = alt_quote {
                        match self
                            .quotes
                            .curve_spend_lamports(&quote_mint, expected_quote)
                            .await
                        {
                            Ok(lamports) => {
                                sol_amount = lamports;
                                price = if token_amount == 0 {
                                    0.0
                                } else {
                                    lamports as f64 / token_amount as f64
                                };
                            }
                            Err(err) => {
                                tracing::warn!(
                                    error = %err,
                                    quote = %quote_mint,
                                    "[08 EXIT] Alt-quote sell proceeds are 0; FX missing"
                                );
                                sol_amount = 0;
                                price = 0.0;
                            }
                        }
                        self.spawn_quote_reclaim(quote_mint);
                    }
                    self.finish_sell(token, reason, token_amount, sol_amount, price)
                        .await?;
                    return Ok(());
                }
                Err(err) => {
                    last_error = err.to_string();
                    let mut state = token.lock().await;
                    state.prepared_sell = None;
                    let venue = state.descriptor.venue.clone();
                    let mint = state.descriptor.mint.clone();
                    tracing::warn!(attempt, mint = %mint, error = %last_error, "[08 EXIT] Sell attempt failed");
                    if !is_empty_sell_error(&last_error, &venue) {
                        continue;
                    }
                    drop(state);
                    match self.wallet_token_amount(&mint).await {
                        Ok(0) => {
                            let mut state = token.lock().await;
                            self.close_empty_position(&mut state, reason, &last_error);
                            return Ok(());
                        }
                        Ok(balance) => {
                            let mut state = token.lock().await;
                            let current = state.actual_token_amount.unwrap_or(0);
                            if balance < current {
                                tracing::warn!(
                                    mint = %mint,
                                    balance,
                                    requested = current,
                                    "[08 EXIT] Sell amount was above the wallet balance; retrying the on-chain amount"
                                );
                                state.actual_token_amount = Some(balance);
                                continue;
                            }
                            self.close_empty_position(&mut state, reason, &last_error);
                            return Ok(());
                        }
                        Err(balance_err) => {
                            // Another identical sell would land and pay the priority fee again.
                            tracing::error!(
                                mint = %mint,
                                error = %balance_err,
                                "[08 EXIT] Could not read the token balance after NotEnoughTokensToSell; not resubmitting"
                            );
                            stop_retry = true;
                            break;
                        }
                    }
                }
            }
        }
        let mut state = token.lock().await;
        if stop_retry {
            if matches!(
                state.lifecycle,
                Lifecycle::SellPrepared | Lifecycle::SellSent
            ) {
                let _ = state.transition(Lifecycle::PositionActiveConfirmed);
            }
            state.release_sell_send();
            state.prepared_sell = None;
            return Ok(());
        }
        self.sell_failed(&mut state, reason, &last_error).await;
        Ok(())
    }

    /// Submit first, journal `*_sent` with the signature, then confirm (bot_target order).
    async fn submit_and_confirm(
        &self,
        token: &Mutex<TokenInner>,
        prepared: &crate::state::PreparedTrade,
        is_buy: bool,
        attempt: u32,
        sell_reason: Option<&str>,
    ) -> Result<(String, u64, u64, f64)> {
        let blockhash = self.blockhashes.get().await?;
        let tx = compile_trade(prepared, blockhash, &self.wallet)?;
        self.submit_compiled(token, tx, is_buy, attempt, sell_reason)
            .await
    }

    async fn submit_compiled(
        &self,
        token: &Mutex<TokenInner>,
        tx: VersionedTransaction,
        is_buy: bool,
        attempt: u32,
        sell_reason: Option<&str>,
    ) -> Result<(String, u64, u64, f64)> {
        let raw = transaction_wire_bytes(&tx)?;
        let encoded = base64::engine::general_purpose::STANDARD.encode(&raw);
        let (signature, fallback) = match self.submit(&encoded, &tx).await {
            Ok(pair) => pair,
            Err(err) => return Err(err),
        };
        let (mint, venue, sent_record) = {
            let mut state = token.lock().await;
            let (event, extra) = if is_buy {
                state.buy_signature = Some(signature.clone());
                state.transition(Lifecycle::BuySent)?;
                (
                    "buy_sent",
                    json!({ "attempt": attempt, "signature": signature, "fallback": fallback }),
                )
            } else {
                state.sell_signature = Some(signature.clone());
                (
                    "sell_sent",
                    json!({ "attempt": attempt, "reason": sell_reason.unwrap_or(""), "signature": signature, "fallback": fallback }),
                )
            };
            let value = self.record_value(&state, event, extra);
            (
                state.descriptor.mint.clone(),
                state.descriptor.venue.clone(),
                value,
            )
        };
        tracing::info!(value = %sent_record, "{}", record_message(if is_buy { "buy_sent" } else { "sell_sent" }));
        // Disk flush used to hold the token lock and run before confirmation polling.
        let journal = Arc::clone(&self.journal);
        let flush = async move { journal.record_critical(sent_record).await };
        let ((), confirmed) = tokio::join!(
            flush,
            self.confirm(token, &signature, &mint, &venue, fallback)
        );
        let (token_amount, sol_amount, price) = confirmed?;
        if is_buy && token_amount == 0 {
            return Err(anyhow!("buy fill could not be determined"));
        }
        if !is_buy && token_amount == 0 && sol_amount == 0 {
            return Err(anyhow!("sell fill could not be determined"));
        }
        Ok((signature, token_amount, sol_amount, price))
    }

    async fn land_tx(&self, tx: VersionedTransaction) -> Result<Signature> {
        let raw = transaction_wire_bytes(&tx)?;
        let encoded = base64::engine::general_purpose::STANDARD.encode(&raw);
        let (signature, _) = self.submit(&encoded, &tx).await?;
        let parsed = Signature::from_str(&signature)?;
        let deadline = Instant::now() + Duration::from_secs(25);
        loop {
            if Instant::now() >= deadline {
                return Err(anyhow!("transaction {signature} confirm timed out"));
            }
            match self.rpc.get_signature_status(&parsed).await {
                Ok(Some(Ok(()))) => return Ok(parsed),
                Ok(Some(Err(err))) => {
                    return Err(anyhow!("transaction {signature} failed on-chain: {err:?}"));
                }
                _ => tokio::time::sleep(Duration::from_millis(250)).await,
            }
        }
    }

    async fn finish_sell(
        &self,
        token: &Mutex<TokenInner>,
        reason: &str,
        token_amount: u64,
        sol_amount: u64,
        price: f64,
    ) -> Result<()> {
        let (closed, pnl, mint, wallet_pnl) = {
            let mut state = token.lock().await;
            state.prices.actual_exit_fill_price = Some(price);
            if state.lifecycle == Lifecycle::SellPrepared {
                state.transition(Lifecycle::SellSent)?;
            }
            let closed = self.record_value(
                &state,
                "position_closed",
                json!({ "reason": reason }),
            );
            let closed_at = now_ms();
            let pnl = if let (Some(entry_sol), Some(entry_price), Some(entry_ms)) = (
                state.actual_entry_sol_amount,
                state.prices.actual_entry_fill_price,
                state.entry_processed_ms,
            ) {
                Some(ClosedTradePnlInput {
                    closed_at_ms: closed_at,
                    descriptor: state.descriptor.clone(),
                    buy_lamports: entry_sol,
                    sell_lamports: sol_amount,
                    token_amount,
                    entry_price,
                    exit_price: price,
                    prices: state.prices.clone(),
                    exit_reason: reason.to_string(),
                    buy_signature: state.buy_signature.clone(),
                    sell_signature: state.sell_signature.clone(),
                    entry_processed_ms: entry_ms,
                    wallet_pnl_lamports: None,
                })
            } else {
                None
            };
            let wallet_pnl = if state.alt_refill_signature.is_some()
                || !crate::venues::ids::is_sol_quote(state.descriptor.quote_mint.as_deref())
                    && (state.descriptor.venue == "pump" || state.descriptor.venue == "launchlab")
            {
                Some((
                    state.buy_signature.clone(),
                    state.sell_signature.clone(),
                    state.alt_refill_signature.clone(),
                ))
            } else {
                None
            };
            state.engine.on_sell_fill();
            state.sell_after_fill = false;
            state.clear_filled_position();
            state.reset_buy_send_claim();
            state.reset_sell_send_claim();
            state.transition(Lifecycle::TrackingPool)?;
            let mint = state.descriptor.mint.clone();
            tracing::info!(mint = %mint, "[REENTRY] Round finished; still watching for the next target buy");
            (closed, pnl, mint, wallet_pnl)
        };
        tracing::info!(value = %closed, "{}", record_message("position_closed"));
        let journal = Arc::clone(&self.journal);
        let pnl_journal = Arc::clone(&self.pnl);
        let rpc = Arc::clone(&self.rpc);
        let wallet = self.wallet.pubkey();
        tokio::spawn(async move {
            journal.record_critical(closed).await;
            if let Some(mut input) = pnl {
                if let Some((buy_sig, sell_sig, refill_sig)) = wallet_pnl {
                    match wallet_roundtrip_lamports(
                        &rpc,
                        &wallet,
                        buy_sig.as_deref(),
                        sell_sig.as_deref(),
                        refill_sig.as_deref(),
                    )
                    .await
                    {
                        Some(net) => {
                            tracing::info!(
                                mint = %mint,
                                wallet_pnl_lamports = net,
                                "[PNL] Closed trade uses wallet lamports"
                            );
                            input.wallet_pnl_lamports = Some(net);
                        }
                        None => {
                            tracing::warn!(
                                mint = %mint,
                                "[PNL] Wallet lamports incomplete; keeping the quote-book close"
                            );
                        }
                    }
                }
                match pnl_journal.record(input).await {
                    Ok(()) => {
                        tracing::info!(mint = %mint, "[PNL] Closed trade appended")
                    }
                    Err(err) => {
                        tracing::error!(error = %err, mint = %mint, "[PNL] Failed to append closed trade")
                    }
                }
                let _ = create_pnl_record;
            }
        });
        Ok(())
    }

    fn spawn_quote_reclaim(self: &Arc<Self>, quote_mint: Pubkey) {
        let execution = Arc::clone(self);
        tokio::spawn(async move {
            tokio::time::sleep(RECLAIM_DELAY).await;
            execution.reclaim_quote_pass(&quote_mint).await;
        });
    }

    /// Dropped mint with no open position: swap the quote now, then once more 8 seconds later.
    pub fn spawn_abandon_reclaim(self: &Arc<Self>, quote_mint: Pubkey) {
        let execution = Arc::clone(self);
        tokio::spawn(async move {
            execution.reclaim_quote_pass(&quote_mint).await;
            tokio::time::sleep(ABANDON_RECLAIM_DELAY).await;
            execution.reclaim_quote_pass(&quote_mint).await;
        });
    }

    async fn reclaim_quote_pass(&self, quote_mint: &Pubkey) {
        for attempt in 1..=3 {
            match self.reclaim_quote_once(quote_mint).await {
                Ok(()) => break,
                Err(err) => {
                    tracing::warn!(
                        attempt,
                        error = %err,
                        quote = %quote_mint,
                        "[08 EXIT] Quote reclaim attempt failed"
                    );
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            }
        }
        if let Err(err) = self.unwrap_wsol().await {
            tracing::warn!(error = %err, "[08 EXIT] wSOL unwrap after reclaim failed");
        }
    }

    async fn reclaim_quote_once(&self, quote_mint: &Pubkey) -> Result<()> {
        let Some(build) = self
            .quotes
            .reclaim_build(&self.wallet.pubkey(), quote_mint)
            .await?
        else {
            tracing::info!(quote = %quote_mint, "[08 EXIT] Quote ATA already empty");
            return Ok(());
        };
        let blockhash = self.blockhashes.get().await?;
        let budget: Pubkey = "ComputeBudget111111111111111111111111111111"
            .parse()
            .expect("compute budget program");
        let instructions: Vec<_> = build
            .instructions
            .iter()
            .filter(|ix| ix.program_id != budget)
            .cloned()
            .collect();
        let tx = compile_v0_ixs(
            &self.wallet.pubkey(),
            &instructions,
            &build.alts,
            blockhash,
            &self.wallet,
            self.venues
                .compute_unit_limit
                .max(crate::alt_quote::ALT_COMPUTE_UNIT_LIMIT),
            self.venues.priority_fee_lamports,
            self.venues.tip_lamports,
        )?;
        let signature = self.land_tx(tx).await?;
        tracing::info!(
            quote = %quote_mint,
            %signature,
            "[08 EXIT] Reclaimed quote to SOL"
        );
        Ok(())
    }

    fn spawn_unwrap_wsol(self: &Arc<Self>) {
        let execution = Arc::clone(self);
        tokio::spawn(async move {
            if let Err(err) = execution.unwrap_wsol().await {
                tracing::warn!(error = %err, "[08 EXIT] wSOL unwrap failed");
            }
        });
    }

    async fn unwrap_wsol(&self) -> Result<()> {
        let owner = self.wallet.pubkey();
        let wsol = *crate::venues::ids::WSOL_MINT;
        let wsol_ata = crate::venues::ids::ata(&owner, &wsol, &crate::venues::ids::TOKEN_PROGRAM);
        if self.rpc.get_account(&wsol_ata).await.is_err() {
            return Ok(());
        }
        let close = solana_sdk::instruction::Instruction {
            program_id: *crate::venues::ids::TOKEN_PROGRAM,
            accounts: vec![
                solana_sdk::instruction::AccountMeta {
                    pubkey: wsol_ata,
                    is_signer: false,
                    is_writable: true,
                },
                solana_sdk::instruction::AccountMeta {
                    pubkey: owner,
                    is_signer: false,
                    is_writable: true,
                },
                solana_sdk::instruction::AccountMeta {
                    pubkey: owner,
                    is_signer: true,
                    is_writable: false,
                },
            ],
            data: vec![9],
        };
        let blockhash = self.blockhashes.get().await?;
        let tx = compile_v0_ixs(
            &owner,
            &[close],
            &[],
            blockhash,
            &self.wallet,
            40_000,
            self.venues.priority_fee_lamports,
            0,
        )?;
        let signature = self.land_tx(tx).await?;
        tracing::info!(%signature, "[08 EXIT] Unwrapped leftover wSOL");
        Ok(())
    }

    async fn submit(
        &self,
        encoded: &str,
        transaction: &VersionedTransaction,
    ) -> Result<(String, bool)> {
        let mut last_error = anyhow!("sender failed");
        for attempt in 1..=3 {
            match self.sender.send(encoded, mono_ms()).await {
                Ok((signature, _)) => return Ok((signature, false)),
                Err(err) => {
                    tracing::warn!(attempt, error = %err, "[06 CONFIRM] Helius submission attempt failed");
                    let unavailable = err.downcast_ref::<HeliusSenderUnavailable>().is_some();
                    last_error = err;
                    if unavailable {
                        break;
                    }
                }
            }
        }
        match self
            .rpc
            .send_transaction_with_config(
                transaction,
                RpcSendTransactionConfig {
                    skip_preflight: false,
                    max_retries: Some(2),
                    ..Default::default()
                },
            )
            .await
        {
            Ok(signature) => Ok((signature.to_string(), true)),
            Err(err) => Err(anyhow!(
                "Sender and RPC fallback both failed: {last_error}; {err}"
            )),
        }
    }

    async fn confirm(
        &self,
        token: &Mutex<TokenInner>,
        signature: &str,
        mint: &str,
        venue: &str,
        fallback: bool,
    ) -> Result<(u64, u64, f64)> {
        // If status was seen but fill lagged, re-poll up to 3 outer rounds.
        let mut last_err = anyhow!("confirmation timed out for {signature}");
        for poll in 1..=3 {
            match self
                .wait_processed(token, signature, mint, venue, fallback)
                .await
            {
                Ok(fill) => return Ok(fill),
                Err(err) => {
                    let seen = err
                        .downcast_ref::<ConfirmationTimeoutError>()
                        .map(|e| e.seen)
                        .unwrap_or(false);
                    last_err = err;
                    if !seen || poll == 3 {
                        break;
                    }
                    tracing::warn!(
                        poll,
                        signature,
                        "[06 CONFIRM] Seen but fill missing; re-polling"
                    );
                }
            }
        }
        Err(last_err)
    }

    async fn wait_processed(
        &self,
        token: &Mutex<TokenInner>,
        signature: &str,
        mint: &str,
        venue: &str,
        fallback: bool,
    ) -> Result<(u64, u64, f64)> {
        let signature_parsed = Signature::from_str(signature)?;
        let started = Instant::now();
        let deadline = Duration::from_secs(30);
        let unseen_deadline = if fallback {
            Duration::from_secs(10)
        } else {
            deadline
        };
        let notify = token.lock().await.own_fill_notify.clone();
        let mut seen = false;
        while started.elapsed() < deadline {
            if let Some(fill) = token.lock().await.own_fill(signature) {
                return Ok(fill);
            }
            let signatures = [signature_parsed];
            let pending = self.rpc.get_signature_statuses_with_history(&signatures);
            tokio::pin!(pending);
            let statuses = tokio::select! {
                _ = notify.notified() => {
                    if let Some(fill) = token.lock().await.own_fill(signature) {
                        return Ok(fill);
                    }
                    continue;
                }
                result = &mut pending => result?,
            };
            if let Some(Some(status)) = statuses.value.first() {
                seen = true;
                if let Some(err) = &status.err {
                    return Err(anyhow!("transaction failed: {err:?}"));
                }
                if matches!(
                    status.confirmation_status,
                    Some(
                        TransactionConfirmationStatus::Confirmed
                            | TransactionConfirmationStatus::Finalized
                    )
                ) {
                    let fetch = self.rpc.get_transaction_with_config(
                        &signature_parsed,
                        RpcTransactionConfig {
                            encoding: Some(UiTransactionEncoding::Json),
                            commitment: Some(CommitmentConfig::confirmed()),
                            max_supported_transaction_version: Some(1),
                        },
                    );
                    tokio::pin!(fetch);
                    match tokio::select! {
                        _ = notify.notified() => {
                            if let Some(fill) = token.lock().await.own_fill(signature) {
                                return Ok(fill);
                            }
                            continue;
                        }
                        result = &mut fetch => result,
                    } {
                        Ok(tx) => {
                            if let Some(fill) =
                                parse_fill_from_tx(&tx, &self.wallet.pubkey(), mint, venue)
                            {
                                return Ok(fill);
                            }
                            // Confirmed but fill not parseable yet — keep polling.
                        }
                        Err(err) => {
                            tracing::debug!(error = %err, signature, "[06 CONFIRM] getTransaction lagged");
                        }
                    }
                }
            } else if !seen && started.elapsed() >= unseen_deadline {
                break;
            }
            tokio::select! {
                _ = notify.notified() => {}
                _ = tokio::time::sleep(confirm_backoff(started.elapsed())) => {}
            }
        }
        Err(ConfirmationTimeoutError {
            signature: signature.to_string(),
            seen,
        }
        .into())
    }

    /// Recover fill from a landed signature (bot_target getTransaction + parseFill).
    pub async fn fill_from_signature(
        &self,
        signature: &str,
        mint: &str,
        venue: &str,
    ) -> Result<Option<(u64, u64, f64)>> {
        let signature_parsed = Signature::from_str(signature)?;
        let tx = self
            .rpc
            .get_transaction_with_config(
                &signature_parsed,
                RpcTransactionConfig {
                    encoding: Some(UiTransactionEncoding::Json),
                    commitment: Some(CommitmentConfig::confirmed()),
                    max_supported_transaction_version: Some(1),
                },
            )
            .await?;
        if tx
            .transaction
            .meta
            .as_ref()
            .and_then(|m| m.err.as_ref())
            .is_some()
        {
            return Ok(None);
        }
        Ok(parse_fill_from_tx(&tx, &self.wallet.pubkey(), mint, venue))
    }

    fn record_value(&self, state: &TokenInner, event: &str, extra: Value) -> Value {
        let mut value = json!({
            "event": event,
            "strategy": self.strategy,
            "descriptor": state.descriptor,
            "lifecycle": state.lifecycle.as_str(),
            "actualTokenAmount": state.actual_token_amount.map(|v| v.to_string()),
            "actualEntrySolAmount": state.actual_entry_sol_amount.map(|v| v.to_string()),
            "buyLamports": state.buy_lamports.map(|v| v.to_string()),
            "buySignature": state.buy_signature,
            "sellSignature": state.sell_signature,
            "entryProcessedMs": state.entry_processed_ms,
            "prices": state.prices,
        });
        if let (Value::Object(base), Value::Object(extra)) = (&mut value, extra) {
            for (key, item) in extra {
                base.insert(key, item);
            }
        }
        value
    }

    async fn reject_buy(&self, state: &mut TokenInner, event: &str, error: &str) {
        state.engine.on_buy_failed();
        state.sell_after_fill = false;
        if state.lifecycle == Lifecycle::BuyPrepared || state.lifecycle == Lifecycle::BuySent {
            let _ = state.transition(Lifecycle::TrackingPool);
        }
        state.reset_buy_send_claim();
        state.prepared_buy = None;
        self.journal.record(json!({
            "event": event,
            "descriptor": state.descriptor,
            "lifecycle": state.lifecycle.as_str(),
            "error": error,
            "prices": state.prices,
        }));
        tracing::warn!(error, mint = %state.descriptor.mint, "[05 ENTRY] Buy failed; still watching this mint");
    }

    /// Pump already rejected the sell because the wallet does not hold that
    /// amount. Closing here skips `on_sell_failed`, which would submit the
    /// same transaction again and pay the priority fee each time.
    fn close_empty_position(&self, state: &mut TokenInner, reason: &str, error: &str) {
        let closed = self.record_value(
            state,
            "position_closed",
            json!({ "reason": reason, "error": error, "flat": true }),
        );
        let mint = state.descriptor.mint.clone();
        state.engine.on_sell_fill();
        state.sell_after_fill = false;
        state.clear_filled_position();
        state.reset_buy_send_claim();
        state.reset_sell_send_claim();
        if !matches!(
            state.lifecycle,
            Lifecycle::SellPrepared | Lifecycle::SellSent
        ) {
            let _ = state.begin_sell_attempt();
        }
        if matches!(
            state.lifecycle,
            Lifecycle::SellPrepared | Lifecycle::SellSent
        ) {
            let _ = state.transition(Lifecycle::TrackingPool);
        }
        tracing::error!(
            error,
            reason,
            mint = %mint,
            "[08 EXIT] Token balance cannot cover the sell; stopped sell retries"
        );
        tracing::info!(value = %closed, "{}", record_message("position_closed"));
        let journal = Arc::clone(&self.journal);
        tokio::spawn(async move {
            journal.record_critical(closed).await;
        });
    }

    async fn wallet_token_amount(&self, mint: &str) -> Result<u64> {
        let mint: Pubkey = mint.parse()?;
        let accounts = self
            .rpc
            .get_token_accounts_by_owner(&self.wallet.pubkey(), TokenAccountsFilter::Mint(mint))
            .await?;
        let mut total = 0u64;
        for keyed in accounts {
            if let UiAccountData::Json(parsed) = keyed.account.data {
                let amount = parsed
                    .parsed
                    .get("info")
                    .and_then(|info| info.get("tokenAmount"))
                    .and_then(|amount| amount.get("amount"))
                    .and_then(|value| value.as_str())
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(0);
                total = total.saturating_add(amount);
            }
        }
        Ok(total)
    }

    async fn sell_failed(&self, state: &mut TokenInner, reason: &str, error: &str) {
        if state.lifecycle == Lifecycle::SellPrepared || state.lifecycle == Lifecycle::SellSent {
            let _ = state.transition(Lifecycle::PositionActiveConfirmed);
        }
        state.release_sell_send();
        state.prepared_sell = None;
        state.engine.on_sell_failed(now_ms());
        self.journal.record(json!({
            "event": "sell_retry_exhausted",
            "strategy": self.strategy,
            "descriptor": state.descriptor,
            "lifecycle": state.lifecycle.as_str(),
            "reason": reason,
            "error": error,
            "prices": state.prices,
        }));
        tracing::error!(error, reason, mint = %state.descriptor.mint, "[08 EXIT] Sell retries exhausted; position still monitored");
    }
}

#[derive(Debug, thiserror::Error)]
#[error("confirmation timed out for {signature}")]
struct ConfirmationTimeoutError {
    signature: String,
    seen: bool,
}

fn confirm_backoff(elapsed: Duration) -> Duration {
    if elapsed < Duration::from_secs(2) {
        Duration::from_millis(100)
    } else if elapsed < Duration::from_secs(8) {
        Duration::from_millis(250)
    } else {
        Duration::from_millis(500)
    }
}

fn combo_needs_split(err: &anyhow::Error) -> bool {
    too_large(err)
}

fn record_message(event: &str) -> &'static str {
    match event {
        "buy_sent" => "[05 ENTRY] Buy transaction submitted",
        "buy_processed" => "[06 CONFIRM] Buy processed; fill calculated",
        "buy_confirmed" => "[07 POSITION] Buy confirmed; position active",
        "sell_sent" => "[08 EXIT] Sell transaction submitted",
        "position_closed" => "[08 EXIT] Sell confirmed; position closed",
        _ => "[WORKFLOW]",
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn token_balance_sum(
    balances: &OptionSerializer<Vec<UiTransactionTokenBalance>>,
    owner: &str,
    mint: &str,
) -> u64 {
    let OptionSerializer::Some(items) = balances else {
        return 0;
    };
    items
        .iter()
        .filter(|balance| {
            balance.mint == mint
                && matches!(&balance.owner, OptionSerializer::Some(value) if value == owner)
        })
        .filter_map(|balance| balance.ui_token_amount.amount.parse::<u64>().ok())
        .sum()
}

fn launchpad_fill_from_tx(
    tx: &EncodedConfirmedTransactionWithStatusMeta,
    owner: &Pubkey,
    mint: &str,
) -> Option<(u64, u64, f64)> {
    let meta = tx.transaction.meta.as_ref()?;
    let owner_s = owner.to_string();
    let token_amount = token_balance_sum(&meta.pre_token_balances, &owner_s, mint)
        .abs_diff(token_balance_sum(&meta.post_token_balances, &owner_s, mint));
    if token_amount == 0 {
        return None;
    }
    let wsol = crate::venues::ids::WSOL_MINT.to_string();
    let wsol_delta = token_balance_sum(&meta.pre_token_balances, &owner_s, &wsol)
        .abs_diff(token_balance_sum(&meta.post_token_balances, &owner_s, &wsol));
    let sol_amount = if wsol_delta > 0 {
        wsol_delta
    } else {
        sol_delta_for_owner(tx, owner)
    };
    let price = sol_amount as f64 / token_amount as f64;
    Some((token_amount, sol_amount, price))
}

pub fn parse_fill_from_tx(
    tx: &EncodedConfirmedTransactionWithStatusMeta,
    owner: &Pubkey,
    mint: &str,
    venue: &str,
) -> Option<(u64, u64, f64)> {
    let meta = tx.transaction.meta.as_ref()?;
    if meta.err.is_some() {
        return None;
    }
    if venue == "launchlab" {
        return launchpad_fill_from_tx(tx, owner, mint);
    }
    let logs = match &meta.log_messages {
        OptionSerializer::Some(logs) => logs.as_slice(),
        _ => &[],
    };
    if let Some((token_amount, sol_amount)) = fill_from_logs(logs, owner, mint, venue) {
        let price = if token_amount == 0 {
            0.0
        } else {
            sol_amount as f64 / token_amount as f64
        };
        return Some((token_amount, sol_amount, price));
    }
    let owner_s = owner.to_string();
    let pre = token_balance_sum(&meta.pre_token_balances, &owner_s, mint);
    let post = token_balance_sum(&meta.post_token_balances, &owner_s, mint);
    let token_amount = pre.abs_diff(post);
    if token_amount == 0 {
        return None;
    }
    let sol_amount = sol_delta_for_owner(tx, owner);
    let price = if token_amount == 0 {
        0.0
    } else {
        sol_amount as f64 / token_amount as f64
    };
    Some((token_amount, sol_amount, price))
}

fn sol_delta_for_owner(tx: &EncodedConfirmedTransactionWithStatusMeta, owner: &Pubkey) -> u64 {
    let Some(meta) = tx.transaction.meta.as_ref() else {
        return 0;
    };
    let Some(decoded) = tx.transaction.transaction.decode() else {
        return 0;
    };
    let keys = decoded.message.static_account_keys();
    let Some(index) = keys.iter().position(|key| key == owner) else {
        return 0;
    };
    let pre = meta.pre_balances.get(index).copied().unwrap_or(0);
    let post = meta.post_balances.get(index).copied().unwrap_or(0);
    pre.abs_diff(post)
}

pub fn holding(state: &TokenInner) -> bool {
    is_holding(state.lifecycle)
}

/// Sum of wallet `post − pre` lamports. `None` when any signature is missing or not indexed yet.
async fn wallet_roundtrip_lamports(
    rpc: &RpcClient,
    wallet: &Pubkey,
    buy_sig: Option<&str>,
    sell_sig: Option<&str>,
    refill_sig: Option<&str>,
) -> Option<i64> {
    let sigs = [refill_sig, buy_sig, sell_sig];
    let sigs: Vec<&str> = sigs.into_iter().flatten().collect();
    if sigs.is_empty() {
        return None;
    }
    let mut sum = 0i64;
    for sig in sigs {
        let parsed = sig.parse().ok()?;
        let mut got = None;
        for attempt in 1..=6u32 {
            match rpc
                .get_transaction_with_config(
                    &parsed,
                    RpcTransactionConfig {
                        encoding: Some(UiTransactionEncoding::Json),
                        commitment: Some(CommitmentConfig::confirmed()),
                        max_supported_transaction_version: Some(1),
                    },
                )
                .await
            {
                Ok(tx) => {
                    if let Some(delta) = signed_wallet_delta(&tx, wallet) {
                        got = Some(delta);
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(400 * attempt as u64)).await;
                }
                Err(err) => {
                    tracing::warn!(
                        error = %err,
                        signature = sig,
                        "[PNL] Wallet delta lookup failed"
                    );
                    break;
                }
            }
        }
        sum += got?;
    }
    Some(sum)
}

fn signed_wallet_delta(
    tx: &EncodedConfirmedTransactionWithStatusMeta,
    wallet: &Pubkey,
) -> Option<i64> {
    let meta = tx.transaction.meta.as_ref()?;
    if meta.err.is_some() {
        return None;
    }
    let decoded = tx.transaction.transaction.decode()?;
    let index = decoded
        .message
        .static_account_keys()
        .iter()
        .position(|key| key == wallet)?;
    let pre = *meta.pre_balances.get(index)?;
    let post = *meta.post_balances.get(index)?;
    Some(post as i64 - pre as i64)
}
