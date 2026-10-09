//! Transaction + bonding-curve account decode for the geyser stream.

use crate::events::{CompiledIx, ParsedTargetTransaction, PumpCurveSnapshot, TokenBalanceInfo};
use solana_sdk::pubkey::Pubkey;
use yellowstone_grpc_proto::prelude::{
    CompiledInstruction, InnerInstruction, SubscribeUpdateAccount, SubscribeUpdateTransactionInfo,
};

pub fn parse_transaction(slot: u64, info: Option<&SubscribeUpdateTransactionInfo>) -> Option<ParsedTargetTransaction> {
    let info = info?;
    if info.meta.as_ref().and_then(|meta| meta.err.as_ref()).is_some() {
        return None;
    }
    let message = info.transaction.as_ref()?.message.as_ref()?;
    let mut account_keys: Vec<String> = message.account_keys.iter().map(|key| bs58::encode(key).into_string()).collect();
    if let Some(meta) = info.meta.as_ref() {
        account_keys.extend(meta.loaded_writable_addresses.iter().map(|key| bs58::encode(key).into_string()));
        account_keys.extend(meta.loaded_readonly_addresses.iter().map(|key| bs58::encode(key).into_string()));
    }
    let instructions: Vec<CompiledIx> = message.instructions.iter().map(compiled_ix).collect();
    let inner_instructions = info.meta.as_ref().map(|meta| {
        meta.inner_instructions.iter().flat_map(|group| group.instructions.iter().map(inner_ix)).collect()
    }).unwrap_or_default();
    let program_ids = instructions.iter().filter_map(|ix| account_keys.get(ix.program_id_index).cloned()).collect();
    let post_token_balances = info.meta.as_ref().map(|meta| {
        meta.post_token_balances.iter().map(|balance| TokenBalanceInfo {
            account_index: balance.account_index as usize,
            mint: balance.mint.clone(),
            owner: balance.owner.clone(),
            program_id: balance.program_id.clone(),
            amount: balance.ui_token_amount.as_ref().and_then(|ui| ui.amount.parse().ok()).unwrap_or(0),
        }).collect()
    }).unwrap_or_default();
    let logs = info.meta.as_ref().map(|meta| meta.log_messages.clone()).unwrap_or_default();
    Some(ParsedTargetTransaction {
        signature: bs58::encode(&info.signature).into_string(),
        slot,
        timestamp_ms: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as u64,
        account_keys,
        program_ids,
        logs,
        instructions,
        inner_instructions,
        post_token_balances,
        failed: false,
    })
}

pub fn account_pubkey(acc: &SubscribeUpdateAccount) -> Option<Pubkey> {
    let info = acc.account.as_ref()?;
    Pubkey::try_from(info.pubkey.as_slice()).ok()
}

pub fn account_data(acc: &SubscribeUpdateAccount) -> Option<&[u8]> {
    Some(acc.account.as_ref()?.data.as_slice())
}

/// Disc (8) + 5×u64 + complete + creator = 81 core bytes (New Folder layout).
pub fn bonding_curve_from_account_data(data: &[u8]) -> Option<PumpCurveSnapshot> {
    const CORE: usize = 8 + 5 * 8 + 1 + 32;
    if data.len() < CORE {
        return None;
    }
    let body = &data[8..];
    let virtual_token = u64::from_le_bytes(body[0..8].try_into().ok()?);
    let virtual_quote = u64::from_le_bytes(body[8..16].try_into().ok()?);
    let real_token = u64::from_le_bytes(body[16..24].try_into().ok()?);
    let complete = body[40] != 0;
    if complete {
        return None;
    }
    let creator = Pubkey::new_from_array(body[41..73].try_into().ok()?);
    let mayhem = body.len() > 73 && body[73] != 0;
    let cashback = body.len() > 74 && body[74] != 0;
    let quote_mint = if body.len() >= 75 + 32 {
        Some(Pubkey::new_from_array(body[75..107].try_into().ok()?).to_string())
    } else {
        Some(crate::venues::ids::WSOL_MINT.to_string())
    };
    Some(PumpCurveSnapshot {
        virtual_quote_reserves: virtual_quote as u128,
        virtual_token_reserves: virtual_token as u128,
        real_token_reserves: real_token as u128,
        creator: creator.to_string(),
        mayhem_mode: mayhem,
        quote_mint,
        protocol_fee_bps: 100,
        creator_fee_bps: if creator == Pubkey::default() { 0 } else { 30 },
        fee_recipient: None,
        cashback,
    })
}

pub fn token_account_amount(data: &[u8]) -> Option<u64> {
    if data.len() < 72 {
        return None;
    }
    Some(u64::from_le_bytes(data[64..72].try_into().ok()?))
}

fn compiled_ix(ix: &CompiledInstruction) -> CompiledIx {
    CompiledIx {
        program_id_index: ix.program_id_index as usize,
        accounts: ix.accounts.iter().map(|a| *a as usize).collect(),
        data: ix.data.clone(),
    }
}

fn inner_ix(ix: &InnerInstruction) -> CompiledIx {
    CompiledIx {
        program_id_index: ix.program_id_index as usize,
        accounts: ix.accounts.iter().map(|a| *a as usize).collect(),
        data: ix.data.clone(),
    }
}
