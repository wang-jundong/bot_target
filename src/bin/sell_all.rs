use base64::Engine;
use bot_target::alt_quote::{ALT_COMPUTE_UNIT_LIMIT, AltQuote};
use bot_target::config::load_config;
use bot_target::events::{PumpCurveSnapshot, PumpSwapSnapshot, mono_ms};
use bot_target::helius::HeliusSender;
use bot_target::state::PreparedTrade;
use bot_target::venues::ids::{
    PUMP_BUYBACK_FEE_RECIPIENTS, TOKEN_2022_PROGRAM, TOKEN_PROGRAM, WSOL_MINT, amm_global_config,
    bonding_curve_pda, canonical_pool_pda, is_sol_quote, parse_amm_global_config, parse_amm_pool,
    parse_pump_buyback, pump_global,
};
use bot_target::venues::launchpad::{
    self, LaunchpadPoolState, POOL_MINT_A_OFFSET, PROGRAM_ID as LAUNCHPAD_PROGRAM,
};
use bot_target::venues::trade::{
    build_amm_sell, build_pump_sell, build_pump_sell_alt, compile_trade, compile_v0_ixs,
    transaction_wire_bytes,
};
use solana_account_decoder_client_types::UiAccountEncoding;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_client::rpc_config::{RpcAccountInfoConfig, RpcProgramAccountsConfig, RpcSendTransactionConfig};
use solana_client::rpc_filter::{Memcmp, RpcFilterType};
use solana_sdk::instruction::Instruction;
use solana_sdk::message::AddressLookupTableAccount;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signature};
use solana_sdk::signer::Signer;
use solana_sdk::transaction::VersionedTransaction;
use solana_transaction_status_client_types::TransactionConfirmationStatus;
use std::collections::HashSet;
use std::str::FromStr;
use std::sync::Arc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        println!(
            "Sell every pump / LaunchLab token the trading wallet still holds.\n\n\
Usage:\n  sell-all\n  sell-all --dry-run\n  sell-all --mint <mint>\n\n\
Matches live non-SOL exits:\n\
  pump.fun active SOL     → pump sell\n\
  pump.fun active non-SOL → SellV2 mint→quote, then Jupiter quote→SOL\n\
  pump.fun migrated SOL   → PumpSwap\n\
  pump.fun migrated alt   → Jupiter mint→SOL\n\
  LaunchLab active        → LaunchLab sell (alt: + Jupiter reclaim)\n\
  LaunchLab completed     → Jupiter mint→SOL"
        );
        return Ok(());
    }
    let dry_run = args.iter().any(|arg| arg == "--dry-run");
    let mut only = Vec::new();
    let mut index = 0;
    while index < args.len() {
        if args[index] == "--mint" {
            let value = args
                .get(index + 1)
                .ok_or_else(|| anyhow::anyhow!("--mint needs a mint address"))?;
            only.push(value.parse::<Pubkey>()?.to_string());
            index += 2;
            continue;
        }
        if args[index] != "--dry-run" {
            anyhow::bail!("Unknown argument: {}", args[index]);
        }
        index += 1;
    }
    let config = load_config()?;
    let rpc = Arc::new(RpcClient::new(config.helius_rpc_url.clone()));
    let quotes = AltQuote::new(Arc::clone(&rpc));
    let sender = HeliusSender::new(
        config.helius_sender_url.clone(),
        config.helius_sender_swqos_only,
    );
    let buyback = rpc
        .get_account(&pump_global())
        .await
        .ok()
        .and_then(|account| parse_pump_buyback(&account.data))
        .unwrap_or(true);
    let mut failed = 0usize;
    let mut seen_wallets = HashSet::new();
    for plan in &config.strategy_plans {
        let owner = plan.keypair.pubkey();
        if !seen_wallets.insert(owner) {
            continue;
        }
        println!("wallet strategy={} owner={owner}", plan.name.as_str());
        let mut reclaim_quotes: HashSet<Pubkey> = HashSet::new();
        for program in [*TOKEN_PROGRAM, *TOKEN_2022_PROGRAM] {
            let accounts = rpc
                .get_token_accounts_by_owner(
                    &owner,
                    solana_client::rpc_request::TokenAccountsFilter::ProgramId(program),
                )
                .await?;
            for keyed in accounts {
                let solana_account_decoder_client_types::UiAccountData::Json(parsed) =
                    keyed.account.data
                else {
                    continue;
                };
                let info = parsed
                    .parsed
                    .get("info")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                let Some(mint_str) = info.get("mint").and_then(|v| v.as_str()) else {
                    continue;
                };
                if mint_str == WSOL_MINT.to_string() {
                    println!("skip {mint_str} reason=wrapped SOL");
                    continue;
                }
                if !only.is_empty() && !only.iter().any(|mint| mint == mint_str) {
                    continue;
                }
                let amount: u64 = info
                    .pointer("/tokenAmount/amount")
                    .and_then(|v| v.as_str())
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
                if amount == 0 {
                    continue;
                }
                let mint: Pubkey = mint_str.parse()?;
                let pump = fetch_curve(&rpc, &mint).await?;
                let launchpad = if pump.is_none() {
                    fetch_launchpad(&rpc, &mint).await?
                } else {
                    None
                };
                if pump.is_none() && launchpad.is_none() {
                    println!("skip {mint_str} reason=no pump/launchlab pool");
                    if only.iter().any(|m| m == mint_str) {
                        failed += 1;
                    }
                    continue;
                }
                let cu_base = config.compute_unit_limit.max(400_000);
                let result = if let Some((complete, curve)) = pump {
                    let alt_quote = curve
                        .quote_mint
                        .as_deref()
                        .filter(|q| !is_sol_quote(Some(*q)))
                        .and_then(|q| q.parse::<Pubkey>().ok());
                    let venue = match (complete, alt_quote.is_some()) {
                        (true, true) => "jupiter-flatten",
                        (true, false) => "pumpswap",
                        (false, true) => "pump-alt",
                        (false, false) => "pump",
                    };
                    println!(
                        "sell {mint_str} amount={amount} venue={venue}{}{}",
                        alt_quote
                            .map(|q| format!(" quote={q}"))
                            .unwrap_or_default(),
                        if dry_run { " dry-run" } else { "" }
                    );
                    if dry_run {
                        if let Some(quote_mint) = alt_quote.filter(|_| !complete) {
                            reclaim_quotes.insert(quote_mint);
                        }
                        continue;
                    }
                    let cu = cu_base.max(if alt_quote.is_some() {
                        ALT_COMPUTE_UNIT_LIMIT
                    } else {
                        0
                    });
                    if complete {
                        if alt_quote.is_some() {
                            sell_jupiter_flatten(
                                &rpc,
                                &sender,
                                &quotes,
                                &owner,
                                &plan.keypair,
                                &mint,
                                amount,
                                cu,
                                config.priority_fee_lamports,
                                config.helius_tip_lamports,
                            )
                            .await
                        } else {
                            let swap = fetch_amm_swap(&rpc, &mint, &program).await?;
                            let prepared = build_amm_sell(
                                &owner,
                                amount,
                                config.sell_slippage_bps,
                                &swap,
                                cu,
                                config.priority_fee_lamports,
                                config.helius_tip_lamports,
                            )?;
                            let blockhash = rpc.get_latest_blockhash().await?;
                            let tx = compile_trade(&prepared, blockhash, &plan.keypair)?;
                            land_tx(&rpc, &sender, &tx).await
                        }
                    } else if let Some(quote_mint) = alt_quote {
                        reclaim_quotes.insert(quote_mint);
                        let quote_program = quotes.token_program(&quote_mint).await?;
                        let prepared = build_pump_sell_alt(
                            &owner,
                            &mint,
                            &program,
                            &quote_mint,
                            &quote_program,
                            amount,
                            config.sell_slippage_bps,
                            &curve,
                            cu,
                            config.priority_fee_lamports,
                            config.helius_tip_lamports,
                        )?;
                        let blockhash = rpc.get_latest_blockhash().await?;
                        let tx = compile_trade(&prepared, blockhash, &plan.keypair)?;
                        land_tx(&rpc, &sender, &tx).await
                    } else {
                        let prepared = build_pump_sell(
                            &owner,
                            &mint,
                            &program,
                            amount,
                            config.sell_slippage_bps,
                            &curve,
                            cu,
                            config.priority_fee_lamports,
                            config.helius_tip_lamports,
                            buyback,
                        )?;
                        let blockhash = rpc.get_latest_blockhash().await?;
                        let tx = compile_trade(&prepared, blockhash, &plan.keypair)?;
                        land_tx(&rpc, &sender, &tx).await
                    }
                } else if let Some(pool) = launchpad {
                    // Same as live: LaunchLab mint→quote (or SOL), completed → Jupiter.
                    let quote_mint = pool.mint_b;
                    let alt = !is_sol_quote(Some(&quote_mint.to_string()));
                    let complete = pool.completed();
                    let venue = match (complete, alt) {
                        (true, _) => "jupiter-flatten",
                        (false, true) => "launchlab-alt",
                        (false, false) => "launchlab",
                    };
                    println!(
                        "sell {mint_str} amount={amount} venue={venue}{}{}",
                        if alt {
                            format!(" quote={quote_mint}")
                        } else {
                            String::new()
                        },
                        if dry_run { " dry-run" } else { "" }
                    );
                    if dry_run {
                        if alt && !complete {
                            reclaim_quotes.insert(quote_mint);
                        }
                        continue;
                    }
                    let cu = cu_base.max(ALT_COMPUTE_UNIT_LIMIT);
                    if complete {
                        sell_jupiter_flatten(
                            &rpc,
                            &sender,
                            &quotes,
                            &owner,
                            &plan.keypair,
                            &mint,
                            amount,
                            cu,
                            config.priority_fee_lamports,
                            config.helius_tip_lamports,
                        )
                        .await
                    } else {
                        if alt {
                            reclaim_quotes.insert(quote_mint);
                        }
                        let quote_program = if alt {
                            quotes.token_program(&quote_mint).await?
                        } else {
                            *TOKEN_PROGRAM
                        };
                        let instructions = launchpad::sell_instructions(
                            &owner,
                            &pool,
                            &program,
                            &quote_program,
                            amount,
                            config.sell_slippage_bps,
                        )?;
                        let prepared = PreparedTrade {
                            instructions,
                            payer: owner,
                            compute_unit_limit: cu,
                            priority_fee_lamports: config.priority_fee_lamports,
                            tip_lamports: config.helius_tip_lamports,
                        };
                        let blockhash = rpc.get_latest_blockhash().await?;
                        let tx = compile_trade(&prepared, blockhash, &plan.keypair)?;
                        land_tx(&rpc, &sender, &tx).await
                    }
                } else {
                    unreachable!()
                };
                match result {
                    Ok(signature) => println!("ok mint={mint_str} signature={signature}"),
                    Err(err) => {
                        failed += 1;
                        println!("fail mint={mint_str} error={err}");
                    }
                }
            }
        }
        // Reclaim leftover quote for --mint targets even when the bag is already flat.
        for mint_str in &only {
            let Ok(mint) = mint_str.parse::<Pubkey>() else {
                continue;
            };
            if let Ok(Some((complete, curve))) = fetch_curve(&rpc, &mint).await {
                if !complete {
                    if let Some(quote) = curve
                        .quote_mint
                        .as_deref()
                        .filter(|q| !is_sol_quote(Some(*q)))
                        .and_then(|q| q.parse::<Pubkey>().ok())
                    {
                        reclaim_quotes.insert(quote);
                    }
                }
            } else if let Ok(Some(pool)) = fetch_launchpad(&rpc, &mint).await {
                if !pool.completed() && !is_sol_quote(Some(&pool.mint_b.to_string())) {
                    reclaim_quotes.insert(pool.mint_b);
                }
            }
        }
        for quote_mint in &reclaim_quotes {
            if dry_run {
                println!("reclaim quote={quote_mint} dry-run");
                continue;
            }
            println!("reclaim quote={quote_mint}");
            match reclaim_quote(
                &rpc,
                &sender,
                &quotes,
                &owner,
                &plan.keypair,
                quote_mint,
                config.compute_unit_limit.max(ALT_COMPUTE_UNIT_LIMIT),
                config.priority_fee_lamports,
                config.helius_tip_lamports,
            )
            .await
            {
                Ok(None) => println!("reclaim skip quote={quote_mint} reason=empty"),
                Ok(Some((signature, out))) => {
                    println!("ok reclaim quote={quote_mint} signature={signature} out≈{out}")
                }
                Err(err) => {
                    failed += 1;
                    println!("fail reclaim quote={quote_mint} error={err}");
                }
            }
        }
    }
    if failed > 0 {
        std::process::exit(1);
    }
    Ok(())
}

async fn sell_jupiter_flatten(
    rpc: &RpcClient,
    sender: &HeliusSender,
    quotes: &AltQuote,
    owner: &Pubkey,
    keypair: &Keypair,
    mint: &Pubkey,
    amount: u64,
    compute_unit_limit: u32,
    priority_fee_lamports: u64,
    tip_lamports: u64,
) -> anyhow::Result<String> {
    let build = quotes.flatten_mint_to_sol(owner, mint, amount).await?;
    land_jupiter(
        rpc,
        sender,
        owner,
        keypair,
        &build.instructions,
        &build.alts,
        compute_unit_limit,
        priority_fee_lamports,
        tip_lamports,
    )
    .await
}

async fn reclaim_quote(
    rpc: &RpcClient,
    sender: &HeliusSender,
    quotes: &AltQuote,
    owner: &Pubkey,
    keypair: &Keypair,
    quote_mint: &Pubkey,
    compute_unit_limit: u32,
    priority_fee_lamports: u64,
    tip_lamports: u64,
) -> anyhow::Result<Option<(String, u64)>> {
    let Some(build) = quotes.reclaim_build(owner, quote_mint).await? else {
        return Ok(None);
    };
    let signature = land_jupiter(
        rpc,
        sender,
        owner,
        keypair,
        &build.instructions,
        &build.alts,
        compute_unit_limit,
        priority_fee_lamports,
        tip_lamports,
    )
    .await?;
    Ok(Some((signature, build.out_amount)))
}

async fn land_jupiter(
    rpc: &RpcClient,
    sender: &HeliusSender,
    owner: &Pubkey,
    keypair: &Keypair,
    instructions: &[Instruction],
    alts: &[AddressLookupTableAccount],
    compute_unit_limit: u32,
    priority_fee_lamports: u64,
    tip_lamports: u64,
) -> anyhow::Result<String> {
    let budget: Pubkey = "ComputeBudget111111111111111111111111111111"
        .parse()
        .expect("compute budget program");
    let instructions: Vec<_> = instructions
        .iter()
        .filter(|ix| ix.program_id != budget)
        .cloned()
        .collect();
    let blockhash = rpc.get_latest_blockhash().await?;
    let tx = compile_v0_ixs(
        owner,
        &instructions,
        alts,
        blockhash,
        keypair,
        compute_unit_limit,
        priority_fee_lamports,
        tip_lamports,
    )?;
    land_tx(rpc, sender, &tx).await
}

async fn land_tx(
    rpc: &RpcClient,
    sender: &HeliusSender,
    tx: &VersionedTransaction,
) -> anyhow::Result<String> {
    let raw = transaction_wire_bytes(tx)?;
    let encoded = base64::engine::general_purpose::STANDARD.encode(&raw);
    let signature = match sender.send(&encoded, mono_ms()).await {
        Ok((signature, _)) => signature,
        Err(err) => {
            eprintln!("Helius sender failed; falling back to RPC: {err}");
            rpc.send_transaction_with_config(
                tx,
                RpcSendTransactionConfig {
                    skip_preflight: false,
                    max_retries: Some(2),
                    ..Default::default()
                },
            )
            .await
            .map(|sig| sig.to_string())
            .map_err(|rpc_err| anyhow::anyhow!("{err}; {rpc_err}"))?
        }
    };
    let parsed = Signature::from_str(&signature)?;
    for _ in 0..60 {
        let status = rpc
            .get_signature_statuses(&[parsed])
            .await
            .ok()
            .and_then(|r| r.value.into_iter().next().flatten());
        match status {
            Some(status) if status.err.is_some() => {
                return Err(anyhow::anyhow!(
                    "signature={signature} err={:?}",
                    status.err
                ));
            }
            Some(status)
                if matches!(
                    status.confirmation_status,
                    Some(
                        TransactionConfirmationStatus::Confirmed
                            | TransactionConfirmationStatus::Finalized
                    )
                ) =>
            {
                return Ok(signature);
            }
            _ => tokio::time::sleep(std::time::Duration::from_millis(500)).await,
        }
    }
    Err(anyhow::anyhow!(
        "signature={signature} reason=not confirmed"
    ))
}

/// Discover a Stonk LaunchLab pool for `mint` via getProgramAccounts on mint_a.
async fn fetch_launchpad(
    rpc: &RpcClient,
    mint: &Pubkey,
) -> anyhow::Result<Option<LaunchpadPoolState>> {
    let config = RpcProgramAccountsConfig {
        filters: Some(vec![RpcFilterType::Memcmp(Memcmp::new_raw_bytes(
            POOL_MINT_A_OFFSET,
            mint.to_bytes().to_vec(),
        ))]),
        account_config: RpcAccountInfoConfig {
            encoding: Some(UiAccountEncoding::Base64),
            ..RpcAccountInfoConfig::default()
        },
        ..RpcProgramAccountsConfig::default()
    };
    let accounts = match rpc
        .get_program_ui_accounts_with_config(&LAUNCHPAD_PROGRAM, config)
        .await
    {
        Ok(accounts) => accounts,
        Err(err) => {
            eprintln!("launchlab GPA failed for {mint}: {err}");
            return Ok(None);
        }
    };
    let mut best: Option<LaunchpadPoolState> = None;
    for (key, ui) in accounts {
        let Some(account) = ui.to_account() else {
            continue;
        };
        let Ok(pool) = LaunchpadPoolState::from_account_data(&key, &account.data) else {
            continue;
        };
        if pool.mint_a != *mint {
            continue;
        }
        if !launchpad::is_stonk_platform(&pool.platform_id) {
            continue;
        }
        // Prefer an active pool when several match.
        match &best {
            None => best = Some(pool),
            Some(prev) if prev.completed() && !pool.completed() => best = Some(pool),
            _ => {}
        }
    }
    Ok(best)
}

async fn fetch_curve(
    rpc: &RpcClient,
    mint: &Pubkey,
) -> anyhow::Result<Option<(bool, PumpCurveSnapshot)>> {
    let account = rpc.get_account(&bonding_curve_pda(mint)).await.ok();
    let Some(account) = account else {
        return Ok(None);
    };
    if account.data.len() < 8 + 8 * 5 + 1 + 32 {
        return Ok(None);
    }
    let data = &account.data[8..];
    let virtual_token = u64::from_le_bytes(data[0..8].try_into().unwrap());
    let virtual_quote = u64::from_le_bytes(data[8..16].try_into().unwrap());
    let real_token = u64::from_le_bytes(data[16..24].try_into().unwrap());
    let complete = data[40] != 0;
    let creator = Pubkey::new_from_array(data[41..73].try_into().unwrap());
    let mayhem_mode = data.get(73).copied().unwrap_or(0) != 0;
    let cashback = data.get(74).copied().unwrap_or(0) != 0;
    // Layout after creator: mayhem(u8) + cashback(u8) + quote_mint(Pubkey).
    let quote_mint = data
        .get(75..107)
        .and_then(|bytes| bytes.try_into().ok())
        .map(|bytes| Pubkey::new_from_array(bytes).to_string())
        .unwrap_or_else(|| WSOL_MINT.to_string());
    Ok(Some((
        complete,
        PumpCurveSnapshot {
            virtual_quote_reserves: virtual_quote as u128,
            virtual_token_reserves: virtual_token as u128,
            real_token_reserves: real_token as u128,
            creator: creator.to_string(),
            mayhem_mode,
            quote_mint: Some(quote_mint),
            protocol_fee_bps: 100,
            creator_fee_bps: if creator == Pubkey::default() { 0 } else { 30 },
            fee_recipient: None,
            cashback,
        },
    )))
}

async fn fetch_amm_swap(
    rpc: &RpcClient,
    mint: &Pubkey,
    base_program: &Pubkey,
) -> anyhow::Result<PumpSwapSnapshot> {
    let pool = canonical_pool_pda(mint, &WSOL_MINT);
    let pool_account = rpc.get_account(&pool).await?;
    let parsed = parse_amm_pool(&pool_account.data)
        .ok_or_else(|| anyhow::anyhow!("unable to parse pumpswap pool for {mint}"))?;
    let global_account = rpc.get_account(&amm_global_config()).await?;
    let global = parse_amm_global_config(&global_account.data)
        .ok_or_else(|| anyhow::anyhow!("unable to parse pumpswap global config"))?;
    let base_bal = token_amount(rpc, &parsed.pool_base_token_account).await?;
    let quote_bal = token_amount(rpc, &parsed.pool_quote_token_account).await?;
    let protocol = global
        .protocol_fee_recipients
        .into_iter()
        .find(|k| *k != Pubkey::default())
        .unwrap_or(global.protocol_fee_recipients[0]);
    let buyback = PUMP_BUYBACK_FEE_RECIPIENTS
        .first()
        .copied()
        .unwrap_or_default();
    Ok(PumpSwapSnapshot {
        pool: pool.to_string(),
        base_mint: parsed.base_mint.to_string(),
        quote_mint: parsed.quote_mint.to_string(),
        pool_base_token_account: parsed.pool_base_token_account.to_string(),
        pool_quote_token_account: parsed.pool_quote_token_account.to_string(),
        base_token_program: base_program.to_string(),
        quote_token_program: TOKEN_PROGRAM.to_string(),
        base_reserve: base_bal as u128,
        quote_reserve: quote_bal as u128,
        virtual_quote_reserves: parsed.virtual_quote_reserves,
        lp_fee_bps: global.lp_fee_bps as u128,
        protocol_fee_bps: global.protocol_fee_bps as u128,
        coin_creator_fee_bps: global.coin_creator_fee_bps as u128,
        coin_creator: parsed.coin_creator.to_string(),
        protocol_fee_recipient: protocol.to_string(),
        buyback_fee_recipient: buyback.to_string(),
        cashback: parsed.cashback,
    })
}

async fn token_amount(rpc: &RpcClient, account: &Pubkey) -> anyhow::Result<u64> {
    let data = rpc.get_account_data(account).await?;
    if data.len() < 72 {
        return Ok(0);
    }
    Ok(u64::from_le_bytes(data[64..72].try_into()?))
}
