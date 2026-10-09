use bot_target::config::load_config;
use bot_target::venues::ids::{TOKEN_2022_PROGRAM, TOKEN_PROGRAM};
use bot_target::venues::trade::{burn_instruction, close_account_instruction, compile_trade, harvest_withheld_instruction};
use bot_target::state::PreparedTrade;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signer::Signer;

#[derive(Clone)]
struct Account {
    address: Pubkey,
    mint: Pubkey,
    amount: u64,
    lamports: u64,
    program: Pubkey,
    frozen: bool,
    native: bool,
    withheld: u64,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        println!("Close token accounts owned by the trading wallet and return their rent.\n\nUsage:\n  close-accounts\n  close-accounts --dry-run\n  close-accounts --burn");
        return Ok(());
    }
    let burn = args.iter().any(|arg| arg == "--burn");
    let dry_run = args.iter().any(|arg| arg == "--dry-run");
    let config = load_config()?;
    let rpc = RpcClient::new(config.helius_rpc_url.clone());
    let mut seen = std::collections::HashSet::new();
    for plan in &config.strategy_plans {
        let owner = plan.keypair.pubkey();
        if !seen.insert(owner) {
            continue;
        }
        let accounts = load_accounts(&rpc, &owner).await?;
        let mut steps = Vec::new();
        let mut skipped = 0usize;
        let mut reclaim = 0u64;
        for account in &accounts {
            if account.frozen {
                skipped += 1;
                println!("skip {} reason=frozen", account.mint);
                continue;
            }
            if account.native || account.amount == 0 {
                steps.push(account.clone());
                reclaim += account.lamports;
                continue;
            }
            if burn {
                steps.push(account.clone());
                reclaim += account.lamports;
                continue;
            }
            skipped += 1;
            println!("skip {} reason=balance remaining", account.mint);
        }
        println!(
            "strategy={} wallet={owner} closing={} skipped={skipped} reclaim_lamports={reclaim} dry_run={dry_run}",
            plan.name.as_str(),
            steps.len()
        );
        if dry_run {
            continue;
        }
        for chunk in steps.chunks(4) {
            let mut instructions = Vec::new();
            for account in chunk {
                if account.withheld > 0 {
                    instructions.push(harvest_withheld_instruction(&account.mint, &account.address, &account.program));
                }
                if burn && account.amount > 0 && !account.native {
                    instructions.push(burn_instruction(&account.address, &account.mint, &owner, account.amount, &account.program));
                }
                instructions.push(close_account_instruction(&account.address, &owner, &owner, &account.program));
            }
            let prepared = PreparedTrade {
                instructions,
                payer: owner,
                compute_unit_limit: config.compute_unit_limit.max(200_000),
                priority_fee_lamports: config.priority_fee_lamports,
                tip_lamports: 0,
            };
            let blockhash = rpc.get_latest_blockhash().await?;
            let tx = compile_trade(&prepared, blockhash, &plan.keypair)?;
            let signature = rpc.send_and_confirm_transaction(&tx).await?;
            println!("closed signature={signature}");
        }
    }
    Ok(())
}

async fn load_accounts(rpc: &RpcClient, owner: &Pubkey) -> anyhow::Result<Vec<Account>> {
    let mut out = Vec::new();
    for program in [*TOKEN_PROGRAM, *TOKEN_2022_PROGRAM] {
        let accounts = rpc.get_token_accounts_by_owner(owner, solana_client::rpc_request::TokenAccountsFilter::ProgramId(program)).await?;
        for keyed in accounts {
            let address: Pubkey = keyed.pubkey.parse()?;
            let solana_account_decoder_client_types::UiAccountData::Json(parsed) = keyed.account.data else { continue };
            let info = parsed.parsed.get("info").cloned().unwrap_or(serde_json::Value::Null);
            let mint: Pubkey = info.get("mint").and_then(|v| v.as_str()).unwrap_or_default().parse()?;
            let amount = info.pointer("/tokenAmount/amount").and_then(|v| v.as_str()).and_then(|v| v.parse().ok()).unwrap_or(0);
            let frozen = info.get("state").and_then(|v| v.as_str()) == Some("frozen");
            let native = info.get("isNative").map(|v| v.as_bool().unwrap_or(v.is_number())).unwrap_or(false) || mint == *bot_target::venues::ids::WSOL_MINT;
            let withheld = info
                .get("extensions")
                .and_then(|v| v.as_array())
                .into_iter()
                .flatten()
                .find(|ext| ext.get("extension").and_then(|v| v.as_str()) == Some("transferFeeAmount"))
                .and_then(|ext| ext.pointer("/state/withheldAmount"))
                .and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok())))
                .unwrap_or(0);
            out.push(Account {
                address,
                mint,
                amount,
                lamports: keyed.account.lamports,
                program,
                frozen,
                native,
                withheld,
            });
        }
    }
    Ok(out)
}
