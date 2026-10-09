use super::ids::{
    amm_event_authority, amm_fee_config, amm_global_config, amm_global_volume, amm_user_volume, ata, bonding_curve_pda,
    bonding_curve_v2_pda, coin_creator_vault_authority, creator_vault_pda, pool_v2_pda, pump_event_authority, pump_fee_config,
    pump_global, pump_global_volume, pump_user_volume, HELIUS_TIP_ACCOUNTS, PUMP_AMM_PROGRAM, PUMP_BUYBACK_FEE_RECIPIENTS,
    PUMP_FEE_PROGRAM, PUMP_FEE_RECIPIENTS, PUMP_PROGRAM,
};
use super::quote::{quote_amm_buy, quote_amm_sell, quote_buy_tokens, quote_sell_sol, slippage_down};
use crate::events::{PumpCurveSnapshot, PumpSwapSnapshot};
use crate::state::PreparedTrade;
use anyhow::{bail, Result};
use solana_sdk::hash::Hash;
use solana_sdk::instruction::{AccountMeta, Instruction};
use solana_sdk::message::{v0, VersionedMessage};
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Keypair;
#[allow(deprecated)]
use solana_sdk::system_instruction;
use solana_sdk::transaction::VersionedTransaction;
use solana_sdk::compute_budget::ComputeBudgetInstruction;

const BUY_EXACT_SOL_IN: [u8; 8] = [56, 252, 116, 8, 158, 223, 205, 95];
const SELL: [u8; 8] = [51, 230, 133, 164, 1, 127, 131, 173];
const AMM_BUY: [u8; 8] = [102, 6, 61, 18, 1, 218, 235, 234];

fn pick(list: &[Pubkey]) -> Pubkey {
    list[rand::random::<usize>() % list.len()]
}

fn u64_le(value: u64) -> [u8; 8] {
    value.to_le_bytes()
}

fn meta(pubkey: Pubkey, writable: bool, signer: bool) -> AccountMeta {
    AccountMeta { pubkey, is_signer: signer, is_writable: writable }
}

pub fn create_ata_idempotent(payer: &Pubkey, owner: &Pubkey, mint: &Pubkey, token_program: &Pubkey) -> Instruction {
    let address = ata(owner, mint, token_program);
    Instruction {
        program_id: *super::ids::ATA_PROGRAM,
        accounts: vec![
            meta(*payer, true, true),
            meta(address, true, false),
            meta(*owner, false, false),
            meta(*mint, false, false),
            meta(solana_sdk::system_program::id(), false, false),
            meta(*token_program, false, false),
        ],
        data: vec![1],
    }
}

pub fn build_pump_buy(owner: &Pubkey, mint: &Pubkey, token_program: &Pubkey, lamports: u64, slippage_bps: u64, curve: &PumpCurveSnapshot, compute_unit_limit: u32, priority_fee_lamports: u64, tip_lamports: u64) -> Result<PreparedTrade> {
    let quoted = quote_buy_tokens(lamports as u128, curve);
    if quoted == 0 {
        bail!("buy quotes zero tokens for {mint}");
    }
    let min_tokens = slippage_down(quoted, slippage_bps) as u64;
    let associated_user = ata(owner, mint, token_program);
    let creator: Pubkey = curve.creator.parse()?;
    let fee_recipient = curve.fee_recipient.as_deref().and_then(|value| value.parse().ok()).unwrap_or_else(|| pick(&PUMP_FEE_RECIPIENTS));
    let curve_pda = bonding_curve_pda(mint);
    let mut data = Vec::with_capacity(8 + 8 + 8 + 1);
    data.extend_from_slice(&BUY_EXACT_SOL_IN);
    data.extend_from_slice(&u64_le(lamports));
    data.extend_from_slice(&u64_le(min_tokens));
    data.push(1);
    let buy = Instruction {
        program_id: *PUMP_PROGRAM,
        accounts: vec![
            meta(pump_global(), false, false),
            meta(fee_recipient, true, false),
            meta(*mint, false, false),
            meta(curve_pda, true, false),
            meta(ata(&curve_pda, mint, token_program), true, false),
            meta(associated_user, true, false),
            meta(*owner, true, true),
            meta(solana_sdk::system_program::id(), false, false),
            meta(*token_program, false, false),
            meta(creator_vault_pda(&creator), true, false),
            meta(pump_event_authority(), false, false),
            meta(*PUMP_PROGRAM, false, false),
            meta(pump_global_volume(), false, false),
            meta(pump_user_volume(owner), true, false),
            meta(pump_fee_config(), false, false),
            meta(*PUMP_FEE_PROGRAM, false, false),
            meta(bonding_curve_v2_pda(mint), false, false),
            meta(pick(&PUMP_BUYBACK_FEE_RECIPIENTS), true, false),
        ],
        data,
    };
    Ok(prepared(vec![create_ata_idempotent(owner, owner, mint, token_program), buy], *owner, compute_unit_limit, priority_fee_lamports, tip_lamports))
}

pub fn build_pump_sell(owner: &Pubkey, mint: &Pubkey, token_program: &Pubkey, token_amount: u64, slippage_bps: u64, curve: &PumpCurveSnapshot, compute_unit_limit: u32, priority_fee_lamports: u64, tip_lamports: u64) -> Result<PreparedTrade> {
    let sol_out = quote_sell_sol(token_amount as u128, curve);
    let min_sol = slippage_down(sol_out, slippage_bps) as u64;
    let creator: Pubkey = curve.creator.parse()?;
    let fee_recipient = curve.fee_recipient.as_deref().and_then(|value| value.parse().ok()).unwrap_or_else(|| pick(&PUMP_FEE_RECIPIENTS));
    let curve_pda = bonding_curve_pda(mint);
    let mut data = Vec::with_capacity(24);
    data.extend_from_slice(&SELL);
    data.extend_from_slice(&u64_le(token_amount));
    data.extend_from_slice(&u64_le(min_sol));
    let mut accounts = vec![
        meta(pump_global(), false, false),
        meta(fee_recipient, true, false),
        meta(*mint, false, false),
        meta(curve_pda, true, false),
        meta(ata(&curve_pda, mint, token_program), true, false),
        meta(ata(owner, mint, token_program), true, false),
        meta(*owner, true, true),
        meta(solana_sdk::system_program::id(), false, false),
        meta(creator_vault_pda(&creator), true, false),
        meta(*token_program, false, false),
        meta(pump_event_authority(), false, false),
        meta(*PUMP_PROGRAM, false, false),
        meta(pump_fee_config(), false, false),
        meta(*PUMP_FEE_PROGRAM, false, false),
    ];
    if curve.cashback {
        accounts.push(meta(pump_user_volume(owner), true, false));
    }
    accounts.push(meta(bonding_curve_v2_pda(mint), false, false));
    accounts.push(meta(pick(&PUMP_BUYBACK_FEE_RECIPIENTS), true, false));
    Ok(prepared(
        vec![Instruction { program_id: *PUMP_PROGRAM, accounts, data }],
        *owner,
        compute_unit_limit,
        priority_fee_lamports,
        tip_lamports,
    ))
}

pub fn build_amm_buy(owner: &Pubkey, lamports: u64, slippage_bps: u64, swap: &PumpSwapSnapshot, compute_unit_limit: u32, priority_fee_lamports: u64, tip_lamports: u64) -> Result<PreparedTrade> {
    let (base, max_quote) = quote_amm_buy(lamports as u128, swap, slippage_bps);
    if base == 0 {
        bail!("pumpswap buy quotes zero tokens for {}", swap.base_mint);
    }
    let base_mint: Pubkey = swap.base_mint.parse()?;
    let quote_mint: Pubkey = swap.quote_mint.parse()?;
    let base_program: Pubkey = swap.base_token_program.parse()?;
    let quote_program: Pubkey = swap.quote_token_program.parse()?;
    let pool: Pubkey = swap.pool.parse()?;
    let coin_creator: Pubkey = swap.coin_creator.parse()?;
    let protocol_recipient: Pubkey = swap.protocol_fee_recipient.parse()?;
    let buyback: Pubkey = swap.buyback_fee_recipient.parse()?;
    let user_base = ata(owner, &base_mint, &base_program);
    let user_quote = ata(owner, &quote_mint, &quote_program);
    let vault_authority = coin_creator_vault_authority(&coin_creator);
    let mut data = Vec::with_capacity(25);
    data.extend_from_slice(&AMM_BUY);
    data.extend_from_slice(&u64_le(base as u64));
    data.extend_from_slice(&u64_le(max_quote as u64));
    data.push(1);
    let mut accounts = amm_accounts(owner, &pool, &base_mint, &quote_mint, &user_base, &user_quote, &swap.pool_base_token_account.parse()?, &swap.pool_quote_token_account.parse()?, &protocol_recipient, &base_program, &quote_program, &vault_authority, true);
    if swap.cashback {
        accounts.push(meta(ata(&amm_user_volume(owner), &quote_mint, &quote_program), true, false));
    }
    if coin_creator != Pubkey::default() {
        accounts.push(meta(pool_v2_pda(&base_mint), false, false));
    }
    accounts.push(meta(buyback, false, false));
    accounts.push(meta(ata(&buyback, &quote_mint, &quote_program), true, false));
    let mut instructions = vec![create_ata_idempotent(owner, owner, &base_mint, &base_program)];
    instructions.extend(wrap_wsol(owner, &quote_mint, &quote_program, &user_quote, max_quote as u64));
    instructions.push(Instruction { program_id: *PUMP_AMM_PROGRAM, accounts, data });
    Ok(prepared(instructions, *owner, compute_unit_limit, priority_fee_lamports, tip_lamports))
}

pub fn build_amm_sell(owner: &Pubkey, token_amount: u64, slippage_bps: u64, swap: &PumpSwapSnapshot, compute_unit_limit: u32, priority_fee_lamports: u64, tip_lamports: u64) -> Result<PreparedTrade> {
    let min_quote = quote_amm_sell(token_amount as u128, swap, slippage_bps) as u64;
    let base_mint: Pubkey = swap.base_mint.parse()?;
    let quote_mint: Pubkey = swap.quote_mint.parse()?;
    let base_program: Pubkey = swap.base_token_program.parse()?;
    let quote_program: Pubkey = swap.quote_token_program.parse()?;
    let pool: Pubkey = swap.pool.parse()?;
    let coin_creator: Pubkey = swap.coin_creator.parse()?;
    let protocol_recipient: Pubkey = swap.protocol_fee_recipient.parse()?;
    let buyback: Pubkey = swap.buyback_fee_recipient.parse()?;
    let user_base = ata(owner, &base_mint, &base_program);
    let user_quote = ata(owner, &quote_mint, &quote_program);
    let vault_authority = coin_creator_vault_authority(&coin_creator);
    let mut data = Vec::with_capacity(24);
    data.extend_from_slice(&SELL);
    data.extend_from_slice(&u64_le(token_amount));
    data.extend_from_slice(&u64_le(min_quote));
    let mut accounts = amm_accounts(owner, &pool, &base_mint, &quote_mint, &user_base, &user_quote, &swap.pool_base_token_account.parse()?, &swap.pool_quote_token_account.parse()?, &protocol_recipient, &base_program, &quote_program, &vault_authority, false);
    if swap.cashback {
        accounts.push(meta(ata(&amm_user_volume(owner), &quote_mint, &quote_program), true, false));
        accounts.push(meta(amm_user_volume(owner), true, false));
    }
    if coin_creator != Pubkey::default() {
        accounts.push(meta(pool_v2_pda(&base_mint), false, false));
    }
    accounts.push(meta(buyback, false, false));
    accounts.push(meta(ata(&buyback, &quote_mint, &quote_program), true, false));
    let mut instructions = vec![create_ata_idempotent(owner, owner, &quote_mint, &quote_program)];
    instructions.push(Instruction { program_id: *PUMP_AMM_PROGRAM, accounts, data });
    Ok(prepared(instructions, *owner, compute_unit_limit, priority_fee_lamports, tip_lamports))
}

fn amm_accounts(user: &Pubkey, pool: &Pubkey, base_mint: &Pubkey, quote_mint: &Pubkey, user_base: &Pubkey, user_quote: &Pubkey, pool_base: &Pubkey, pool_quote: &Pubkey, protocol_recipient: &Pubkey, base_program: &Pubkey, quote_program: &Pubkey, vault_authority: &Pubkey, include_volume: bool) -> Vec<AccountMeta> {
    let mut accounts = vec![
        meta(*pool, true, false),
        meta(*user, true, true),
        meta(amm_global_config(), false, false),
        meta(*base_mint, false, false),
        meta(*quote_mint, false, false),
        meta(*user_base, true, false),
        meta(*user_quote, true, false),
        meta(*pool_base, true, false),
        meta(*pool_quote, true, false),
        meta(*protocol_recipient, false, false),
        meta(ata(protocol_recipient, quote_mint, quote_program), true, false),
        meta(*base_program, false, false),
        meta(*quote_program, false, false),
        meta(solana_sdk::system_program::id(), false, false),
        meta(*super::ids::ATA_PROGRAM, false, false),
        meta(amm_event_authority(), false, false),
        meta(*PUMP_AMM_PROGRAM, false, false),
        meta(ata(vault_authority, quote_mint, quote_program), true, false),
        meta(*vault_authority, false, false),
    ];
    if include_volume {
        accounts.push(meta(amm_global_volume(), false, false));
        accounts.push(meta(amm_user_volume(user), true, false));
    }
    accounts.push(meta(amm_fee_config(), false, false));
    accounts.push(meta(*PUMP_FEE_PROGRAM, false, false));
    accounts
}

fn wrap_wsol(owner: &Pubkey, quote_mint: &Pubkey, quote_program: &Pubkey, user_quote: &Pubkey, lamports: u64) -> Vec<Instruction> {
    let create = create_ata_idempotent(owner, owner, quote_mint, quote_program);
    if *quote_mint != *super::ids::WSOL_MINT {
        return vec![create];
    }
    vec![
        create,
        system_instruction::transfer(owner, user_quote, lamports),
        Instruction {
            program_id: *quote_program,
            accounts: vec![meta(*user_quote, true, false)],
            data: vec![17],
        },
    ]
}

fn prepared(instructions: Vec<Instruction>, payer: Pubkey, compute_unit_limit: u32, priority_fee_lamports: u64, tip_lamports: u64) -> PreparedTrade {
    PreparedTrade { instructions, payer, compute_unit_limit, priority_fee_lamports, tip_lamports }
}

pub fn compile_trade(prepared: &PreparedTrade, blockhash: Hash, keypair: &Keypair) -> Result<VersionedTransaction> {
    let micro = (prepared.priority_fee_lamports as u128 * 1_000_000).div_ceil(prepared.compute_unit_limit.max(1) as u128) as u64;
    let mut instructions = vec![
        ComputeBudgetInstruction::set_compute_unit_limit(prepared.compute_unit_limit),
        ComputeBudgetInstruction::set_compute_unit_price(micro),
        system_instruction::transfer(&prepared.payer, &pick(&HELIUS_TIP_ACCOUNTS), prepared.tip_lamports),
    ];
    instructions.extend(prepared.instructions.iter().cloned());
    let message = v0::Message::try_compile(&prepared.payer, &instructions, &[], blockhash)?;
    Ok(VersionedTransaction::try_new(VersionedMessage::V0(message), &[keypair])?)
}

pub fn burn_instruction(account: &Pubkey, mint: &Pubkey, owner: &Pubkey, amount: u64, program: &Pubkey) -> Instruction {
    let mut data = vec![8];
    data.extend_from_slice(&amount.to_le_bytes());
    Instruction {
        program_id: *program,
        accounts: vec![meta(*account, true, false), meta(*mint, true, false), meta(*owner, false, true)],
        data,
    }
}

pub fn close_account_instruction(account: &Pubkey, destination: &Pubkey, owner: &Pubkey, program: &Pubkey) -> Instruction {
    Instruction {
        program_id: *program,
        accounts: vec![meta(*account, true, false), meta(*destination, true, false), meta(*owner, false, true)],
        data: vec![9],
    }
}

pub fn harvest_withheld_instruction(mint: &Pubkey, account: &Pubkey, program: &Pubkey) -> Instruction {
    Instruction {
        program_id: *program,
        accounts: vec![meta(*mint, true, false), meta(*account, true, false)],
        data: vec![26, 4],
    }
}
