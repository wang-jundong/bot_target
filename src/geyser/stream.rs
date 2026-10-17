//! Persistent Yellowstone/Geyser stream — transport matches New Folder:
//! keepalive, in-place `sink.send` filter updates, 300s without a transaction, 2s reconnect.
//! Parsed transactions are forwarded on `outbound` for the strategy runtime.
//!
//! Pings and account updates do not refresh the transaction timer. Geyser sends
//! those on an open socket even after it has stopped delivering trades, and
//! treating them as liveness left the bot deaf for hours.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use futures::{SinkExt, StreamExt};
use solana_sdk::pubkey::Pubkey;
use tokio::sync::mpsc;
use tracing::{error, info, warn};
use yellowstone_grpc_client::{ClientTlsConfig, GeyserGrpcClient};
use yellowstone_grpc_proto::prelude::{SubscribeUpdate, subscribe_update::UpdateOneof};

use crate::events::ParsedTargetTransaction;
use crate::venues::Venues;

use super::parser::{
    account_data, account_pubkey, bonding_curve_from_account_data, parse_transaction,
    token_account_amount,
};
use super::subscribe::{SubscriptionState, build_request};

const IDLE: Duration = Duration::from_secs(300);
const RECONNECT: Duration = Duration::from_secs(2);

#[derive(Debug)]
pub enum GeyserCommand {
    RegisterCurve {
        mint: Pubkey,
        bonding_curve: Pubkey,
        associated_bonding_curve: Pubkey,
        associated_quote_bonding_curve: Option<Pubkey>,
    },
    RegisterAmm {
        mint: Pubkey,
        pool: Pubkey,
        pool_base_token_account: Pubkey,
        pool_quote_token_account: Pubkey,
    },
    Unregister {
        mint: Pubkey,
    },
    SetTargetWallets {
        wallets: Vec<Pubkey>,
    },
}

pub struct GeyserHandle {
    pub commands: mpsc::Sender<GeyserCommand>,
}

#[derive(Clone, Copy)]
enum WatchedMint {
    Curve { mint: Pubkey },
    Amm { mint: Pubkey },
}

#[derive(Clone, Copy)]
enum ReserveAccountRef {
    Curve { mint: Pubkey },
    AmmVault { mint: Pubkey, is_base: bool },
}

struct PumpCtx {
    venues: Arc<Venues>,
    outbound: mpsc::Sender<ParsedTargetTransaction>,
    watched: HashMap<Pubkey, WatchedMint>,
    reserve_accounts: HashMap<Pubkey, ReserveAccountRef>,
    sub_state: SubscriptionState,
    dropped: u64,
}

pub async fn run(
    endpoint: String,
    token: Option<String>,
    wallet_pubkeys: Vec<Pubkey>,
    target_wallets: Vec<Pubkey>,
    venues: Arc<Venues>,
    outbound: mpsc::Sender<ParsedTargetTransaction>,
    mut commands: mpsc::Receiver<GeyserCommand>,
) -> Result<()> {
    let mut ctx = PumpCtx {
        venues,
        outbound,
        watched: HashMap::new(),
        reserve_accounts: HashMap::new(),
        sub_state: SubscriptionState {
            wallet_pubkeys,
            target_wallets,
            ..Default::default()
        },
        dropped: 0,
    };

    loop {
        match connect_and_pump(&endpoint, token.as_deref(), &mut ctx, &mut commands).await {
            Ok(()) => {
                warn!("[02 STREAM] geyser stream closed cleanly, reconnecting in 2s");
            }
            Err(e) => {
                error!(error = %e, "[02 STREAM] geyser stream error, reconnecting in 2s");
            }
        }
        if commands.is_closed() {
            return Ok(());
        }
        tokio::time::sleep(RECONNECT).await;
    }
}

async fn connect_and_pump(
    endpoint: &str,
    token: Option<&str>,
    ctx: &mut PumpCtx,
    commands: &mut mpsc::Receiver<GeyserCommand>,
) -> Result<()> {
    // Keepalive + connect timeout: half-open TCP otherwise leaves stream.next() Pending forever.
    let mut client = GeyserGrpcClient::build_from_shared(endpoint.to_string())?
        .x_token(token.map(|t| t.to_string()))?
        .tls_config(ClientTlsConfig::new().with_native_roots())?
        .connect_timeout(Duration::from_secs(10))
        .http2_keep_alive_interval(Duration::from_secs(20))
        .keep_alive_timeout(Duration::from_secs(10))
        .keep_alive_while_idle(true)
        .tcp_keepalive(Some(Duration::from_secs(20)))
        .max_decoding_message_size(16 * 1024 * 1024)
        .connect()
        .await?;

    let initial_request = build_request(&ctx.sub_state);
    let (mut sink, mut stream) = client.subscribe_with_request(Some(initial_request)).await?;
    let gates = ctx
        .sub_state
        .target_wallets
        .iter()
        .map(|w| w.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    info!(%gates, pools = ctx.sub_state.watched_pools.len(), "[02 STREAM] geyser stream connected");

    let mut last_tx = Instant::now();
    loop {
        // Checked before select so a burst of pings or filter commands cannot
        // postpone reconnect once the transaction feed has been quiet for IDLE.
        if transaction_feed_expired(last_tx.elapsed(), false) {
            anyhow::bail!(
                "no geyser transactions for {}s — forcing reconnect",
                IDLE.as_secs()
            );
        }
        let quiet_for = IDLE.saturating_sub(last_tx.elapsed());
        tokio::select! {
            _ = tokio::time::sleep(quiet_for) => {
                anyhow::bail!(
                    "no geyser transactions for {}s — forcing reconnect",
                    IDLE.as_secs()
                );
            }
            cmd = commands.recv() => {
                match cmd {
                    Some(GeyserCommand::RegisterCurve {
                        mint,
                        bonding_curve,
                        associated_bonding_curve,
                        associated_quote_bonding_curve,
                    }) => {
                        ctx.watched.retain(|_, w| !matches!(w, WatchedMint::Curve { mint: m } | WatchedMint::Amm { mint: m } if *m == mint));
                        if let Some(prev) = ctx.sub_state.watched_accounts.get(&mint) {
                            for account in prev {
                                ctx.reserve_accounts.remove(account);
                            }
                        }
                        ctx.sub_state.watched_pools.insert(mint, bonding_curve);
                        let mut accounts = vec![bonding_curve, associated_bonding_curve];
                        if let Some(q) = associated_quote_bonding_curve {
                            accounts.push(q);
                        }
                        ctx.sub_state.watched_accounts.insert(mint, accounts);
                        ctx.watched.insert(bonding_curve, WatchedMint::Curve { mint });
                        ctx.reserve_accounts.insert(bonding_curve, ReserveAccountRef::Curve { mint });
                        let req = build_request(&ctx.sub_state);
                        if let Err(e) = sink.send(req).await {
                            error!("failed to push updated geyser subscription for {mint}: {e}");
                        } else {
                            info!("geyser subscription updated for mint {mint} (curve)");
                        }
                    }
                    Some(GeyserCommand::RegisterAmm {
                        mint,
                        pool,
                        pool_base_token_account,
                        pool_quote_token_account,
                    }) => {
                        ctx.watched.retain(|_, w| !matches!(w, WatchedMint::Curve { mint: m } | WatchedMint::Amm { mint: m } if *m == mint));
                        if let Some(prev) = ctx.sub_state.watched_accounts.get(&mint) {
                            for account in prev {
                                ctx.reserve_accounts.remove(account);
                            }
                        }
                        ctx.sub_state.watched_pools.insert(mint, pool);
                        ctx.sub_state.watched_accounts.insert(
                            mint,
                            vec![pool_base_token_account, pool_quote_token_account],
                        );
                        ctx.watched.insert(pool, WatchedMint::Amm { mint });
                        ctx.reserve_accounts.insert(
                            pool_base_token_account,
                            ReserveAccountRef::AmmVault { mint, is_base: true },
                        );
                        ctx.reserve_accounts.insert(
                            pool_quote_token_account,
                            ReserveAccountRef::AmmVault { mint, is_base: false },
                        );
                        let req = build_request(&ctx.sub_state);
                        if let Err(e) = sink.send(req).await {
                            error!("failed to push updated geyser subscription for {mint}: {e}");
                        } else {
                            info!("geyser subscription updated for mint {mint} (AMM)");
                        }
                    }
                    Some(GeyserCommand::Unregister { mint }) => {
                        ctx.watched.retain(|_, w| !matches!(w, WatchedMint::Curve { mint: m } | WatchedMint::Amm { mint: m } if *m == mint));
                        if let Some(accounts) = ctx.sub_state.watched_accounts.remove(&mint) {
                            for account in accounts {
                                ctx.reserve_accounts.remove(&account);
                            }
                        }
                        ctx.sub_state.watched_pools.remove(&mint);
                        let req = build_request(&ctx.sub_state);
                        if let Err(e) = sink.send(req).await {
                            error!("failed to push updated geyser subscription for {mint}: {e}");
                        } else {
                            info!("geyser subscription dropped for removed mint {mint}");
                        }
                    }
                    Some(GeyserCommand::SetTargetWallets { wallets }) => {
                        ctx.sub_state.target_wallets = wallets;
                        let req = build_request(&ctx.sub_state);
                        if let Err(e) = sink.send(req).await {
                            error!("failed to push target-wallet geyser subscription: {e}");
                        } else {
                            info!(
                                targets = ctx.sub_state.target_wallets.len(),
                                "[02 STREAM] geyser target wallets updated"
                            );
                        }
                    }
                    None => return Ok(()),
                }
            }
            update = stream.next() => {
                match update {
                    Some(Ok(update)) => {
                        let is_tx = matches!(update.update_oneof, Some(UpdateOneof::Transaction(_)));
                        if is_tx {
                            last_tx = Instant::now();
                        }
                        handle_update(ctx, update);
                    }
                    Some(Err(status)) => anyhow::bail!("stream status error: {status}"),
                    None => return Ok(()),
                }
            }
        }
    }
}

/// True when this message is not a transaction and the feed has been quiet for [`IDLE`].
fn transaction_feed_expired(since_last_tx: Duration, is_transaction: bool) -> bool {
    !is_transaction && since_last_tx >= IDLE
}

fn handle_update(ctx: &mut PumpCtx, update: SubscribeUpdate) {
    match update.update_oneof {
        Some(UpdateOneof::Account(acc)) => handle_account_update(ctx, acc),
        Some(UpdateOneof::Transaction(tx)) => {
            if let Some(parsed) = parse_transaction(tx.slot, tx.transaction.as_ref()) {
                if ctx.outbound.try_send(parsed).is_err() {
                    ctx.dropped = ctx.dropped.saturating_add(1);
                    if ctx.dropped == 1 || ctx.dropped % 100 == 0 {
                        warn!(
                            dropped = ctx.dropped,
                            "[02 STREAM] Inbound queue full; dropping tx"
                        );
                    }
                }
            }
        }
        _ => {}
    }
}

fn handle_account_update(
    ctx: &mut PumpCtx,
    acc: yellowstone_grpc_proto::prelude::SubscribeUpdateAccount,
) {
    let Some(pubkey) = account_pubkey(&acc) else {
        return;
    };
    let Some(reserve_ref) = ctx.reserve_accounts.get(&pubkey).copied() else {
        return;
    };
    let Some(data) = account_data(&acc) else {
        return;
    };

    match reserve_ref {
        ReserveAccountRef::Curve { mint } => {
            if let Some(curve) = bonding_curve_from_account_data(data) {
                ctx.venues.note_curve(&mint.to_string(), curve);
            }
        }
        ReserveAccountRef::AmmVault { mint, is_base } => {
            if let Some(amount) = token_account_amount(data) {
                ctx.venues
                    .update_amm_vault(&mint.to_string(), is_base, amount);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pings_and_account_updates_do_not_keep_a_quiet_feed_alive() {
        let almost = Duration::from_secs(299);
        let idle = Duration::from_secs(300);
        assert!(!transaction_feed_expired(almost, false));
        assert!(transaction_feed_expired(idle, false));
        assert!(!transaction_feed_expired(idle, true));
        assert!(!transaction_feed_expired(Duration::from_secs(3_600), true));
    }
}
