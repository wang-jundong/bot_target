use crate::events::mono_ms;
use crate::helius::{is_non_retryable_buy_error, HeliusSender, HeliusSenderUnavailable};
use crate::journal::{create_pnl_record, ClosedTradePnlInput, PnlJournal, RecoveryJournal};
use crate::state::{is_holding, Lifecycle, TokenInner};
use crate::venues::decoder::fill_from_logs;
use crate::venues::trade::compile_trade;
use crate::venues::Venues;
use anyhow::{anyhow, Result};
use base64::Engine;
use serde_json::{json, Value};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_client::rpc_config::{RpcSendTransactionConfig, RpcTransactionConfig};
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signature};
use solana_sdk::signer::Signer;
use solana_sdk::transaction::VersionedTransaction;
use solana_transaction_status_client_types::{
    option_serializer::OptionSerializer, EncodedConfirmedTransactionWithStatusMeta, TransactionConfirmationStatus,
    UiTransactionEncoding, UiTransactionTokenBalance,
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
        Self { client, refresh_ms, current: Mutex::new(None) }
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
        let stale = {
            let guard = self.current.lock().await;
            match *guard {
                Some((_, at)) => at.elapsed() > Duration::from_millis(self.refresh_ms.saturating_mul(2)),
                None => true,
            }
        };
        if stale {
            self.refresh().await?;
        }
        self.current.lock().await.ok_or_else(|| anyhow!("blockhash unavailable")).map(|(hash, _)| hash)
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
}

impl LiveExecution {
    pub async fn send_buy(self: &Arc<Self>, token: &Mutex<TokenInner>) -> Result<()> {
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
                let token_program_owned = state.descriptor.token_program.clone().unwrap_or_else(|| crate::venues::ids::TOKEN_PROGRAM.to_string());
                let token_program = token_program_owned.parse()?;
                let venue = state.descriptor.venue.clone();
                let mint = state.descriptor.mint.clone();
                if attempt > 1 || state.prepared_buy.is_none() {
                    state.prepared_buy = Some(self.venues.build_buy(&venue, &self.wallet.pubkey(), &mint, &token_program, lamports, slippage)?);
                }
                state.prepared_buy.clone()
            };
            let Some(prepared) = prepared else { last_error = "buy was not prepared".to_string(); continue };
            match self.submit_and_confirm(token, &prepared, true, attempt, None).await {
                Ok((_signature, token_amount, sol_amount, price)) => {
                    let mut state = token.lock().await;
                    state.actual_token_amount = Some(token_amount);
                    state.actual_entry_sol_amount = Some(sol_amount);
                    state.prices.actual_entry_fill_price = Some(price);
                    state.entry_processed_ms = Some(now_ms());
                    state.transition(Lifecycle::BuyProcessed)?;
                    state.transition(Lifecycle::PositionActiveUnconfirmed)?;
                    self.record_critical(&state, "buy_processed", json!({ "fill": { "tokenAmount": token_amount.to_string(), "solAmount": sol_amount.to_string(), "price": price } })).await;
                    state.transition(Lifecycle::BuyConfirmed)?;
                    state.transition(Lifecycle::PositionActiveConfirmed)?;
                    self.record(&state, "buy_confirmed", json!({}));
                    let strategy_price = crate::events::to_strategy_price(price);
                    let missed = state.engine.on_buy_fill(if strategy_price > 0.0 { strategy_price } else { 0.0 }, now_ms());
                    if missed.is_some() {
                        state.sell_after_fill = true;
                    } else if state.sell_after_fill {
                        state.engine.apply_missed_target_sell();
                    }
                    return Ok(());
                }
                Err(err) => {
                    last_error = err.to_string();
                    let venue = token.lock().await.descriptor.venue.clone();
                    let mint = token.lock().await.descriptor.mint.clone();
                    tracing::warn!(attempt, mint = %mint, error = %last_error, "[05 ENTRY] Buy attempt failed");
                    if is_non_retryable_buy_error(&last_error, &venue) {
                        let mut state = token.lock().await;
                        self.reject_buy(&mut state, "buy_slippage_rejected", &last_error).await;
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

    pub async fn send_sell(self: &Arc<Self>, token: &Mutex<TokenInner>, reason: &str) -> Result<()> {
        let amount = token.lock().await.actual_token_amount;
        let Some(amount) = amount else {
            let mut state = token.lock().await;
            self.sell_failed(&mut state, reason, "position token amount is unknown").await;
            return Ok(());
        };
        let mut last_error = String::from("sell failed");
        for attempt in 1..=3 {
            let prepared = {
                let mut state = token.lock().await;
                let slippage = state.sell_slippage_bps.unwrap_or(self.sell_slippage_bps);
                let token_program = state.descriptor.token_program.clone().unwrap_or_else(|| crate::venues::ids::TOKEN_PROGRAM.to_string()).parse()?;
                let venue = state.descriptor.venue.clone();
                let mint = state.descriptor.mint.clone();
                state.prepared_sell = Some(self.venues.build_sell(&venue, &self.wallet.pubkey(), &mint, &token_program, amount, slippage)?);
                if state.lifecycle != Lifecycle::SellPrepared {
                    state.transition(Lifecycle::SellPrepared)?;
                }
                state.prepared_sell.clone()
            };
            let Some(prepared) = prepared else { continue };
            match self.submit_and_confirm(token, &prepared, false, attempt, Some(reason)).await {
                Ok((_signature, token_amount, sol_amount, price)) => {
                    let mut state = token.lock().await;
                    state.prices.actual_exit_fill_price = Some(price);
                    self.record_critical(&state, "position_closed", json!({ "reason": reason })).await;
                    let closed_at = now_ms();
                    if let (Some(entry_sol), Some(entry_price), Some(entry_ms)) = (state.actual_entry_sol_amount, state.prices.actual_entry_fill_price, state.entry_processed_ms) {
                        let input = ClosedTradePnlInput {
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
                        };
                        match self.pnl.record(input).await {
                            Ok(()) => tracing::info!(mint = %state.descriptor.mint, "[PNL] Closed trade appended"),
                            Err(err) => tracing::error!(error = %err, mint = %state.descriptor.mint, "[PNL] Failed to append closed trade"),
                        }
                        let _ = create_pnl_record;
                    }
                    state.engine.on_sell_fill();
                    state.sell_after_fill = false;
                    state.clear_filled_position();
                    state.reset_buy_send_claim();
                    state.reset_sell_send_claim();
                    state.transition(Lifecycle::TrackingPool)?;
                    tracing::info!(mint = %state.descriptor.mint, "[REENTRY] Round finished; still watching for the next target buy");
                    return Ok(());
                }
                Err(err) => {
                    last_error = err.to_string();
                    token.lock().await.prepared_sell = None;
                    tracing::warn!(attempt, error = %last_error, "[08 EXIT] Sell attempt failed");
                }
            }
        }
        let mut state = token.lock().await;
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
        let raw = bincode::serialize(&tx)?;
        let encoded = base64::engine::general_purpose::STANDARD.encode(&raw);
        let (signature, fallback) = match self.submit(&encoded, &tx).await {
            Ok(pair) => pair,
            Err(err) => return Err(err),
        };
        let (mint, venue) = {
            let mut state = token.lock().await;
            if is_buy {
                state.buy_signature = Some(signature.clone());
                state.transition(Lifecycle::BuySent)?;
                self.record_critical(&state, "buy_sent", json!({ "attempt": attempt, "signature": signature, "fallback": fallback })).await;
            } else {
                state.sell_signature = Some(signature.clone());
                state.transition(Lifecycle::SellSent)?;
                self.record_critical(
                    &state,
                    "sell_sent",
                    json!({ "attempt": attempt, "reason": sell_reason.unwrap_or(""), "signature": signature, "fallback": fallback }),
                ).await;
            }
            (state.descriptor.mint.clone(), state.descriptor.venue.clone())
        };
        let (token_amount, sol_amount, price) = self.confirm(&signature, &mint, &venue, fallback).await?;
        if is_buy && token_amount == 0 {
            return Err(anyhow!("buy fill could not be determined"));
        }
        if !is_buy && token_amount == 0 && sol_amount == 0 {
            return Err(anyhow!("sell fill could not be determined"));
        }
        Ok((signature, token_amount, sol_amount, price))
    }

    async fn submit(&self, encoded: &str, transaction: &VersionedTransaction) -> Result<(String, bool)> {
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
            Err(err) => Err(anyhow!("Sender and RPC fallback both failed: {last_error}; {err}")),
        }
    }

    async fn confirm(&self, signature: &str, mint: &str, venue: &str, fallback: bool) -> Result<(u64, u64, f64)> {
        // bot_target: if status was seen but fill lagged, re-poll up to 3 outer rounds.
        let mut last_err = anyhow!("confirmation timed out for {signature}");
        for poll in 1..=3 {
            match self.wait_processed(signature, mint, venue, fallback).await {
                Ok(fill) => return Ok(fill),
                Err(err) => {
                    let seen = err.downcast_ref::<ConfirmationTimeoutError>().map(|e| e.seen).unwrap_or(false);
                    last_err = err;
                    if !seen || poll == 3 {
                        break;
                    }
                    tracing::warn!(poll, signature, "[06 CONFIRM] Seen but fill missing; re-polling");
                }
            }
        }
        Err(last_err)
    }

    async fn wait_processed(&self, signature: &str, mint: &str, venue: &str, fallback: bool) -> Result<(u64, u64, f64)> {
        let signature_parsed = Signature::from_str(signature)?;
        let started = Instant::now();
        let deadline = Duration::from_secs(30);
        let unseen_deadline = if fallback { Duration::from_secs(10) } else { deadline };
        let mut seen = false;
        while started.elapsed() < deadline {
            let statuses = self.rpc.get_signature_statuses_with_history(&[signature_parsed]).await?;
            if let Some(Some(status)) = statuses.value.first() {
                seen = true;
                if let Some(err) = &status.err {
                    return Err(anyhow!("transaction failed: {err:?}"));
                }
                if matches!(
                    status.confirmation_status,
                    Some(TransactionConfirmationStatus::Confirmed | TransactionConfirmationStatus::Finalized)
                ) {
                    match self
                        .rpc
                        .get_transaction_with_config(
                            &signature_parsed,
                            RpcTransactionConfig {
                                encoding: Some(UiTransactionEncoding::Json),
                                commitment: Some(CommitmentConfig::confirmed()),
                                max_supported_transaction_version: Some(0),
                            },
                        )
                        .await
                    {
                        Ok(tx) => {
                            if let Some(fill) = parse_fill_from_tx(&tx, &self.wallet.pubkey(), mint, venue) {
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
            tokio::time::sleep(confirm_backoff(started.elapsed())).await;
        }
        Err(ConfirmationTimeoutError { signature: signature.to_string(), seen }.into())
    }

    /// Recover fill from a landed signature (bot_target getTransaction + parseFill).
    pub async fn fill_from_signature(&self, signature: &str, mint: &str, venue: &str) -> Result<Option<(u64, u64, f64)>> {
        let signature_parsed = Signature::from_str(signature)?;
        let tx = self.rpc.get_transaction_with_config(
            &signature_parsed,
            RpcTransactionConfig {
                encoding: Some(UiTransactionEncoding::Json),
                commitment: Some(CommitmentConfig::confirmed()),
                max_supported_transaction_version: Some(0),
            },
        ).await?;
        if tx.transaction.meta.as_ref().and_then(|m| m.err.as_ref()).is_some() {
            return Ok(None);
        }
        Ok(parse_fill_from_tx(&tx, &self.wallet.pubkey(), mint, venue))
    }

    fn record(&self, state: &TokenInner, event: &str, extra: Value) {
        let value = self.record_value(state, event, extra);
        let message = record_message(event);
        tracing::info!(%value, "{message}");
        self.journal.record(value);
    }

    async fn record_critical(&self, state: &TokenInner, event: &str, extra: Value) {
        let value = self.record_value(state, event, extra);
        let message = record_message(event);
        tracing::info!(%value, "{message}");
        self.journal.record_critical(value).await;
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
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as u64
}

fn token_balance_sum(balances: &OptionSerializer<Vec<UiTransactionTokenBalance>>, owner: &str, mint: &str) -> u64 {
    let OptionSerializer::Some(items) = balances else { return 0 };
    items.iter().filter(|balance| balance.mint == mint && matches!(&balance.owner, OptionSerializer::Some(value) if value == owner)).filter_map(|balance| balance.ui_token_amount.amount.parse::<u64>().ok()).sum()
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
    let logs = match &meta.log_messages {
        OptionSerializer::Some(logs) => logs.as_slice(),
        _ => &[],
    };
    if let Some((token_amount, sol_amount)) = fill_from_logs(logs, owner, mint, venue) {
        let price = if token_amount == 0 { 0.0 } else { sol_amount as f64 / token_amount as f64 };
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
    let price = if token_amount == 0 { 0.0 } else { sol_amount as f64 / token_amount as f64 };
    Some((token_amount, sol_amount, price))
}

fn sol_delta_for_owner(tx: &EncodedConfirmedTransactionWithStatusMeta, owner: &Pubkey) -> u64 {
    let Some(meta) = tx.transaction.meta.as_ref() else { return 0 };
    let Some(decoded) = tx.transaction.transaction.decode() else { return 0 };
    let keys = decoded.message.static_account_keys();
    let Some(index) = keys.iter().position(|key| key == owner) else { return 0 };
    let pre = meta.pre_balances.get(index).copied().unwrap_or(0);
    let post = meta.post_balances.get(index).copied().unwrap_or(0);
    pre.abs_diff(post)
}

pub fn holding(state: &TokenInner) -> bool {
    is_holding(state.lifecycle)
}
