use bot_target::alt_quote::AltQuote;
use bot_target::config::{StrategyName, load_config};
use bot_target::execution::{BlockhashCache, LiveExecution};
use bot_target::geyser::{self, GeyserHandle};
use bot_target::helius::HeliusSender;
use bot_target::journal::{PnlJournal, RecoveryJournal};
use bot_target::runtime::TradingRuntime;
use bot_target::strategy::v071_tape::V071Tape;
use bot_target::venues::Venues;
use solana_sdk::signer::Signer;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
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
                    min_mc_sol = c.min_mc_sol,
                    max_mc_sol = c.max_mc_sol,
                    min_largest_buy_sol = c.min_largest_buy_sol,
                    max_largest_buy_sol = c.max_largest_buy_sol,
                    min_price_ratio = c.min_price_ratio,
                    max_price_ratio = c.max_price_ratio,
                    round_trip_window_s = c.round_trip_window_s,
                    min_round_trip_holders = c.min_round_trip_holders,
                    max_round_trip_holders = c.max_round_trip_holders,
                    sell_hit_count = c.sell_hit_count,
                    sell_hit_min = c.sell_hit_min,
                    take_profit = c.take_profit,
                    max_hold_s = c.max_hold_s,
                    "[01 STARTUP] Strategy knobs"
                );
            }
            bot_target::config::StrategyName::V051 => {
                let c = &config.strategy_v051;
                tracing::info!(
                    strategy = plan.name.as_str(),
                    target_wallet = %plan.target_wallet,
                    size_sol = c.size_sol,
                    min_entry_s = c.min_entry_s,
                    max_entry_s = c.max_entry_s,
                    buy_hit_count = c.buy_hit_count,
                    buy_hit_min = c.buy_hit_min,
                    min_mc_sol = c.min_mc_sol,
                    max_mc_sol = c.max_mc_sol,
                    sell_hit_count = c.sell_hit_count,
                    sell_hit_min = c.sell_hit_min,
                    take_profit = c.take_profit,
                    max_hold_s = c.max_hold_s,
                    "[01 STARTUP] Strategy knobs"
                );
            }
            bot_target::config::StrategyName::V061 => {
                let c = &config.strategy_v061;
                tracing::info!(
                    strategy = plan.name.as_str(),
                    target_wallet = %plan.target_wallet,
                    size_sol = c.size_sol,
                    min_entry_s = c.min_entry_s,
                    max_entry_s = c.max_entry_s,
                    max_chase = c.max_chase,
                    min_mc_sol = c.min_mc_sol,
                    max_mc_sol = c.max_mc_sol,
                    sell_hit_count = c.sell_hit_count,
                    sell_hit_min = c.sell_hit_min,
                    take_profit = c.take_profit,
                    max_hold_s = c.max_hold_s,
                    second_buy_exit = c.second_buy_exit,
                    repeat_entries = c.repeat_entries,
                    "[01 STARTUP] Strategy knobs"
                );
            }
            bot_target::config::StrategyName::V071 => {
                let c = &config.strategy_v071;
                tracing::info!(
                    strategy = plan.name.as_str(),
                    target_wallet = %plan.target_wallet,
                    size_sol = c.size_sol,
                    min_entry_s = c.min_entry_s,
                    max_entry_s = c.max_entry_s,
                    min_mc_sol = c.min_mc_sol,
                    max_mc_sol = c.max_mc_sol,
                    buy_pressure_s = c.buy_pressure_s,
                    min_buy_pressure = c.min_buy_pressure,
                    max_buy_pressure = c.max_buy_pressure,
                    target_volume_s = c.target_volume_s,
                    min_target_volume_sol = c.min_target_volume_sol,
                    max_target_volume_sol = c.max_target_volume_sol,
                    min_target_age_s = c.min_target_age_s,
                    max_target_age_s = c.max_target_age_s,
                    partial_sell_skip = c.partial_sell_skip,
                    sell_hit_count = c.sell_hit_count,
                    sell_hit_min = c.sell_hit_min,
                    take_profit = c.take_profit,
                    max_hold_s = c.max_hold_s,
                    second_buy_exit = c.second_buy_exit,
                    repeat_entries = c.repeat_entries,
                    "[01 STARTUP] Strategy knobs"
                );
            }
            bot_target::config::StrategyName::V081 => {
                let c = &config.strategy_v081;
                tracing::info!(
                    strategy = plan.name.as_str(),
                    target_wallet = %plan.target_wallet,
                    size_sol = c.size_sol,
                    min_entry_s = c.min_entry_s,
                    max_entry_s = c.max_entry_s,
                    buy_hit_count = c.buy_hit_count,
                    min_mc_sol = c.min_mc_sol,
                    max_mc_sol = c.max_mc_sol,
                    min_largest_buy_sol = c.min_largest_buy_sol,
                    max_largest_buy_sol = c.max_largest_buy_sol,
                    min_price_ratio = c.min_price_ratio,
                    max_price_ratio = c.max_price_ratio,
                    sell_hit_count = c.sell_hit_count,
                    sell_hit_min = c.sell_hit_min,
                    take_profit = c.take_profit,
                    max_hold_s = c.max_hold_s,
                    hold_sell_s = c.hold_sell_s,
                    min_largest_sell_sol = c.min_largest_sell_sol,
                    max_largest_sell_sol = c.max_largest_sell_sol,
                    round_trip_s = c.round_trip_s,
                    min_round_trip_holders = c.min_round_trip_holders,
                    max_round_trip_holders = c.max_round_trip_holders,
                    max_trades_per_mint = c.max_trades_per_mint,
                    repeat_entries = c.repeat_entries,
                    "[01 STARTUP] Strategy knobs"
                );
            }
            bot_target::config::StrategyName::V101 => {
                let c = &config.strategy_v101;
                tracing::info!(
                    strategy = plan.name.as_str(),
                    target_wallet = %plan.target_wallet,
                    size_sol = c.size_sol,
                    min_entry_s = c.min_entry_s,
                    max_entry_s = c.max_entry_s,
                    min_price_ratio = c.min_price_ratio,
                    max_price_ratio = c.max_price_ratio,
                    buy_hit_count = c.buy_hit_count,
                    buy_hit_min = c.buy_hit_min,
                    min_mc_sol = c.min_mc_sol,
                    max_mc_sol = c.max_mc_sol,
                    min_largest_buy_sol = c.min_largest_buy_sol,
                    max_largest_buy_sol = c.max_largest_buy_sol,
                    sell_hit_count = c.sell_hit_count,
                    sell_hit_min = c.sell_hit_min,
                    take_profit = c.take_profit,
                    max_hold_s = c.max_hold_s,
                    target_sell_exit = c.target_sell_exit,
                    second_buy_exit = c.second_buy_exit,
                    repeat_entries = c.repeat_entries,
                    "[01 STARTUP] Strategy knobs"
                );
            }
            bot_target::config::StrategyName::V121 => {
                let c = &config.strategy_v121;
                tracing::info!(
                    strategy = plan.name.as_str(),
                    target_wallet = %plan.target_wallet,
                    size_sol = c.size_sol,
                    dust_sol = c.dust_sol,
                    min_entry_s = c.min_entry_s,
                    max_entry_s = c.max_entry_s,
                    buy_hit_count = c.buy_hit_count,
                    buy_hit_min = c.buy_hit_min,
                    min_mc_sol = c.min_mc_sol,
                    max_mc_sol = c.max_mc_sol,
                    min_largest_buy_sol = c.min_largest_buy_sol,
                    max_largest_buy_sol = c.max_largest_buy_sol,
                    min_price_ratio = c.min_price_ratio,
                    max_price_ratio = c.max_price_ratio,
                    min_buy_round_trip_holders = c.min_buy_round_trip_holders,
                    max_buy_round_trip_holders = c.max_buy_round_trip_holders,
                    sell_hit_count = c.sell_hit_count,
                    sell_hit_min = c.sell_hit_min,
                    take_profit = c.take_profit,
                    max_hold_s = c.max_hold_s,
                    target_sell_exit = c.target_sell_exit,
                    second_buy_exit = c.second_buy_exit,
                    repeat_entries = c.repeat_entries,
                    "[01 STARTUP] Strategy knobs"
                );
            }
            bot_target::config::StrategyName::V131 => {
                let c = &config.strategy_v131;
                tracing::info!(
                    strategy = plan.name.as_str(),
                    target_wallet = %plan.target_wallet,
                    size_sol = c.size_sol,
                    dust_sol = c.dust_sol,
                    min_entry_s = c.min_entry_s,
                    max_entry_s = c.max_entry_s,
                    buy_hit_count = c.buy_hit_count,
                    buy_hit_min = c.buy_hit_min,
                    min_mc_sol = c.min_mc_sol,
                    max_mc_sol = c.max_mc_sol,
                    min_largest_buy_sol = c.min_largest_buy_sol,
                    max_largest_buy_sol = c.max_largest_buy_sol,
                    min_price_ratio = c.min_price_ratio,
                    max_price_ratio = c.max_price_ratio,
                    sell_hit_count = c.sell_hit_count,
                    sell_hit_min = c.sell_hit_min,
                    take_profit = c.take_profit,
                    max_hold_s = c.max_hold_s,
                    progress_s = c.progress_s,
                    min_progress_ret = c.min_progress_ret,
                    target_sell_exit = c.target_sell_exit,
                    second_buy_exit = c.second_buy_exit,
                    repeat_entries = c.repeat_entries,
                    "[01 STARTUP] Strategy knobs"
                );
            }
        }
    }

    let rpc = Arc::new(solana_client::nonblocking::rpc_client::RpcClient::new(
        config.helius_rpc_url.clone(),
    ));
    let blockhashes = Arc::new(BlockhashCache::new(
        Arc::clone(&rpc),
        config.blockhash_refresh_ms,
    ));
    blockhashes.spawn();
    let quotes = Arc::new(AltQuote::new(Arc::clone(&rpc)));
    let venues = Arc::new(Venues::new(
        config.compute_unit_limit,
        config.priority_fee_lamports,
        config.helius_tip_lamports,
    ));
    if let Err(err) = venues.refresh_buyback(&rpc).await {
        tracing::warn!(error = %err, "[01 STARTUP] Pump buyback flag unread");
    } else {
        tracing::info!(
            buyback = venues.buyback_enabled(),
            "[01 STARTUP] Pump buyback accounts"
        );
    }
    {
        let venues = Arc::clone(&venues);
        let rpc = Arc::clone(&rpc);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                if let Err(err) = venues.refresh_buyback(&rpc).await {
                    tracing::warn!(error = %err, "[01 STARTUP] Pump buyback refresh failed");
                }
            }
        });
    }
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
            sender: HeliusSender::new(
                config.helius_sender_url.clone(),
                config.helius_sender_swqos_only,
            ),
            journal: Arc::clone(&journal),
            pnl: Arc::clone(&pnl),
            venues: Arc::clone(&venues),
            quotes: Arc::clone(&quotes),
        }));
    }

    let (commands_tx, commands_rx) = mpsc::channel(256);
    let (inbound_tx, inbound_rx) = mpsc::channel(4_096);
    let geyser = GeyserHandle {
        commands: commands_tx,
    };
    let v071_live = config
        .strategy_plans
        .iter()
        .any(|plan| plan.name == StrategyName::V071);
    let v071_cfg = &config.strategy_v071;
    let birth_keep = Duration::from_secs_f64(
        (v071_cfg
            .max_target_age_s
            .max(v071_cfg.min_target_age_s)
            .max(0.0)
            + v071_cfg.max_entry_s.max(0.0)
            + 60.0)
            .max(120.0),
    );
    let tape = v071_live.then(|| Arc::new(V071Tape::new(birth_keep)));
    // Keep the cycle-open buy on the tape through the whole seek clock. A
    // shorter horizon pruned that sig mid-seek; age then fell back to `now`
    // and a too-young coin could clear `min_target_age_s`.
    let tape_keep = Duration::from_secs_f64(
        (v071_cfg.target_volume_s.max(1.0) + v071_cfg.max_entry_s.max(0.0) + 30.0).max(10.0),
    );
    let endpoint = config.vibe_grpc_endpoint.clone();
    let token = config.vibe_grpc_token.clone();
    let venues_geyser = Arc::clone(&venues);
    let tape_for_stream = tape.clone();
    tokio::spawn(async move {
        if let Err(err) = geyser::run(
            endpoint,
            Some(token),
            own_wallets,
            target_wallets,
            venues_geyser,
            inbound_tx,
            commands_rx,
            tape_for_stream,
            tape_keep,
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
        quotes,
        tape,
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
