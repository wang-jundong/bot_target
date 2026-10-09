use bot_target::config::load_config;
use bot_target::events::{mono_ms, PumpCurveSnapshot, PumpSwapSnapshot};
use bot_target::helius::HeliusSender;
use bot_target::venues::ids::{
    amm_global_config, bonding_curve_pda, canonical_pool_pda, parse_amm_global_config, parse_amm_pool,
    PUMP_BUYBACK_FEE_RECIPIENTS, TOKEN_2022_PROGRAM, TOKEN_PROGRAM, WSOL_MINT,
};
use bot_target::venues::trade::{build_amm_sell, build_pump_sell, compile_trade};
use base64::Engine;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_client::rpc_config::RpcSendTransactionConfig;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signer::Signer;
use solana_sdk::signature::Signature;
use std::str::FromStr;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        println!(
            "Sell every pump token the trading wallet still holds.\n\nUsage:\n  sell-all\n  sell-all --dry-run\n  sell-all --mint <mint>\n\nBonding-curve coins sell on pump; migrated coins sell on pumpswap. Uses Helius Sender + tip like live trading."
        );
        return Ok(());
    }
    let dry_run = args.iter().any(|arg| arg == "--dry-run");
    let mut only = Vec::new();
    let mut index = 0;
    while index < args.len() {
        if args[index] == "--mint" {
            let value = args.get(index + 1).ok_or_else(|| anyhow::anyhow!("--mint needs a mint address"))?;
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
    let rpc = RpcClient::new(config.helius_rpc_url.clone());
    let sender = HeliusSender::new(config.helius_sender_url.clone(), config.helius_sender_swqos_only);
    let mut failed = 0usize;
    let mut seen = std::collections::HashSet::new();
    for plan in &config.strategy_plans {
        let owner = plan.keypair.pubkey();
        if !seen.insert(owner) {
            continue;
        }
        println!("wallet strategy={} owner={owner}", plan.name.as_str());
        for program in [*TOKEN_PROGRAM, *TOKEN_2022_PROGRAM] {
            let accounts = rpc
                .get_token_accounts_by_owner(&owner, solana_client::rpc_request::TokenAccountsFilter::ProgramId(program))
                .await?;
            for keyed in accounts {
                let solana_account_decoder_client_types::UiAccountData::Json(parsed) = keyed.account.data else {
                    continue;
                };
                let info = parsed.parsed.get("info").cloned().unwrap_or(serde_json::Value::Null);
                let Some(mint_str) = info.get("mint").and_then(|v| v.as_str()) else { continue };
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
                let Some(curve) = fetch_curve(&rpc, &mint).await? else {
                    println!("skip {mint_str} reason=no pump bonding curve");
                    if only.iter().any(|m| m == mint_str) {
                        failed += 1;
                    }
                    continue;
                };
                let venue = if curve.0 { "pumpswap" } else { "pump" };
                println!("sell {mint_str} amount={amount} venue={venue}{}", if dry_run { " dry-run" } else { "" });
                if dry_run {
                    continue;
                }
                let prepared = if curve.0 {
                    let swap = fetch_amm_swap(&rpc, &mint, &program).await?;
                    build_amm_sell(
                        &owner,
                        amount,
                        config.sell_slippage_bps,
                        &swap,
                        config.compute_unit_limit.max(400_000),
                        config.priority_fee_lamports,
                        config.helius_tip_lamports,
                    )?
                } else {
                    build_pump_sell(
                        &owner,
                        &mint,
                        &program,
                        amount,
                        config.sell_slippage_bps,
                        &curve.1,
                        config.compute_unit_limit.max(400_000),
                        config.priority_fee_lamports,
                        config.helius_tip_lamports,
                    )?
                };
                let blockhash = rpc.get_latest_blockhash().await?;
                let tx = compile_trade(&prepared, blockhash, &plan.keypair)?;
                let raw = bincode::serialize(&tx)?;
                let encoded = base64::engine::general_purpose::STANDARD.encode(&raw);
                let signature = match sender.send(&encoded, mono_ms()).await {
                    Ok((signature, _)) => signature,
                    Err(err) => {
                        eprintln!("Helius sender failed; falling back to RPC: {err}");
                        match rpc
                            .send_transaction_with_config(
                                &tx,
                                RpcSendTransactionConfig {
                                    skip_preflight: false,
                                    max_retries: Some(2),
                                    ..Default::default()
                                },
                            )
                            .await
                        {
                            Ok(sig) => sig.to_string(),
                            Err(rpc_err) => {
                                failed += 1;
                                println!("fail mint={mint_str} error={err}; {rpc_err}");
                                continue;
                            }
                        }
                    }
                };
                match rpc.confirm_transaction(&Signature::from_str(&signature)?).await {
                    Ok(true) => println!("ok mint={mint_str} signature={signature}"),
                    Ok(false) => {
                        failed += 1;
                        println!("fail mint={mint_str} signature={signature} reason=not confirmed");
                    }
                    Err(err) => {
                        failed += 1;
                        println!("fail mint={mint_str} signature={signature} error={err}");
                    }
                }
            }
        }
    }
    if failed > 0 {
        std::process::exit(1);
    }
    Ok(())
}

async fn fetch_curve(rpc: &RpcClient, mint: &Pubkey) -> anyhow::Result<Option<(bool, PumpCurveSnapshot)>> {
    let account = rpc.get_account(&bonding_curve_pda(mint)).await.ok();
    let Some(account) = account else { return Ok(None) };
    if account.data.len() < 8 + 8 * 5 + 1 + 32 {
        return Ok(None);
    }
    let data = &account.data[8..];
    let virtual_token = u64::from_le_bytes(data[0..8].try_into().unwrap());
    let virtual_quote = u64::from_le_bytes(data[8..16].try_into().unwrap());
    let real_token = u64::from_le_bytes(data[16..24].try_into().unwrap());
    let complete = data[40] != 0;
    let creator = Pubkey::new_from_array(data[41..73].try_into().unwrap());
    let cashback = account.data.len() > 8 + 74 && data[74] != 0;
    Ok(Some((
        complete,
        PumpCurveSnapshot {
            virtual_quote_reserves: virtual_quote as u128,
            virtual_token_reserves: virtual_token as u128,
            real_token_reserves: real_token as u128,
            creator: creator.to_string(),
            mayhem_mode: account.data.len() > 8 + 73 && data[73] != 0,
            quote_mint: Some(WSOL_MINT.to_string()),
            protocol_fee_bps: 100,
            creator_fee_bps: if creator == Pubkey::default() { 0 } else { 30 },
            fee_recipient: None,
            cashback,
        },
    )))
}

async fn fetch_amm_swap(rpc: &RpcClient, mint: &Pubkey, base_program: &Pubkey) -> anyhow::Result<PumpSwapSnapshot> {
    let pool = canonical_pool_pda(mint, &WSOL_MINT);
    let pool_account = rpc.get_account(&pool).await?;
    let parsed = parse_amm_pool(&pool_account.data).ok_or_else(|| anyhow::anyhow!("unable to parse pumpswap pool for {mint}"))?;
    let global_account = rpc.get_account(&amm_global_config()).await?;
    let global = parse_amm_global_config(&global_account.data).ok_or_else(|| anyhow::anyhow!("unable to parse pumpswap global config"))?;
    let base_bal = token_amount(rpc, &parsed.pool_base_token_account).await?;
    let quote_bal = token_amount(rpc, &parsed.pool_quote_token_account).await?;
    let protocol = global
        .protocol_fee_recipients
        .into_iter()
        .find(|k| *k != Pubkey::default())
        .unwrap_or(global.protocol_fee_recipients[0]);
    let buyback = PUMP_BUYBACK_FEE_RECIPIENTS.first().copied().unwrap_or_default();
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
