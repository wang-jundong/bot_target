//! One address lookup table for the program accounts that repeat on every
//! non-SOL buy and sell. Jupiter's tables do not contain them, so each one
//! otherwise costs 32 bytes and the packet passes 1232.

use super::ids::{
    ATA_PROGRAM, HELIUS_TIP_ACCOUNTS, PUMP_AMM_PROGRAM, PUMP_BUYBACK_FEE_RECIPIENTS,
    PUMP_FEE_PROGRAM, PUMP_PROGRAM, TOKEN_2022_PROGRAM, TOKEN_PROGRAM, amm_event_authority,
    amm_fee_config, amm_global_config, pubkey, pump_event_authority, pump_fee_config, pump_global,
};
use anyhow::{Context, Result, bail};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::instruction::{AccountMeta, Instruction};
use solana_sdk::message::{AddressLookupTableAccount, VersionedMessage, v0};
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signature};
use solana_sdk::signer::Signer;
use solana_sdk::transaction::VersionedTransaction;
use solana_system_interface::program as system_program;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

const ALT_PROGRAM: &str = "AddressLookupTab1e1111111111111111111111111";
const META_SIZE: usize = 56;

static TABLE: OnceLock<AddressLookupTableAccount> = OnceLock::new();

pub fn cached() -> Option<AddressLookupTableAccount> {
    TABLE.get().cloned()
}

/// Program and fee accounts that show up as full keys in a non-SOL packet.
pub fn addresses() -> Vec<Pubkey> {
    let mut out = vec![
        system_program::id(),
        *TOKEN_PROGRAM,
        *TOKEN_2022_PROGRAM,
        *ATA_PROGRAM,
        *PUMP_PROGRAM,
        *PUMP_AMM_PROGRAM,
        *PUMP_FEE_PROGRAM,
        pubkey("ComputeBudget111111111111111111111111111111"),
        pubkey("JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4"),
        pubkey("D8cy77BBepLMngZx6ZukaTff5hCt1HrWyKk3Hnd9oitf"),
        pump_global(),
        pump_fee_config(),
        pump_event_authority(),
        amm_global_config(),
        amm_fee_config(),
        amm_event_authority(),
    ];
    out.extend(HELIUS_TIP_ACCOUNTS.iter().copied());
    out.extend(PUMP_BUYBACK_FEE_RECIPIENTS.iter().copied());
    let mut seen = HashSet::new();
    out.retain(|key| seen.insert(*key));
    out
}

/// Load the table next to the recovery journal, creating it once if needed.
/// Addresses extended in the landing slot are not usable until the next slot.
pub async fn ensure(rpc: &RpcClient, authority: &Keypair, recovery_path: &Path) -> Result<()> {
    let path = alt_path(recovery_path);
    let wanted = addresses();
    if let Some(key) = read_key(&path).await
        && let Some(loaded) = fetch(rpc, &key).await?
    {
        let have: HashSet<Pubkey> = loaded.addresses.iter().copied().collect();
        let missing: Vec<Pubkey> = wanted
            .iter()
            .copied()
            .filter(|address| !have.contains(address))
            .collect();
        if missing.is_empty() {
            remember(key, loaded.addresses)?;
            tracing::info!(table = %key, "[01 STARTUP] Common lookup table ready");
            return Ok(());
        }
        let signature = send(
            rpc,
            authority,
            vec![extend_instruction(&key, &authority.pubkey(), missing)],
        )
        .await?;
        confirm(rpc, &signature).await?;
        let loaded = fetch(rpc, &key)
            .await?
            .context("lookup table missing after extend")?;
        wait_until_usable(rpc, last_extended_slot(&loaded.raw)).await?;
        let loaded = fetch(rpc, &key)
            .await?
            .context("lookup table missing after extend")?;
        remember(key, loaded.addresses)?;
        tracing::info!(table = %key, "[01 STARTUP] Common lookup table extended");
        return Ok(());
    }
    let slot = rpc.get_slot().await?;
    let (create, key) = create_instruction(&authority.pubkey(), slot);
    let extend = extend_instruction(&key, &authority.pubkey(), wanted);
    let signature = send(rpc, authority, vec![create, extend]).await?;
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(&path, key.to_string()).await?;
    confirm(rpc, &signature).await?;
    let loaded = fetch(rpc, &key)
        .await?
        .context("lookup table missing after create")?;
    wait_until_usable(rpc, last_extended_slot(&loaded.raw)).await?;
    let loaded = fetch(rpc, &key)
        .await?
        .context("lookup table missing after create")?;
    remember(key, loaded.addresses)?;
    tracing::info!(table = %key, "[01 STARTUP] Common lookup table ready");
    Ok(())
}

fn alt_path(recovery_path: &Path) -> PathBuf {
    recovery_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("common-alt")
}

async fn read_key(path: &Path) -> Option<Pubkey> {
    let text = tokio::fs::read_to_string(path).await.ok()?;
    text.trim().parse().ok()
}

fn remember(key: Pubkey, addresses: Vec<Pubkey>) -> Result<()> {
    TABLE
        .set(AddressLookupTableAccount { key, addresses })
        .map_err(|_| anyhow::anyhow!("common lookup table already installed"))
}

struct Loaded {
    addresses: Vec<Pubkey>,
    raw: Vec<u8>,
}

fn last_extended_slot(data: &[u8]) -> u64 {
    u64::from_le_bytes(data[12..20].try_into().unwrap_or([0; 8]))
}

async fn fetch(rpc: &RpcClient, key: &Pubkey) -> Result<Option<Loaded>> {
    let response = rpc
        .get_account_with_commitment(key, rpc.commitment())
        .await?;
    let Some(account) = response.value else {
        return Ok(None);
    };
    let program = pubkey(ALT_PROGRAM);
    if account.owner != program {
        bail!("lookup table {key} is owned by {}", account.owner);
    }
    if account.data.len() < META_SIZE {
        bail!("lookup table {key} is too short");
    }
    let addresses = account.data[META_SIZE..]
        .chunks_exact(32)
        .map(|chunk| Pubkey::try_from(chunk).map_err(|err| anyhow::anyhow!(err)))
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .filter(|address| *address != Pubkey::default())
        .collect();
    Ok(Some(Loaded {
        addresses,
        raw: account.data,
    }))
}

async fn wait_until_usable(rpc: &RpcClient, extended_slot: u64) -> Result<()> {
    for _ in 0..40 {
        let slot = rpc.get_slot().await?;
        if slot > extended_slot {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
    }
    bail!("lookup table extended in slot {extended_slot} is not usable yet")
}

async fn send(
    rpc: &RpcClient,
    authority: &Keypair,
    instructions: Vec<Instruction>,
) -> Result<Signature> {
    let blockhash = rpc.get_latest_blockhash().await?;
    let message = v0::Message::try_compile(&authority.pubkey(), &instructions, &[], blockhash)?;
    let tx = VersionedTransaction::try_new(VersionedMessage::V0(message), &[authority])?;
    Ok(rpc.send_transaction(&tx).await?)
}

async fn confirm(rpc: &RpcClient, signature: &Signature) -> Result<()> {
    for _ in 0..30 {
        match rpc.get_signature_status(signature).await? {
            Some(Ok(())) => return Ok(()),
            Some(Err(err)) => bail!("lookup table transaction {signature} failed: {err}"),
            None => tokio::time::sleep(Duration::from_millis(400)).await,
        }
    }
    bail!("lookup table transaction {signature} was not confirmed")
}

fn create_instruction(authority: &Pubkey, recent_slot: u64) -> (Instruction, Pubkey) {
    let program = pubkey(ALT_PROGRAM);
    let (table, bump) =
        Pubkey::find_program_address(&[authority.as_ref(), &recent_slot.to_le_bytes()], &program);
    let mut data = Vec::with_capacity(13);
    data.extend_from_slice(&0u32.to_le_bytes());
    data.extend_from_slice(&recent_slot.to_le_bytes());
    data.push(bump);
    let instruction = Instruction {
        program_id: program,
        accounts: vec![
            AccountMeta::new(table, false),
            AccountMeta::new_readonly(*authority, true),
            AccountMeta::new(*authority, true),
            AccountMeta::new_readonly(system_program::id(), false),
        ],
        data,
    };
    (instruction, table)
}

fn extend_instruction(
    table: &Pubkey,
    authority: &Pubkey,
    new_addresses: Vec<Pubkey>,
) -> Instruction {
    let mut data = Vec::with_capacity(12 + new_addresses.len() * 32);
    data.extend_from_slice(&2u32.to_le_bytes());
    data.extend_from_slice(&(new_addresses.len() as u64).to_le_bytes());
    for address in &new_addresses {
        data.extend_from_slice(address.as_ref());
    }
    Instruction {
        program_id: pubkey(ALT_PROGRAM),
        accounts: vec![
            AccountMeta::new(*table, false),
            AccountMeta::new_readonly(*authority, true),
            AccountMeta::new(*authority, true),
            AccountMeta::new_readonly(system_program::id(), false),
        ],
        data,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::PreparedTrade;
    use crate::venues::trade::{compile_trade, transaction_wire_bytes};
    use solana_sdk::hash::Hash;
    use solana_system_interface::instruction as system_instruction;

    #[test]
    fn create_and_extend_match_the_lookup_program() {
        use solana_address_lookup_table_interface::instruction::{
            create_lookup_table, extend_lookup_table,
        };
        let authority = Pubkey::new_unique();
        let slot = 42u64;
        let (ours, key) = create_instruction(&authority, slot);
        let (official, official_key) = create_lookup_table(authority, authority, slot);
        assert_eq!(key, official_key);
        assert_eq!(ours.data, official.data);
        let extra = vec![Pubkey::new_unique(), Pubkey::new_unique()];
        let ours = extend_instruction(&key, &authority, extra.clone());
        let official = extend_lookup_table(key, authority, Some(authority), extra);
        assert_eq!(ours.data, official.data);
    }

    #[test]
    fn addresses_are_unique() {
        let list = addresses();
        let set: HashSet<_> = list.iter().copied().collect();
        assert_eq!(list.len(), set.len());
        assert!(list.len() > 16);
    }

    #[test]
    fn the_common_table_pulls_repeated_accounts_out_of_the_packet() {
        let payer = Keypair::new();
        let mut instructions = Vec::new();
        for address in addresses() {
            instructions.push(system_instruction::transfer(&payer.pubkey(), &address, 1));
        }
        let prepared = PreparedTrade {
            instructions,
            payer: payer.pubkey(),
            compute_unit_limit: 50_000,
            priority_fee_lamports: 1,
            tip_lamports: 1,
            lookup_tables: Vec::new(),
        };
        let bare =
            transaction_wire_bytes(&compile_trade(&prepared, Hash::default(), &payer).unwrap())
                .unwrap();
        remember(Pubkey::new_unique(), addresses()).unwrap();
        let packed =
            transaction_wire_bytes(&compile_trade(&prepared, Hash::default(), &payer).unwrap())
                .unwrap();
        assert!(bare.len() > 1232, "bare {}", bare.len());
        assert!(packed.len() <= 1232, "packed {}", packed.len());
        assert!(packed.len() + 200 < bare.len());
    }
}
