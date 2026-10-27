use super::ids::{
    HELIUS_TIP_ACCOUNTS, PUMP_AMM_PROGRAM, PUMP_BUYBACK_FEE_RECIPIENTS, PUMP_FEE_PROGRAM,
    PUMP_PROGRAM, amm_event_authority, amm_fee_config, amm_global_config, amm_global_volume,
    amm_user_volume, ata, bonding_curve_pda, bonding_curve_v2_pda, coin_creator_vault_authority,
    creator_vault_pda, is_pump_fee_recipient, pool_v2_pda, pump_event_authority, pump_fee_config,
    pump_fee_recipients, pump_global, pump_global_volume, pump_user_volume,
};
use super::quote::{
    quote_amm_buy, quote_amm_sell, quote_buy_tokens, quote_sell_sol, slippage_down,
};
use crate::events::{PumpCurveSnapshot, PumpSwapSnapshot};
use crate::state::PreparedTrade;
use anyhow::{Result, bail};
use solana_compute_budget_interface::ComputeBudgetInstruction;
use solana_sdk::hash::Hash;
use solana_sdk::instruction::{AccountMeta, Instruction};
use solana_sdk::message::{VersionedMessage, v0, v1};
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Keypair;
use solana_sdk::transaction::VersionedTransaction;
use solana_system_interface::instruction as system_instruction;
use solana_system_interface::program as system_program;

const BUY_EXACT_SOL_IN: [u8; 8] = [56, 252, 116, 8, 158, 223, 205, 95];
const SELL: [u8; 8] = [51, 230, 133, 164, 1, 127, 131, 173];
const AMM_BUY: [u8; 8] = [102, 6, 61, 18, 1, 218, 235, 234];

fn pick(list: &[Pubkey]) -> Pubkey {
    list[rand::random::<usize>() % list.len()]
}

/// Only allowlisted fee recipients are valid. Streamed `curve.fee_recipient` can be
/// wrong (e.g. native SOL quote mint `11111…`), which makes pump return ConstraintMut.
fn resolve_fee_recipient(curve: &PumpCurveSnapshot) -> Pubkey {
    let allowed = pump_fee_recipients(curve.mayhem_mode);
    if let Some(candidate) = curve
        .fee_recipient
        .as_deref()
        .and_then(|value| value.parse().ok())
    {
        if is_pump_fee_recipient(&candidate, curve.mayhem_mode) {
            return candidate;
        }
    }
    pick(allowed)
}

fn resolve_buyback_fee_recipient() -> Pubkey {
    pick(&PUMP_BUYBACK_FEE_RECIPIENTS)
}

fn u64_le(value: u64) -> [u8; 8] {
    value.to_le_bytes()
}

fn meta(pubkey: Pubkey, writable: bool, signer: bool) -> AccountMeta {
    AccountMeta {
        pubkey,
        is_signer: signer,
        is_writable: writable,
    }
}

pub fn create_ata_idempotent(
    payer: &Pubkey,
    owner: &Pubkey,
    mint: &Pubkey,
    token_program: &Pubkey,
) -> Instruction {
    let address = ata(owner, mint, token_program);
    Instruction {
        program_id: *super::ids::ATA_PROGRAM,
        accounts: vec![
            meta(*payer, true, true),
            meta(address, true, false),
            meta(*owner, false, false),
            meta(*mint, false, false),
            meta(system_program::id(), false, false),
            meta(*token_program, false, false),
        ],
        data: vec![1],
    }
}

pub fn build_pump_buy(
    owner: &Pubkey,
    mint: &Pubkey,
    token_program: &Pubkey,
    lamports: u64,
    slippage_bps: u64,
    curve: &PumpCurveSnapshot,
    compute_unit_limit: u32,
    priority_fee_lamports: u64,
    tip_lamports: u64,
) -> Result<PreparedTrade> {
    let quoted = quote_buy_tokens(lamports as u128, curve);
    if quoted == 0 {
        bail!("buy quotes zero tokens for {mint}");
    }
    let min_tokens = slippage_down(quoted, slippage_bps) as u64;
    let associated_user = ata(owner, mint, token_program);
    let creator: Pubkey = curve.creator.parse()?;
    let fee_recipient = resolve_fee_recipient(curve);
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
            meta(system_program::id(), false, false),
            meta(*token_program, false, false),
            meta(creator_vault_pda(&creator), true, false),
            meta(pump_event_authority(), false, false),
            meta(*PUMP_PROGRAM, false, false),
            meta(pump_global_volume(), false, false),
            meta(pump_user_volume(owner), true, false),
            meta(pump_fee_config(), false, false),
            meta(*PUMP_FEE_PROGRAM, false, false),
            meta(bonding_curve_v2_pda(mint), false, false),
            meta(resolve_buyback_fee_recipient(), true, false),
        ],
        data,
    };
    Ok(prepared(
        vec![
            create_ata_idempotent(owner, owner, mint, token_program),
            buy,
        ],
        *owner,
        compute_unit_limit,
        priority_fee_lamports,
        tip_lamports,
    ))
}

pub fn build_pump_sell(
    owner: &Pubkey,
    mint: &Pubkey,
    token_program: &Pubkey,
    token_amount: u64,
    slippage_bps: u64,
    curve: &PumpCurveSnapshot,
    compute_unit_limit: u32,
    priority_fee_lamports: u64,
    tip_lamports: u64,
) -> Result<PreparedTrade> {
    let sol_out = quote_sell_sol(token_amount as u128, curve);
    let min_sol = slippage_down(sol_out, slippage_bps) as u64;
    let creator: Pubkey = curve.creator.parse()?;
    let fee_recipient = resolve_fee_recipient(curve);
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
        meta(system_program::id(), false, false),
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
    accounts.push(meta(resolve_buyback_fee_recipient(), true, false));
    Ok(prepared(
        vec![Instruction {
            program_id: *PUMP_PROGRAM,
            accounts,
            data,
        }],
        *owner,
        compute_unit_limit,
        priority_fee_lamports,
        tip_lamports,
    ))
}

pub fn build_amm_buy(
    owner: &Pubkey,
    lamports: u64,
    slippage_bps: u64,
    swap: &PumpSwapSnapshot,
    compute_unit_limit: u32,
    priority_fee_lamports: u64,
    tip_lamports: u64,
) -> Result<PreparedTrade> {
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
    let mut accounts = amm_accounts(
        owner,
        &pool,
        &base_mint,
        &quote_mint,
        &user_base,
        &user_quote,
        &swap.pool_base_token_account.parse()?,
        &swap.pool_quote_token_account.parse()?,
        &protocol_recipient,
        &base_program,
        &quote_program,
        &vault_authority,
        true,
    );
    if swap.cashback {
        accounts.push(meta(
            ata(&amm_user_volume(owner), &quote_mint, &quote_program),
            true,
            false,
        ));
    }
    if coin_creator != Pubkey::default() {
        accounts.push(meta(pool_v2_pda(&base_mint), false, false));
    }
    accounts.push(meta(buyback, false, false));
    accounts.push(meta(
        ata(&buyback, &quote_mint, &quote_program),
        true,
        false,
    ));
    let mut instructions = vec![create_ata_idempotent(
        owner,
        owner,
        &base_mint,
        &base_program,
    )];
    instructions.extend(wrap_wsol(
        owner,
        &quote_mint,
        &quote_program,
        &user_quote,
        max_quote as u64,
    ));
    instructions.push(Instruction {
        program_id: *PUMP_AMM_PROGRAM,
        accounts,
        data,
    });
    Ok(prepared(
        instructions,
        *owner,
        compute_unit_limit,
        priority_fee_lamports,
        tip_lamports,
    ))
}

pub fn build_amm_sell(
    owner: &Pubkey,
    token_amount: u64,
    slippage_bps: u64,
    swap: &PumpSwapSnapshot,
    compute_unit_limit: u32,
    priority_fee_lamports: u64,
    tip_lamports: u64,
) -> Result<PreparedTrade> {
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
    let mut accounts = amm_accounts(
        owner,
        &pool,
        &base_mint,
        &quote_mint,
        &user_base,
        &user_quote,
        &swap.pool_base_token_account.parse()?,
        &swap.pool_quote_token_account.parse()?,
        &protocol_recipient,
        &base_program,
        &quote_program,
        &vault_authority,
        false,
    );
    if swap.cashback {
        accounts.push(meta(
            ata(&amm_user_volume(owner), &quote_mint, &quote_program),
            true,
            false,
        ));
        accounts.push(meta(amm_user_volume(owner), true, false));
    }
    if coin_creator != Pubkey::default() {
        accounts.push(meta(pool_v2_pda(&base_mint), false, false));
    }
    accounts.push(meta(buyback, false, false));
    accounts.push(meta(
        ata(&buyback, &quote_mint, &quote_program),
        true,
        false,
    ));
    let mut instructions = vec![create_ata_idempotent(
        owner,
        owner,
        &quote_mint,
        &quote_program,
    )];
    instructions.push(Instruction {
        program_id: *PUMP_AMM_PROGRAM,
        accounts,
        data,
    });
    Ok(prepared(
        instructions,
        *owner,
        compute_unit_limit,
        priority_fee_lamports,
        tip_lamports,
    ))
}

fn amm_accounts(
    user: &Pubkey,
    pool: &Pubkey,
    base_mint: &Pubkey,
    quote_mint: &Pubkey,
    user_base: &Pubkey,
    user_quote: &Pubkey,
    pool_base: &Pubkey,
    pool_quote: &Pubkey,
    protocol_recipient: &Pubkey,
    base_program: &Pubkey,
    quote_program: &Pubkey,
    vault_authority: &Pubkey,
    include_volume: bool,
) -> Vec<AccountMeta> {
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
        meta(
            ata(protocol_recipient, quote_mint, quote_program),
            true,
            false,
        ),
        meta(*base_program, false, false),
        meta(*quote_program, false, false),
        meta(system_program::id(), false, false),
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

pub(crate) fn wrap_wsol(
    owner: &Pubkey,
    quote_mint: &Pubkey,
    quote_program: &Pubkey,
    user_quote: &Pubkey,
    lamports: u64,
) -> Vec<Instruction> {
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

fn prepared(
    instructions: Vec<Instruction>,
    payer: Pubkey,
    compute_unit_limit: u32,
    priority_fee_lamports: u64,
    tip_lamports: u64,
) -> PreparedTrade {
    PreparedTrade {
        instructions,
        payer,
        compute_unit_limit,
        priority_fee_lamports,
        tip_lamports,
        lookup_tables: Vec::new(),
    }
}

/// v0 charged this when `SetLoadedAccountsDataSizeLimit` was omitted.
/// v1 treats an unset limit as zero and the transaction fails.
const LOADED_ACCOUNTS_DATA_SIZE_LIMIT: u32 = 64 * 1024 * 1024;

pub fn compile_trade(
    prepared: &PreparedTrade,
    blockhash: Hash,
    keypair: &Keypair,
) -> Result<VersionedTransaction> {
    compile_v0(prepared, blockhash, keypair)
}

fn compile_v0(
    prepared: &PreparedTrade,
    blockhash: Hash,
    keypair: &Keypair,
) -> Result<VersionedTransaction> {
    let micro = (prepared.priority_fee_lamports as u128 * 1_000_000)
        .div_ceil(prepared.compute_unit_limit.max(1) as u128) as u64;
    let mut instructions = vec![
        ComputeBudgetInstruction::set_compute_unit_limit(prepared.compute_unit_limit),
        ComputeBudgetInstruction::set_compute_unit_price(micro),
        system_instruction::transfer(
            &prepared.payer,
            &pick(&HELIUS_TIP_ACCOUNTS),
            prepared.tip_lamports,
        ),
    ];
    instructions.extend(prepared.instructions.iter().cloned());
    let mut lookup_tables = prepared.lookup_tables.clone();
    if let Some(common) = super::common_alt::cached() {
        lookup_tables.push(common);
    }
    let message =
        v0::Message::try_compile(&prepared.payer, &instructions, &lookup_tables, blockhash)?;
    Ok(VersionedTransaction::try_new(
        VersionedMessage::V0(message),
        &[keypair],
    )?)
}

#[allow(dead_code)]
fn compile_v1(
    prepared: &PreparedTrade,
    blockhash: Hash,
    keypair: &Keypair,
) -> Result<VersionedTransaction> {
    let mut instructions = vec![system_instruction::transfer(
        &prepared.payer,
        &pick(&HELIUS_TIP_ACCOUNTS),
        prepared.tip_lamports,
    )];
    instructions.extend(prepared.instructions.iter().cloned());
    // v1 stores the budget in the message config. Priority fee is a total in
    // lamports, not micro-lamports per compute unit.
    let config = v1::TransactionConfig::empty()
        .with_compute_unit_limit(prepared.compute_unit_limit)
        .with_loaded_accounts_data_size_limit(LOADED_ACCOUNTS_DATA_SIZE_LIMIT)
        .with_priority_fee(prepared.priority_fee_lamports);
    let message =
        v1::Message::try_compile_with_config(&prepared.payer, &instructions, blockhash, config)?;
    Ok(VersionedTransaction::try_new(
        VersionedMessage::V1(message),
        &[keypair],
    )?)
}

/// Wire bytes for `sendTransaction` with `encoding: base64`.
/// v1 is not bincode-compatible; the envelope starts with `0x81` and signatures sit at the tail.
pub fn transaction_wire_bytes(transaction: &VersionedTransaction) -> Result<Vec<u8>> {
    wincode::serialize(transaction).map_err(|err| anyhow::anyhow!("serialize transaction: {err}"))
}

pub fn burn_instruction(
    account: &Pubkey,
    mint: &Pubkey,
    owner: &Pubkey,
    amount: u64,
    program: &Pubkey,
) -> Instruction {
    let mut data = vec![8];
    data.extend_from_slice(&amount.to_le_bytes());
    Instruction {
        program_id: *program,
        accounts: vec![
            meta(*account, true, false),
            meta(*mint, true, false),
            meta(*owner, false, true),
        ],
        data,
    }
}

pub fn close_account_instruction(
    account: &Pubkey,
    destination: &Pubkey,
    owner: &Pubkey,
    program: &Pubkey,
) -> Instruction {
    Instruction {
        program_id: *program,
        accounts: vec![
            meta(*account, true, false),
            meta(*destination, true, false),
            meta(*owner, false, true),
        ],
        data: vec![9],
    }
}

pub fn harvest_withheld_instruction(
    mint: &Pubkey,
    account: &Pubkey,
    program: &Pubkey,
) -> Instruction {
    Instruction {
        program_id: *program,
        accounts: vec![meta(*mint, true, false), meta(*account, true, false)],
        data: vec![26, 4],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::venues::ids::PUMP_FEE_RECIPIENTS;
    use solana_sdk::signer::Signer;

    fn sample_curve(fee_recipient: Option<&str>) -> PumpCurveSnapshot {
        PumpCurveSnapshot {
            virtual_quote_reserves: 30_000_000_000,
            virtual_token_reserves: 1_073_000_000_000_000,
            real_token_reserves: 800_000_000_000_000,
            creator: Pubkey::new_unique().to_string(),
            mayhem_mode: false,
            quote_mint: Some(system_program::id().to_string()),
            protocol_fee_bps: 100,
            creator_fee_bps: 30,
            fee_recipient: fee_recipient.map(str::to_string),
            cashback: false,
        }
    }

    #[test]
    fn rejects_system_program_fee_recipient_on_sell() {
        let owner = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        // This is the on-chain failure mode: quote mint / System Program leaked into fee_recipient.
        let curve = sample_curve(Some(&system_program::id().to_string()));
        let prepared = build_pump_sell(
            &owner,
            &mint,
            &crate::venues::ids::TOKEN_2022_PROGRAM,
            1_000_000,
            500,
            &curve,
            150_000,
            100_000,
            5_000,
        )
        .unwrap();
        let sell = prepared
            .instructions
            .iter()
            .find(|ix| ix.program_id == *PUMP_PROGRAM)
            .unwrap();
        let fee_recipient = sell.accounts[1].pubkey;
        assert_ne!(fee_recipient, system_program::id());
        assert!(
            PUMP_FEE_RECIPIENTS
                .iter()
                .any(|allowed| *allowed == fee_recipient)
        );
    }

    #[test]
    fn compiles_a_v1_transaction() {
        let payer = Keypair::new();
        let prepared = PreparedTrade {
            instructions: vec![system_instruction::transfer(
                &payer.pubkey(),
                &Pubkey::new_unique(),
                1,
            )],
            payer: payer.pubkey(),
            compute_unit_limit: 150_000,
            priority_fee_lamports: 10_000,
            tip_lamports: 5_000,
            lookup_tables: Vec::new(),
        };
        let tx = compile_v1(&prepared, Hash::default(), &payer).unwrap();
        let VersionedMessage::V1(message) = &tx.message else {
            panic!("expected a v1 message");
        };
        assert_eq!(message.config.compute_unit_limit, Some(150_000));
        assert_eq!(message.config.priority_fee, Some(10_000));
        assert_eq!(
            message.config.loaded_accounts_data_size_limit,
            Some(LOADED_ACCOUNTS_DATA_SIZE_LIMIT)
        );
        assert!(message.instructions.iter().all(|ix| {
            message.account_keys[ix.program_id_index as usize] == system_program::id()
        }));
        let raw = transaction_wire_bytes(&tx).unwrap();
        assert_eq!(raw[0], v1::V1_PREFIX);
    }

    #[test]
    fn every_pair_compiles_v0() {
        let owner = Keypair::new();
        let mint = Pubkey::new_unique();
        let sol = sample_curve(None);
        let prepared = build_pump_sell(
            &owner.pubkey(),
            &mint,
            &crate::venues::ids::TOKEN_PROGRAM,
            1_000_000,
            500,
            &sol,
            150_000,
            10_000,
            5_000,
        )
        .unwrap();
        let tx = compile_trade(&prepared, Hash::default(), &owner).unwrap();
        assert!(matches!(tx.message, VersionedMessage::V0(_)));
        assert_ne!(transaction_wire_bytes(&tx).unwrap()[0], v1::V1_PREFIX);

        let mut other = sample_curve(None);
        other.quote_mint = Some(Pubkey::new_unique().to_string());
        let prepared = build_pump_sell(
            &owner.pubkey(),
            &mint,
            &crate::venues::ids::TOKEN_PROGRAM,
            1_000_000,
            500,
            &other,
            150_000,
            10_000,
            5_000,
        )
        .unwrap();
        let tx = compile_trade(&prepared, Hash::default(), &owner).unwrap();
        assert!(matches!(tx.message, VersionedMessage::V0(_)));
    }

    #[test]
    fn keeps_allowlisted_fee_recipient() {
        let allowed = PUMP_FEE_RECIPIENTS[2];
        let curve = sample_curve(Some(&allowed.to_string()));
        assert_eq!(resolve_fee_recipient(&curve), allowed);
    }
}
