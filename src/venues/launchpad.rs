//! Raydium LaunchLab (Stonk.fun) pool decode and `buy_exact_in` / `sell_exact_in`.
//! Account order matches project `1` / raydium-sdk-V2.

use anyhow::{Result, bail};
use solana_sdk::instruction::{AccountMeta, Instruction};
use solana_sdk::pubkey::Pubkey;
use solana_system_interface::instruction as system_instruction;
use solana_system_interface::program as system_program;

use super::ids::ata;
use super::quote::slippage_down;
use super::trade::create_ata_idempotent;

pub const PROGRAM_ID_STR: &str = "LanMV9sAd7wArD4vJFi2qDdfnVhFxYSUg6eADduJ3uj";

pub static PROGRAM_ID: std::sync::LazyLock<Pubkey> =
    std::sync::LazyLock::new(|| PROGRAM_ID_STR.parse().expect("launchlab program"));
pub static PLATFORM_REWARD: std::sync::LazyLock<Pubkey> = std::sync::LazyLock::new(|| {
    "6BwHHDg3u1854jC8PDLXvR4spTcLNaoBxLJNGC4nTESt"
        .parse()
        .expect("stonk platform")
});
pub static PLATFORM_STANDARD: std::sync::LazyLock<Pubkey> = std::sync::LazyLock::new(|| {
    "4E876qZTE9FJMrBzgVtBrSrzz2TLivB5Y5QXPjB4gZL7"
        .parse()
        .expect("stonk platform")
});

const BUY_EXACT_IN: [u8; 8] = [250, 234, 13, 123, 213, 156, 19, 236];
const SELL_EXACT_IN: [u8; 8] = [149, 39, 222, 155, 211, 124, 152, 26];
const FEE_RATE_DENOMINATOR: u64 = 1_000_000;
pub const DEFAULT_TRADE_FEE_RATE: u64 = 10_000;

pub const IX_POOL_ACCOUNT_INDEX: usize = 4;
pub const IX_VAULT_B_ACCOUNT_INDEX: usize = 8;
pub const IX_MINT_A_ACCOUNT_INDEX: usize = 9;
pub const IX_MINT_B_ACCOUNT_INDEX: usize = 10;
const POOL_MINT_B_OFFSET: usize = 237;

/// Quote atoms of one LaunchLab print.
///
/// A buy uses `amount_in`. Wrapping SOL in the same transaction leaves the
/// trader's quote balance unchanged, so the wallet delta is not the trade.
/// A sell uses the trader's quote gain, or the quote vault's drop when that
/// gain was unwrapped in the same transaction.
pub fn quote_print_atoms(is_buy: bool, amount_in: u64, user_delta: u64, vault_out: u64) -> u64 {
    if is_buy {
        if amount_in > 0 { amount_in } else { user_delta }
    } else if user_delta > 0 {
        user_delta
    } else {
        vault_out
    }
}

pub fn is_stonk_platform(platform_id: &Pubkey) -> bool {
    *platform_id == *PLATFORM_REWARD || *platform_id == *PLATFORM_STANDARD
}

pub fn auth_pda() -> Pubkey {
    Pubkey::find_program_address(&[b"vault_auth_seed"], &PROGRAM_ID).0
}

pub fn event_authority_pda() -> Pubkey {
    Pubkey::find_program_address(&[b"__event_authority"], &PROGRAM_ID).0
}

pub fn platform_fee_vault_pda(platform_id: &Pubkey, mint_b: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[platform_id.as_ref(), mint_b.as_ref()], &PROGRAM_ID).0
}

pub fn creator_fee_vault_pda(creator: &Pubkey, mint_b: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[creator.as_ref(), mint_b.as_ref()], &PROGRAM_ID).0
}

#[derive(Debug, Clone, Copy)]
pub struct LaunchpadPoolState {
    pub pool: Pubkey,
    pub status: u8,
    pub virtual_a: u64,
    pub virtual_b: u64,
    pub real_a: u64,
    pub real_b: u64,
    pub total_sell_a: u64,
    pub config_id: Pubkey,
    pub platform_id: Pubkey,
    pub mint_a: Pubkey,
    pub mint_b: Pubkey,
    pub vault_a: Pubkey,
    pub vault_b: Pubkey,
    pub creator: Pubkey,
}

impl LaunchpadPoolState {
    pub fn from_account_data(pool: &Pubkey, data: &[u8]) -> Result<Self> {
        if data.len() < POOL_MINT_B_OFFSET + 32 + 32 + 32 + 32 {
            bail!("launchpad pool data too short ({})", data.len());
        }
        let mut o = 8usize;
        o += 8;
        o += 1;
        let status = data[o];
        o += 1 + 1 + 1 + 1;
        o += 8;
        let total_sell_a = u64::from_le_bytes(data[o..o + 8].try_into()?);
        o += 8;
        let virtual_a = u64::from_le_bytes(data[o..o + 8].try_into()?);
        o += 8;
        let virtual_b = u64::from_le_bytes(data[o..o + 8].try_into()?);
        o += 8;
        let real_a = u64::from_le_bytes(data[o..o + 8].try_into()?);
        o += 8;
        let real_b = u64::from_le_bytes(data[o..o + 8].try_into()?);
        o += 8;
        o += 8 * 4;
        o += 8 * 5;
        let config_id = Pubkey::new_from_array(data[o..o + 32].try_into()?);
        o += 32;
        let platform_id = Pubkey::new_from_array(data[o..o + 32].try_into()?);
        o += 32;
        let mint_a = Pubkey::new_from_array(data[o..o + 32].try_into()?);
        o += 32;
        let mint_b = Pubkey::new_from_array(data[o..o + 32].try_into()?);
        o += 32;
        let vault_a = Pubkey::new_from_array(data[o..o + 32].try_into()?);
        o += 32;
        let vault_b = Pubkey::new_from_array(data[o..o + 32].try_into()?);
        o += 32;
        let creator = Pubkey::new_from_array(data[o..o + 32].try_into()?);
        Ok(Self {
            pool: *pool,
            status,
            virtual_a,
            virtual_b,
            real_a,
            real_b,
            total_sell_a,
            config_id,
            platform_id,
            mint_a,
            mint_b,
            vault_a,
            vault_b,
            creator,
        })
    }

    pub fn completed(&self) -> bool {
        self.status != 0 || self.real_a >= self.total_sell_a
    }

    fn quote_reserve(&self) -> u128 {
        self.virtual_b as u128 + self.real_b as u128
    }

    fn base_reserve(&self) -> u128 {
        self.virtual_a.saturating_sub(self.real_a) as u128
    }
}

fn cp_out(amount_in: u64, input_reserve: u128, output_reserve: u128) -> u64 {
    if amount_in == 0 || input_reserve == 0 || output_reserve == 0 {
        return 0;
    }
    let out = (amount_in as u128).saturating_mul(output_reserve)
        / input_reserve.saturating_add(amount_in as u128);
    out.min(u64::MAX as u128) as u64
}

pub fn quote_buy_tokens(pool: &LaunchpadPoolState, amount_b: u64) -> u64 {
    let fee = amount_b.saturating_mul(DEFAULT_TRADE_FEE_RATE) / FEE_RATE_DENOMINATOR;
    let net = amount_b.saturating_sub(fee);
    let out = cp_out(net, pool.quote_reserve(), pool.base_reserve());
    out.min(pool.total_sell_a.saturating_sub(pool.real_a))
}

pub fn quote_sell_quote(pool: &LaunchpadPoolState, amount_a: u64) -> u64 {
    let gross = cp_out(amount_a, pool.base_reserve(), pool.quote_reserve());
    let fee = gross.saturating_mul(DEFAULT_TRADE_FEE_RATE) / FEE_RATE_DENOMINATOR;
    gross.saturating_sub(fee)
}

fn meta(pubkey: Pubkey, writable: bool, signer: bool) -> AccountMeta {
    AccountMeta {
        pubkey,
        is_signer: signer,
        is_writable: writable,
    }
}

fn launchpad_accounts(
    owner: &Pubkey,
    pool: &LaunchpadPoolState,
    token_program: &Pubkey,
    quote_program: &Pubkey,
) -> Vec<AccountMeta> {
    let user_base = ata(owner, &pool.mint_a, token_program);
    let user_quote = ata(owner, &pool.mint_b, quote_program);
    vec![
        meta(*owner, true, true),
        meta(auth_pda(), false, false),
        meta(pool.config_id, false, false),
        meta(pool.platform_id, false, false),
        meta_pool(pool),
        meta(user_base, true, false),
        meta(user_quote, true, false),
        meta(pool.vault_a, true, false),
        meta(pool.vault_b, true, false),
        meta(pool.mint_a, false, false),
        meta(pool.mint_b, false, false),
        meta(*token_program, false, false),
        meta(*quote_program, false, false),
        meta(event_authority_pda(), false, false),
        meta(*PROGRAM_ID, false, false),
        meta(system_program::id(), false, false),
        meta(
            platform_fee_vault_pda(&pool.platform_id, &pool.mint_b),
            true,
            false,
        ),
        meta(
            creator_fee_vault_pda(&pool.creator, &pool.mint_b),
            true,
            false,
        ),
    ]
}

fn meta_pool(pool: &LaunchpadPoolState) -> AccountMeta {
    meta(pool.pool, true, false)
}

pub fn buy_instructions(
    owner: &Pubkey,
    pool: &LaunchpadPoolState,
    token_program: &Pubkey,
    quote_program: &Pubkey,
    spend_atoms: u64,
    slippage_bps: u64,
) -> Result<Vec<Instruction>> {
    let quoted = quote_buy_tokens(pool, spend_atoms);
    if quoted == 0 {
        bail!("launchlab buy quotes zero tokens for {}", pool.mint_a);
    }
    let min_tokens = slippage_down(quoted as u128, slippage_bps) as u64;
    let mut data = Vec::with_capacity(32);
    data.extend_from_slice(&BUY_EXACT_IN);
    data.extend_from_slice(&spend_atoms.to_le_bytes());
    data.extend_from_slice(&min_tokens.to_le_bytes());
    data.extend_from_slice(&0u64.to_le_bytes());
    Ok(vec![
        create_ata_idempotent(owner, owner, &pool.mint_a, token_program),
        create_ata_idempotent(owner, owner, &pool.mint_b, quote_program),
        Instruction {
            program_id: *PROGRAM_ID,
            accounts: launchpad_accounts(owner, pool, token_program, quote_program),
            data,
        },
    ])
}

/// SOL-quoted LaunchLab spends wrapped SOL. Wrap the budget, then `buy_exact_in`.
pub fn sol_buy_instructions(
    owner: &Pubkey,
    pool: &LaunchpadPoolState,
    token_program: &Pubkey,
    quote_program: &Pubkey,
    spend_lamports: u64,
    slippage_bps: u64,
) -> Result<Vec<Instruction>> {
    let mut ixs = buy_instructions(
        owner,
        pool,
        token_program,
        quote_program,
        spend_lamports,
        slippage_bps,
    )?;
    let Some(buy) = ixs.pop() else {
        bail!("launchlab buy missing");
    };
    let user_quote = ata(owner, &pool.mint_b, quote_program);
    ixs.push(system_instruction::transfer(
        owner,
        &user_quote,
        spend_lamports,
    ));
    ixs.push(Instruction {
        program_id: *quote_program,
        accounts: vec![meta(user_quote, true, false)],
        data: vec![17],
    });
    ixs.push(buy);
    Ok(ixs)
}

pub fn sell_instructions(
    owner: &Pubkey,
    pool: &LaunchpadPoolState,
    token_program: &Pubkey,
    quote_program: &Pubkey,
    token_amount: u64,
    slippage_bps: u64,
) -> Result<Vec<Instruction>> {
    let quoted = quote_sell_quote(pool, token_amount);
    let min_quote = slippage_down(quoted as u128, slippage_bps) as u64;
    let mut data = Vec::with_capacity(32);
    data.extend_from_slice(&SELL_EXACT_IN);
    data.extend_from_slice(&token_amount.to_le_bytes());
    data.extend_from_slice(&min_quote.to_le_bytes());
    data.extend_from_slice(&0u64.to_le_bytes());
    Ok(vec![
        create_ata_idempotent(owner, owner, &pool.mint_b, quote_program),
        Instruction {
            program_id: *PROGRAM_ID,
            accounts: launchpad_accounts(owner, pool, token_program, quote_program),
            data,
        },
    ])
}

pub fn trade_from_ix(
    account_keys: &[String],
    ix: &crate::events::CompiledIx,
) -> Option<LaunchpadTrade> {
    let program = account_keys.get(ix.program_id_index)?;
    if program != PROGRAM_ID_STR
        || ix.data.len() < 24
        || ix.accounts.len() <= IX_MINT_B_ACCOUNT_INDEX
    {
        return None;
    }
    let disc: [u8; 8] = ix.data[..8].try_into().ok()?;
    let side = if disc == BUY_EXACT_IN {
        true
    } else if disc == SELL_EXACT_IN {
        false
    } else {
        return None;
    };
    let key = |index: usize| {
        let pos = *ix.accounts.get(index)? as usize;
        account_keys.get(pos).cloned()
    };
    Some(LaunchpadTrade {
        is_buy: side,
        trader: key(0)?,
        pool: key(IX_POOL_ACCOUNT_INDEX)?,
        vault_b: key(IX_VAULT_B_ACCOUNT_INDEX)?,
        mint_a: key(IX_MINT_A_ACCOUNT_INDEX)?,
        mint_b: key(IX_MINT_B_ACCOUNT_INDEX)?,
        amount_in: u64::from_le_bytes(ix.data[8..16].try_into().ok()?),
        min_out: u64::from_le_bytes(ix.data[16..24].try_into().ok()?),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mint_offsets_match_the_sdk_layout() {
        let mut data = vec![0u8; 429];
        let mint_a = Pubkey::new_unique();
        let mint_b = Pubkey::new_unique();
        data[205..237].copy_from_slice(mint_a.as_ref());
        data[237..269].copy_from_slice(mint_b.as_ref());
        let pool = Pubkey::new_unique();
        let parsed = LaunchpadPoolState::from_account_data(&pool, &data).unwrap();
        assert_eq!(parsed.mint_a, mint_a);
        assert_eq!(parsed.mint_b, mint_b);
        assert_eq!(parsed.pool, pool);
    }

    #[test]
    fn wrapped_sol_buy_uses_the_instruction_amount() {
        assert_eq!(quote_print_atoms(true, 1_000_000_000, 0, 0), 1_000_000_000);
        assert_eq!(quote_print_atoms(false, 0, 0, 400_000_000), 400_000_000);
        assert_eq!(
            quote_print_atoms(false, 0, 250_000_000, 400_000_000),
            250_000_000
        );
    }
}

pub struct LaunchpadTrade {
    pub is_buy: bool,
    pub trader: String,
    pub pool: String,
    pub vault_b: String,
    pub mint_a: String,
    pub mint_b: String,
    pub amount_in: u64,
    pub min_out: u64,
}
