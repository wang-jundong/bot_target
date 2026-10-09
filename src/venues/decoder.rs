use super::ids::{
    amm_event_authority, amm_fee_config, amm_global_config, amm_global_volume, amm_user_volume, ata, bonding_curve_pda,
    coin_creator_vault_authority, is_pump_fee_recipient, pool_v2_pda, PUMP_AMM_PROGRAM, PUMP_FEE_PROGRAM, PUMP_PROGRAM,
    TOKEN_2022_PROGRAM, TOKEN_PROGRAM,
};
use crate::events::{
    mono_ms, CompiledIx, ParsedTargetTransaction, PoolDescriptor, PoolTradeEvent, PumpCurveSnapshot, PumpSwapSnapshot, Side,
    TokenBalanceInfo,
};
use solana_sdk::pubkey::Pubkey;
use std::collections::HashSet;

const TRADE_EVENT: [u8; 8] = [189, 219, 127, 211, 78, 230, 97, 238];
const BUY_EVENT: [u8; 8] = [103, 244, 82, 31, 44, 245, 119, 119];
const SELL_EVENT: [u8; 8] = [62, 47, 55, 10, 165, 3, 220, 42];
const SWAP_DISCS: [[u8; 8]; 3] = [
    [102, 6, 61, 18, 1, 218, 235, 234],
    [198, 46, 21, 82, 180, 217, 232, 112],
    [51, 230, 133, 164, 1, 127, 131, 173],
];

struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }
    fn rest(&self) -> usize {
        self.data.len().saturating_sub(self.pos)
    }
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.rest() < n {
            return None;
        }
        let out = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Some(out)
    }
    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|b| b[0])
    }
    fn bool(&mut self) -> Option<bool> {
        self.u8().map(|v| v != 0)
    }
    fn u64(&mut self) -> Option<u64> {
        self.take(8).map(|b| u64::from_le_bytes(b.try_into().unwrap()))
    }
    fn i64(&mut self) -> Option<i64> {
        self.take(8).map(|b| i64::from_le_bytes(b.try_into().unwrap()))
    }
    fn i128(&mut self) -> Option<i128> {
        self.take(16).map(|b| i128::from_le_bytes(b.try_into().unwrap()))
    }
    fn pubkey(&mut self) -> Option<Pubkey> {
        self.take(32).map(|b| Pubkey::new_from_array(b.try_into().unwrap()))
    }
    fn string(&mut self) -> Option<String> {
        let len = self.take(4).map(|b| u32::from_le_bytes(b.try_into().unwrap()))? as usize;
        let bytes = self.take(len)?;
        Some(String::from_utf8_lossy(bytes).into_owned())
    }
}

pub struct ParsedTrade {
    pub descriptor: PoolDescriptor,
    pub event: PoolTradeEvent,
}

pub fn decode(tx: &ParsedTargetTransaction) -> Vec<ParsedTrade> {
    let mut trades = Vec::new();
    let mut event_index = 0u32;
    for log in &tx.logs {
        let Some(encoded) = log.strip_prefix("Program data: ") else { continue };
        let Ok(bytes) = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, encoded) else { continue };
        if bytes.len() < 8 {
            continue;
        }
        let (disc, body) = bytes.split_at(8);
        if disc == TRADE_EVENT {
            if let Some(trade) = pump_trade(tx, body, event_index) {
                event_index += 1;
                trades.push(trade);
            }
            continue;
        }
        if disc == BUY_EVENT {
            if let Some(trade) = amm_trade(tx, true, body, event_index) {
                event_index += 1;
                trades.push(trade);
            }
        } else if disc == SELL_EVENT {
            if let Some(trade) = amm_trade(tx, false, body, event_index) {
                event_index += 1;
                trades.push(trade);
            }
        }
    }
    trades
}

fn pump_trade(tx: &ParsedTargetTransaction, body: &[u8], event_index: u32) -> Option<ParsedTrade> {
    let mut c = Cursor::new(body);
    let mint = c.pubkey()?;
    let sol_amount = c.u64()?;
    let token_amount = c.u64()?;
    let is_buy = c.bool()?;
    let user = c.pubkey()?;
    let timestamp = c.i64()?;
    let virtual_sol = c.u64()?;
    let virtual_token = c.u64()?;
    let _real_sol = c.u64()?;
    let real_token = c.u64()?;
    let fee_recipient = c.pubkey();
    let fee_bps = c.u64();
    let _fee = c.u64();
    let creator = c.pubkey();
    let creator_fee_bps = c.u64();
    let _creator_fee = c.u64();
    let _track = c.bool();
    let _unclaimed = c.u64();
    let _claimed = c.u64();
    let _volume = c.u64();
    let _updated = c.i64();
    let _ix_name = c.string();
    let mayhem = c.bool().unwrap_or(false);
    let cashback_bps = c.u64().unwrap_or(0);
    let _cashback = c.u64();
    let _buyback_bps = c.u64();
    let _buyback = c.u64();
    if let Some(len) = c.take(4).map(|b| u32::from_le_bytes(b.try_into().unwrap())) {
        for _ in 0..len {
            let _ = c.pubkey();
            let _ = c.take(2);
        }
    }
    let quote_mint = c.pubkey();
    let _quote_amount = c.u64();
    let virtual_quote = c.u64();
    let mint_s = mint.to_string();
    let pool = bonding_curve_pda(&mint).to_string();
    let token_program = token_program_for(tx, &mint_s);
    let descriptor = PoolDescriptor {
        mint: mint_s.clone(),
        pool: pool.clone(),
        program_id: PUMP_PROGRAM.to_string(),
        venue: "pump".to_string(),
        token_program,
        quote_mint: quote_mint.map(|k| k.to_string()),
        pool_base_token_account: None,
        pool_quote_token_account: None,
        relevant_accounts: vec![pool.clone(), mint_s.clone()],
    };
    let quote_reserves = virtual_quote.filter(|v| *v > 0).unwrap_or(virtual_sol);
    let price = ratio(quote_reserves, virtual_token);
    let mut event = base_event(tx, event_index, &descriptor, &user.to_string(), is_buy, sol_amount, token_amount, price, None, timestamp);
    if let Some(creator) = creator {
        // Drop invalid fee recipients (e.g. System Program / quote mint) so trades never trust them.
        let fee_recipient = fee_recipient.and_then(|key| {
            if is_pump_fee_recipient(&key, mayhem) {
                Some(key.to_string())
            } else {
                None
            }
        });
        event.curve = Some(PumpCurveSnapshot {
            virtual_quote_reserves: quote_reserves as u128,
            virtual_token_reserves: virtual_token as u128,
            real_token_reserves: real_token as u128,
            creator: creator.to_string(),
            mayhem_mode: mayhem,
            quote_mint: quote_mint.map(|k| k.to_string()),
            protocol_fee_bps: fee_bps.unwrap_or(0) as u128,
            creator_fee_bps: creator_fee_bps.unwrap_or(0) as u128,
            fee_recipient,
            cashback: cashback_bps > 0,
        });
    }
    Some(ParsedTrade { descriptor, event })
}

fn amm_trade(tx: &ParsedTargetTransaction, is_buy: bool, body: &[u8], event_index: u32) -> Option<ParsedTrade> {
    let mut c = Cursor::new(body);
    let timestamp = c.i64()?;
    let base_amount = c.u64()?;
    let _limit = c.u64()?;
    let _user_base = c.u64()?;
    let _user_quote = c.u64()?;
    let pool_base = c.u64()?;
    let pool_quote = c.u64()?;
    let quote_amount = c.u64()?;
    let lp_bps = c.u64()?;
    let _lp_fee = c.u64()?;
    let protocol_bps = c.u64()?;
    let _protocol_fee = c.u64()?;
    let _mid = c.u64()?;
    let _user_quote_amount = c.u64()?;
    let pool = c.pubkey()?;
    let user = c.pubkey()?;
    let _user_base_ata = c.pubkey()?;
    let _user_quote_ata = c.pubkey()?;
    let protocol_recipient = c.pubkey()?;
    let _protocol_ata = c.pubkey()?;
    let coin_creator = c.pubkey()?;
    let creator_bps = c.u64().unwrap_or(0);
    let _creator_fee = c.u64();
    let cashback_bps = if is_buy {
        let _track = c.bool();
        let _unclaimed = c.u64();
        let _claimed = c.u64();
        let _volume = c.u64();
        let _updated = c.i64();
        let _min_base = c.u64();
        let _ix = c.string();
        c.u64().unwrap_or(0)
    } else {
        c.u64().unwrap_or(0)
    };
    let _cashback_amount = c.u64();
    let _buyback_bps = c.u64();
    let _buyback = c.u64();
    let virtual_quote = c.i128().unwrap_or(0);
    let pool_s = pool.to_string();
    let vaults = pool_vaults(tx, &pool_s, pool_base, pool_quote)?;
    let descriptor = PoolDescriptor {
        mint: vaults.0.mint.clone(),
        pool: pool_s.clone(),
        program_id: PUMP_AMM_PROGRAM.to_string(),
        venue: "pumpswap".to_string(),
        token_program: Some(vaults.0.program_id.clone()),
        quote_mint: Some(vaults.1.mint.clone()),
        pool_base_token_account: Some(vaults.0.address.clone()),
        pool_quote_token_account: Some(vaults.1.address.clone()),
        relevant_accounts: vec![pool_s.clone(), vaults.0.mint.clone()],
    };
    let price = ratio(pool_quote, pool_base);
    let mut event = base_event(tx, event_index, &descriptor, &user.to_string(), is_buy, quote_amount, base_amount, price, Some(1.0), timestamp);
    if let Some(buyback) = buyback_recipient(tx, &pool_s, &user.to_string(), &vaults, &coin_creator, &protocol_recipient) {
        event.swap = Some(PumpSwapSnapshot {
            pool: pool_s,
            base_mint: vaults.0.mint.clone(),
            quote_mint: vaults.1.mint.clone(),
            pool_base_token_account: vaults.0.address.clone(),
            pool_quote_token_account: vaults.1.address.clone(),
            base_token_program: vaults.0.program_id.clone(),
            quote_token_program: vaults.1.program_id.clone(),
            base_reserve: pool_base as u128,
            quote_reserve: pool_quote as u128,
            virtual_quote_reserves: virtual_quote,
            lp_fee_bps: lp_bps as u128,
            protocol_fee_bps: protocol_bps as u128,
            coin_creator_fee_bps: creator_bps as u128,
            coin_creator: coin_creator.to_string(),
            protocol_fee_recipient: protocol_recipient.to_string(),
            buyback_fee_recipient: buyback,
            cashback: cashback_bps > 0,
        });
    }
    Some(ParsedTrade { descriptor, event })
}

struct Vault {
    mint: String,
    address: String,
    program_id: String,
}

fn pool_vaults(tx: &ParsedTargetTransaction, pool: &str, base_reserve: u64, quote_reserve: u64) -> Option<(Vault, Vault)> {
    let owned: Vec<&TokenBalanceInfo> = tx.post_token_balances.iter().filter(|b| b.owner == pool).collect();
    let source: Vec<&TokenBalanceInfo> = if owned.is_empty() { tx.post_token_balances.iter().collect() } else { owned };
    let base = vault(tx, &source, base_reserve)?;
    let rest: Vec<&TokenBalanceInfo> = source.into_iter().filter(|b| tx.account_keys.get(b.account_index).map(String::as_str) != Some(base.address.as_str())).collect();
    let quote = vault(tx, &rest, quote_reserve)?;
    Some((base, quote))
}

fn vault(tx: &ParsedTargetTransaction, balances: &[&TokenBalanceInfo], amount: u64) -> Option<Vault> {
    let match_ = balances.iter().find(|b| b.amount == amount)?;
    let address = tx.account_keys.get(match_.account_index)?.clone();
    Some(Vault {
        mint: match_.mint.clone(),
        address,
        program_id: if match_.program_id.is_empty() { TOKEN_PROGRAM.to_string() } else { match_.program_id.clone() },
    })
}

fn token_program_for(tx: &ParsedTargetTransaction, mint: &str) -> Option<String> {
    tx.post_token_balances.iter().find(|b| b.mint == mint).map(|b| b.program_id.clone()).filter(|p| !p.is_empty())
}

fn base_event(tx: &ParsedTargetTransaction, event_index: u32, descriptor: &PoolDescriptor, trader: &str, is_buy: bool, sol: u64, token: u64, price: f64, curve_progress: Option<f64>, timestamp: i64) -> PoolTradeEvent {
    PoolTradeEvent {
        signature: tx.signature.clone(),
        slot: tx.slot,
        transaction_index: None,
        event_index,
        timestamp_ms: (timestamp.max(0) as u64).saturating_mul(1000),
        received_mono_ms: mono_ms(),
        mint: descriptor.mint.clone(),
        pool: descriptor.pool.clone(),
        program_id: descriptor.program_id.clone(),
        trader: trader.to_string(),
        side: if is_buy { Side::Buy } else { Side::Sell },
        sol_amount: sol,
        token_amount: token,
        price,
        curve_progress,
        curve: None,
        swap: None,
    }
}

fn ratio(quote: u64, base: u64) -> f64 {
    if base == 0 { 0.0 } else { quote as f64 / base as f64 }
}

fn buyback_recipient(tx: &ParsedTargetTransaction, pool: &str, trader: &str, vaults: &(Vault, Vault), coin_creator: &Pubkey, protocol_fee_recipient: &Pubkey) -> Option<String> {
    let quote_program: Pubkey = vaults.1.program_id.parse().ok()?;
    let base_program: Pubkey = vaults.0.program_id.parse().ok()?;
    let trader_key: Pubkey = trader.parse().ok()?;
    let base_mint: Pubkey = vaults.0.mint.parse().ok()?;
    let quote_mint: Pubkey = vaults.1.mint.parse().ok()?;
    let creator_vault = coin_creator_vault_authority(coin_creator);
    let user_volume = amm_user_volume(&trader_key);
    let mut known = HashSet::new();
    for key in [
        pool.to_string(),
        trader.to_string(),
        amm_global_config().to_string(),
        vaults.0.mint.clone(),
        vaults.1.mint.clone(),
        ata(&trader_key, &base_mint, &base_program).to_string(),
        ata(&trader_key, &quote_mint, &quote_program).to_string(),
        vaults.0.address.clone(),
        vaults.1.address.clone(),
        protocol_fee_recipient.to_string(),
        ata(protocol_fee_recipient, &quote_mint, &quote_program).to_string(),
        vaults.0.program_id.clone(),
        vaults.1.program_id.clone(),
        solana_sdk::system_program::id().to_string(),
        super::ids::ATA_PROGRAM.to_string(),
        amm_event_authority().to_string(),
        PUMP_AMM_PROGRAM.to_string(),
        ata(&creator_vault, &quote_mint, &quote_program).to_string(),
        creator_vault.to_string(),
        amm_global_volume().to_string(),
        user_volume.to_string(),
        amm_fee_config().to_string(),
        PUMP_FEE_PROGRAM.to_string(),
        pool_v2_pda(&base_mint).to_string(),
        ata(&user_volume, &quote_mint, &quote_program).to_string(),
    ] {
        known.insert(key);
    }
    for ix in tx.instructions.iter().chain(tx.inner_instructions.iter()) {
        if ix.data.len() < 8 || !SWAP_DISCS.iter().any(|disc| ix.data.starts_with(disc)) {
            continue;
        }
        if tx.account_keys.get(ix.program_id_index).map(String::as_str) != Some(PUMP_AMM_PROGRAM.to_string().as_str()) {
            continue;
        }
        let keys: Vec<&str> = ix.accounts.iter().filter_map(|index| tx.account_keys.get(*index).map(String::as_str)).filter(|key| !known.contains(*key)).collect();
        for index in 0..keys.len().saturating_sub(1) {
            let recipient = keys[index];
            let Ok(recipient_key) = recipient.parse::<Pubkey>() else { continue };
            let ata = ata(&recipient_key, &quote_mint, &quote_program).to_string();
            if keys.get(index + 1).copied() == Some(ata.as_str()) {
                return Some(recipient.to_string());
            }
        }
    }
    None
}

pub fn fill_from_logs(logs: &[String], owner: &Pubkey, mint: &str, venue: &str) -> Option<(u64, u64)> {
    for log in logs {
        let Some(encoded) = log.strip_prefix("Program data: ") else { continue };
        let Ok(bytes) = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, encoded) else { continue };
        if bytes.len() < 8 {
            continue;
        }
        let (disc, body) = bytes.split_at(8);
        if venue == "pump" && disc == TRADE_EVENT {
            let mut c = Cursor::new(body);
            let event_mint = c.pubkey()?;
            let sol = c.u64()?;
            let token = c.u64()?;
            let _buy = c.bool()?;
            let user = c.pubkey()?;
            if event_mint.to_string() == mint && user == *owner && token > 0 {
                return Some((token, sol));
            }
        }
        if venue == "pumpswap" && (disc == BUY_EVENT || disc == SELL_EVENT) {
            let mut c = Cursor::new(body);
            let _ts = c.i64()?;
            let base = c.u64()?;
            let _limit = c.u64()?;
            let _ub = c.u64()?;
            let _uq = c.u64()?;
            let _pb = c.u64()?;
            let _pq = c.u64()?;
            let quote = c.u64()?;
            for _ in 0..6 {
                let _ = c.u64()?;
            }
            let _pool = c.pubkey()?;
            let user = c.pubkey()?;
            let user_base_ata = c.pubkey()?;
            let Ok(mint_key) = mint.parse::<Pubkey>() else { continue };
            let ata_legacy = ata(owner, &mint_key, &TOKEN_PROGRAM);
            let ata_2022 = ata(owner, &mint_key, &TOKEN_2022_PROGRAM);
            if user == *owner && base > 0 && (user_base_ata == ata_legacy || user_base_ata == ata_2022) {
                return Some((base, quote));
            }
        }
    }
    None
}

pub fn program_ids(account_keys: &[String], instructions: &[CompiledIx]) -> Vec<String> {
    instructions.iter().filter_map(|ix| account_keys.get(ix.program_id_index).cloned()).collect()
}
