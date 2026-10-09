use bot_target::config::load_config;
use bot_target::execution::{BlockhashCache, LiveExecution};
use bot_target::geyser::{self, GeyserHandle};
use bot_target::helius::HeliusSender;
use bot_target::journal::{PnlJournal, RecoveryJournal};
use bot_target::runtime::TradingRuntime;
use bot_target::venues::Venues;
use solana_sdk::signer::Signer;
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = Arc::new(load_config()?);
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new(&config.log_level))
        .with_target(false)
        .without_time()
        .init();

    for plan in &config.strategy_plans {
        match plan.name {
            bot_target::config::StrategyName::V011 => {
                let c = &config.strategy_v011;
                tracing::info!(
                    strategy = plan.name.as_str(),
                    target_wallet = %plan.target_wallet,
                    size_sol = c.size_sol,
                    min_entry_s = c.min_entry_s,
                    max_entry_s = c.max_entry_s,
                    max_chase = c.max_chase,
                    take_profit = c.take_profit,
                    sell_hit_count = c.sell_hit_count,
                    sell_hit_min = c.sell_hit_min,
                    "[01 STARTUP] Strategy knobs"
                );
            }
            bot_target::config::StrategyName::V022 => {
                let c = &config.strategy_v022;
                tracing::info!(
                    strategy = plan.name.as_str(),
                    target_wallet = %plan.target_wallet,
                    size_sol = c.size_sol,
                    buy_hit_count = c.buy_hit_count,
                    buy_hit_min = c.buy_hit_min,
                    buy_hit_round = c.buy_hit_round,
                    sell_hit_count = c.sell_hit_count,
                    sell_hit_min = c.sell_hit_min,
                    max_mc_sol = c.max_mc_sol,
                    take_profit = c.take_profit,
                    "[01 STARTUP] Strategy knobs"
                );
            }
            bot_target::config::StrategyName::V031 => {
                let c = &config.strategy_v031;
                tracing::info!(
                    strategy = plan.name.as_str(),
                    target_wallet = %plan.target_wallet,
                    size_sol = c.size_sol,
                    min_entry_s = c.min_entry_s,
                    max_entry_s = c.max_entry_s,
                    buy_hit_count = c.buy_hit_count,
                    buy_hit_min = c.buy_hit_min,
                    sell_hit_count = c.sell_hit_count,
                    sell_hit_min = c.sell_hit_min,
                    take_profit = c.take_profit,
                    max_hold_s = c.max_hold_s,
                    "[01 STARTUP] Strategy knobs"
                );
            }
        }
    }

    let rpc = Arc::new(solana_client::nonblocking::rpc_client::RpcClient::new(config.helius_rpc_url.clone()));
    let blockhashes = Arc::new(BlockhashCache::new(Arc::clone(&rpc), config.blockhash_refresh_ms));
    blockhashes.spawn();
    let venues = Arc::new(Venues::new(config.compute_unit_limit, config.priority_fee_lamports, config.helius_tip_lamports));
    let journal = Arc::new(RecoveryJournal::new(config.recovery_path.clone()));
    let pnl = Arc::new(PnlJournal::new(config.pnl_path.clone()));

    let mut executions = Vec::with_capacity(config.strategy_plans.len());
    let mut own_wallets = Vec::new();
    let mut target_wallets = Vec::new();
    let mut seen_targets = HashSet::new();
    for plan in &config.strategy_plans {
        own_wallets.push(plan.keypair.pubkey());
        if seen_targets.insert(plan.target_wallet.clone()) {
            target_wallets.push(plan.target_wallet.parse()?);
        }
        executions.push(Arc::new(LiveExecution {
            rpc: Arc::clone(&rpc),
            wallet: Arc::new(plan.keypair.insecure_clone()),
            strategy: plan.name.as_str().to_string(),
            buy_lamports: plan.buy_amount_lamports,
            buy_slippage_bps: config.buy_slippage_bps,
            sell_slippage_bps: config.sell_slippage_bps,
            blockhashes: Arc::clone(&blockhashes),
            sender: HeliusSender::new(config.helius_sender_url.clone(), config.helius_sender_swqos_only),
            journal: Arc::clone(&journal),
            pnl: Arc::clone(&pnl),
            venues: Arc::clone(&venues),
        }));
    }

    let (commands_tx, commands_rx) = mpsc::channel(256);
    let (inbound_tx, inbound_rx) = mpsc::channel(4_096);
    let geyser = GeyserHandle { commands: commands_tx };
    let endpoint = config.vibe_grpc_endpoint.clone();
    let token = config.vibe_grpc_token.clone();
    let venues_geyser = Arc::clone(&venues);
    tokio::spawn(async move {
        if let Err(err) = geyser::run(
            endpoint,
            Some(token),
            own_wallets,
            target_wallets,
            venues_geyser,
            inbound_tx,
            commands_rx,
        )
        .await
        {
            tracing::error!(error = %err, "[02 STREAM] geyser task exited");
        }
    });

    let runtime = TradingRuntime::new(
        Arc::clone(&config),
        rpc,
        geyser,
        journal,
        executions,
        inbound_rx,
    );
    runtime.start().await?;
    let wallets: Vec<_> = config
        .strategy_plans
        .iter()
        .map(|p| (p.name.as_str(), p.pubkey_b58()))
        .collect();
    tracing::info!(
        wallets = ?wallets,
        strategy = %config.strategy_label,
        "[01 STARTUP] Bot ready"
    );
    tokio::signal::ctrl_c().await?;
    runtime.close().await;
    Ok(())
}
