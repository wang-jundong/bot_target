//! SOL ↔ quote leg for mints that are not themselves pump coins (USDC, xStocks, …).
//! The leg is a list of instructions plus address lookup tables, so it can sit in
//! the same transaction as the pump buy or sell.

use anyhow::{Context, Result, bail};
use base64::Engine;
use serde_json::Value;
use solana_sdk::instruction::{AccountMeta, Instruction};
use solana_sdk::message::AddressLookupTableAccount;
use solana_sdk::pubkey::Pubkey;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

const QUOTE_URL: &str = "https://lite-api.jup.ag/swap/v1/quote";
/// One call returns the swap instructions and the lookup-table addresses.
const BUILD_URL: &str = "https://lite-api.jup.ag/swap/v2/build";
/// Jupiter's default of 64 fills the v0 packet. The pump buy, ATA creates, and tip share it.
const MAX_ACCOUNTS: u8 = 32;
const COMPUTE_BUDGET: &str = "ComputeBudget111111111111111111111111111111";

#[derive(Clone)]
pub struct Leg {
    pub instructions: Vec<Instruction>,
    pub lookup_tables: Vec<AddressLookupTableAccount>,
    pub min_out: u64,
}

/// A prepared leg stays usable across one background refresh. The worker
/// rebuilds about every 8 seconds, and immediately after a leg is sent.
const LEG_USABLE: Duration = Duration::from_secs(12);
const REFRESH_GAP: Duration = Duration::from_secs(8);
const RATE_LIMIT_GAP: Duration = Duration::from_secs(8);
const WARM_IDLE: Duration = Duration::from_secs(8);
const RATE_FOR_TAPE: Duration = Duration::from_secs(60);
const RATE_FOR_ROUTE: Duration = Duration::from_secs(10);

#[derive(Clone, PartialEq, Eq, Hash)]
struct SwapKey {
    user: Pubkey,
    input: Pubkey,
    output: Pubkey,
    amount: u64,
    slippage_bps: u64,
}

struct CachedLeg {
    leg: Leg,
    at: Instant,
    served: bool,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct SellKey {
    user: Pubkey,
    terminal: Pubkey,
    token_amount: u64,
}

struct SellMemo {
    aim: u64,
    slippage_bps: u64,
    at: Instant,
    served: bool,
}

fn legs() -> &'static Mutex<HashMap<SwapKey, CachedLeg>> {
    static LEGS: OnceLock<Mutex<HashMap<SwapKey, CachedLeg>>> = OnceLock::new();
    LEGS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn sells() -> &'static Mutex<HashMap<SellKey, SellMemo>> {
    static SELLS: OnceLock<Mutex<HashMap<SellKey, SellMemo>>> = OnceLock::new();
    SELLS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn touches() -> &'static Mutex<HashMap<SwapKey, Instant>> {
    static TOUCHES: OnceLock<Mutex<HashMap<SwapKey, Instant>>> = OnceLock::new();
    TOUCHES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn running() -> &'static Mutex<std::collections::HashSet<SwapKey>> {
    static RUNNING: OnceLock<Mutex<std::collections::HashSet<SwapKey>>> = OnceLock::new();
    RUNNING.get_or_init(|| Mutex::new(std::collections::HashSet::new()))
}

fn flight() -> &'static Mutex<HashMap<SwapKey, Arc<tokio::sync::Mutex<()>>>> {
    static FLIGHT: OnceLock<Mutex<HashMap<SwapKey, Arc<tokio::sync::Mutex<()>>>>> = OnceLock::new();
    FLIGHT.get_or_init(|| Mutex::new(HashMap::new()))
}

struct CachedRate {
    rate: f64,
    at: Instant,
}

fn rates() -> &'static Mutex<HashMap<Pubkey, CachedRate>> {
    static RATES: OnceLock<Mutex<HashMap<Pubkey, CachedRate>>> = OnceLock::new();
    RATES.get_or_init(|| Mutex::new(HashMap::new()))
}

pub async fn lamports_per_raw(mint: &Pubkey, decimals: u8) -> Result<f64> {
    if let Some(rate) = cached_rate(mint, RATE_FOR_ROUTE) {
        return Ok(rate);
    }
    let unit = 10u64.saturating_pow(u32::from(decimals.min(9))).max(1);
    let quote = quote(mint, &super::ids::WSOL_MINT, unit, 50).await?;
    let out = u64_field(&quote, "outAmount")?;
    if out == 0 {
        bail!("no SOL price for {mint}");
    }
    let rate = out as f64 / unit as f64;
    remember_rate(mint, rate);
    Ok(rate)
}

/// SOL per raw unit of a quote mint, from a recent Jupiter price.
pub fn cached_rate(mint: &Pubkey, max_age: Duration) -> Option<f64> {
    let rates = rates().lock().expect("jupiter rates");
    let cached = rates.get(mint)?;
    if cached.at.elapsed() > max_age || !cached.rate.is_finite() || cached.rate <= 0.0 {
        return None;
    }
    Some(cached.rate)
}

pub fn remember_rate(mint: &Pubkey, rate: f64) {
    if !rate.is_finite() || rate <= 0.0 {
        return;
    }
    rates().lock().expect("jupiter rates").insert(
        *mint,
        CachedRate {
            rate,
            at: Instant::now(),
        },
    );
}

pub fn tape_rate(mint: &Pubkey) -> Option<f64> {
    cached_rate(mint, RATE_FOR_TAPE)
}

/// Keep a SOL leg warm while a strategy is watching or holding the coin.
/// The send path then reuses it instead of waiting on Jupiter.
pub fn touch(user: Pubkey, input: Pubkey, output: Pubkey, amount: u64, slippage_bps: u64) {
    if amount == 0 {
        return;
    }
    let key = SwapKey {
        user,
        input,
        output,
        amount,
        slippage_bps,
    };
    touches()
        .lock()
        .expect("jupiter touches")
        .insert(key.clone(), Instant::now());
    if !running().lock().expect("jupiter warm").insert(key.clone()) {
        return;
    }
    tokio::spawn(async move {
        loop {
            let pause = if fresh_unserved(&key, REFRESH_GAP).is_some() {
                REFRESH_GAP
            } else {
                match refresh_once(&key).await {
                    Ok(_) => REFRESH_GAP,
                    Err(err) if err.to_string().contains("429") => RATE_LIMIT_GAP,
                    Err(_) => Duration::from_secs(2),
                }
            };
            let started = Instant::now();
            loop {
                let idle = touches()
                    .lock()
                    .expect("jupiter touches")
                    .get(&key)
                    .map(|at| at.elapsed() > WARM_IDLE)
                    .unwrap_or(true);
                if idle {
                    running().lock().expect("jupiter warm").remove(&key);
                    return;
                }
                let served = legs()
                    .lock()
                    .expect("jupiter legs")
                    .get(&key)
                    .is_some_and(|cached| cached.served);
                if served || started.elapsed() >= pause {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    });
}

/// Remember which quote amount a token sell should hand to Jupiter.
pub fn remember_sell(
    user: Pubkey,
    terminal: Pubkey,
    token_amount: u64,
    aim: u64,
    slippage_bps: u64,
) {
    if token_amount == 0 || aim == 0 {
        return;
    }
    let key = SellKey {
        user,
        terminal,
        token_amount,
    };
    let mut sells = sells().lock().expect("jupiter sells");
    if let Some(existing) = sells.get_mut(&key)
        && existing.aim == aim
        && existing.slippage_bps == slippage_bps
    {
        existing.at = Instant::now();
        return;
    }
    sells.insert(
        key,
        SellMemo {
            aim,
            slippage_bps,
            at: Instant::now(),
            served: false,
        },
    );
}

/// Cached sell leg for this exact token amount. One use, so a retry rebuilds it.
pub fn take_sell(user: &Pubkey, terminal: &Pubkey, token_amount: u64) -> Option<(u64, Leg)> {
    let key = SellKey {
        user: *user,
        terminal: *terminal,
        token_amount,
    };
    let (aim, slippage) = {
        let mut sells = sells().lock().expect("jupiter sells");
        let memo = sells.get_mut(&key)?;
        if memo.served || memo.at.elapsed() > LEG_USABLE {
            return None;
        }
        (memo.aim, memo.slippage_bps)
    };
    let leg = take(&SwapKey {
        user: *user,
        input: *terminal,
        output: *super::ids::WSOL_MINT,
        amount: aim,
        slippage_bps: slippage,
    })?;
    if let Some(memo) = sells().lock().expect("jupiter sells").get_mut(&key) {
        memo.served = true;
    }
    tracing::info!(token_amount, aim, "[ROUTE] Using cached Jupiter SOL leg");
    Some((aim, leg))
}

pub async fn swap(
    user: &Pubkey,
    input: &Pubkey,
    output: &Pubkey,
    amount: u64,
    slippage_bps: u64,
) -> Result<Leg> {
    if amount == 0 {
        bail!("quote leg amount is zero");
    }
    let key = SwapKey {
        user: *user,
        input: *input,
        output: *output,
        amount,
        slippage_bps,
    };
    if let Some(leg) = take(&key) {
        tracing::info!(
            amount,
            input = %input,
            output = %output,
            "[ROUTE] Using cached Jupiter SOL leg"
        );
        return Ok(leg);
    }
    if refresh_in_flight(&key)
        && let Some(leg) = reuse_while_refresh(&key)
    {
        tracing::info!(
            amount,
            input = %input,
            output = %output,
            "[ROUTE] Using Jupiter SOL leg while a refresh is in flight"
        );
        return Ok(leg);
    }
    let leg = refresh_once(&key).await?;
    mark_served(&key);
    Ok(leg)
}

async fn refresh_once(key: &SwapKey) -> Result<Leg> {
    let gate = {
        let mut flight = flight().lock().expect("jupiter flight");
        flight
            .entry(key.clone())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    };
    let _hold = gate.lock().await;
    if let Some(leg) = peek_recent(key) {
        return Ok(leg);
    }
    let leg = fetch_leg(
        &key.user,
        &key.input,
        &key.output,
        key.amount,
        key.slippage_bps,
    )
    .await?;
    legs().lock().expect("jupiter legs").insert(
        key.clone(),
        CachedLeg {
            leg: leg.clone(),
            at: Instant::now(),
            served: false,
        },
    );
    Ok(leg)
}

fn peek_recent(key: &SwapKey) -> Option<Leg> {
    let legs = legs().lock().expect("jupiter legs");
    let cached = legs.get(key)?;
    if cached.served || cached.at.elapsed() > Duration::from_millis(800) {
        return None;
    }
    Some(cached.leg.clone())
}

fn take(key: &SwapKey) -> Option<Leg> {
    let mut legs = legs().lock().expect("jupiter legs");
    let cached = legs.get_mut(key)?;
    if cached.served || cached.at.elapsed() > LEG_USABLE {
        return None;
    }
    cached.served = true;
    Some(cached.leg.clone())
}

/// A send that arrives while Jupiter is rebuilding uses the previous leg
/// instead of waiting out the HTTP call.
fn reuse_while_refresh(key: &SwapKey) -> Option<Leg> {
    let mut legs = legs().lock().expect("jupiter legs");
    let cached = legs.get_mut(key)?;
    if cached.at.elapsed() > LEG_USABLE {
        return None;
    }
    cached.served = true;
    Some(cached.leg.clone())
}

fn fresh_unserved(key: &SwapKey, max_age: Duration) -> Option<Leg> {
    let legs = legs().lock().expect("jupiter legs");
    let cached = legs.get(key)?;
    if cached.served || cached.at.elapsed() > max_age {
        return None;
    }
    Some(cached.leg.clone())
}

fn refresh_in_flight(key: &SwapKey) -> bool {
    let flight = flight().lock().expect("jupiter flight");
    let Some(gate) = flight.get(key) else {
        return false;
    };
    gate.try_lock().is_err()
}

fn mark_served(key: &SwapKey) {
    if let Some(cached) = legs().lock().expect("jupiter legs").get_mut(key) {
        cached.served = true;
    }
}

async fn fetch_leg(
    user: &Pubkey,
    input: &Pubkey,
    output: &Pubkey,
    amount: u64,
    slippage_bps: u64,
) -> Result<Leg> {
    let url = format!(
        "{BUILD_URL}?inputMint={input}&outputMint={output}&amount={amount}&taker={user}&slippageBps={slippage_bps}&maxAccounts={MAX_ACCOUNTS}"
    );
    let response = http().get(&url).send().await.context("jupiter build")?;
    let status = response.status();
    let text = response.text().await.context("jupiter build body")?;
    if !status.is_success() {
        bail!("jupiter build HTTP {status}: {}", clip(&text));
    }
    let json: Value = serde_json::from_str(&text).context("jupiter build json")?;
    let min_out = u64_field(&json, "otherAmountThreshold")?;
    if min_out == 0 {
        bail!("quote leg from {input} to {output} returns zero");
    }
    let mut instructions = Vec::new();
    for key in [
        "setupInstructions",
        "swapInstruction",
        "cleanupInstruction",
        "otherInstructions",
    ] {
        push_instructions(&mut instructions, user, json.get(key))?;
    }
    if instructions.is_empty() {
        bail!("jupiter returned no swap instructions");
    }
    Ok(Leg {
        instructions,
        lookup_tables: tables_from_build(&json),
        min_out,
    })
}

async fn quote(input: &Pubkey, output: &Pubkey, amount: u64, slippage_bps: u64) -> Result<Value> {
    let url = format!(
        "{QUOTE_URL}?inputMint={input}&outputMint={output}&amount={amount}&slippageBps={slippage_bps}&restrictIntermediateTokens=true&maxAccounts={MAX_ACCOUNTS}"
    );
    let response = http().get(&url).send().await.context("jupiter quote")?;
    let status = response.status();
    let text = response.text().await.context("jupiter quote body")?;
    if !status.is_success() {
        bail!(
            "jupiter quote {input} -> {output} HTTP {status}: {}",
            clip(&text)
        );
    }
    serde_json::from_str(&text).context("jupiter quote json")
}

fn alt_cache() -> &'static Mutex<HashMap<Pubkey, Vec<Pubkey>>> {
    static ALTS: OnceLock<Mutex<HashMap<Pubkey, Vec<Pubkey>>>> = OnceLock::new();
    ALTS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn tables_from_build(json: &Value) -> Vec<AddressLookupTableAccount> {
    let Some(map) = json
        .get("addressesByLookupTableAddress")
        .and_then(Value::as_object)
    else {
        return Vec::new();
    };
    let mut tables = Vec::new();
    let mut cache = alt_cache().lock().expect("alt cache");
    for (key, addrs) in map {
        let Ok(key) = key.parse::<Pubkey>() else {
            continue;
        };
        let Some(list) = addrs.as_array() else {
            continue;
        };
        let addresses: Vec<Pubkey> = list
            .iter()
            .filter_map(|item| item.as_str()?.parse().ok())
            .collect();
        if addresses.is_empty() {
            continue;
        }
        cache.insert(key, addresses.clone());
        tables.push(AddressLookupTableAccount { key, addresses });
    }
    tables
}

fn push_instructions(
    out: &mut Vec<Instruction>,
    user: &Pubkey,
    value: Option<&Value>,
) -> Result<()> {
    let Some(value) = value else {
        return Ok(());
    };
    if value.is_null() {
        return Ok(());
    }
    if let Some(list) = value.as_array() {
        for item in list {
            if let Some(ix) = one_instruction(user, item)? {
                out.push(ix);
            }
        }
        return Ok(());
    }
    if let Some(ix) = one_instruction(user, value)? {
        out.push(ix);
    }
    Ok(())
}

fn one_instruction(user: &Pubkey, value: &Value) -> Result<Option<Instruction>> {
    let Some(program) = value.get("programId").and_then(Value::as_str) else {
        return Ok(None);
    };
    if program == COMPUTE_BUDGET {
        return Ok(None);
    }
    let program_id: Pubkey = program
        .parse()
        .with_context(|| format!("program {program}"))?;
    let mut accounts = Vec::new();
    if let Some(list) = value.get("accounts").and_then(Value::as_array) {
        for account in list {
            let key = account
                .get("pubkey")
                .and_then(Value::as_str)
                .context("jupiter account pubkey")?;
            let pubkey: Pubkey = key.parse().with_context(|| format!("account {key}"))?;
            let signer = account
                .get("isSigner")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if signer && pubkey != *user {
                bail!("jupiter instruction needs another signer: {pubkey}");
            }
            accounts.push(AccountMeta {
                pubkey,
                is_signer: signer,
                is_writable: account
                    .get("isWritable")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            });
        }
    }
    let data = value.get("data").and_then(Value::as_str).unwrap_or("");
    let data = if data.is_empty() {
        Vec::new()
    } else {
        base64::engine::general_purpose::STANDARD
            .decode(data)
            .context("jupiter instruction data")?
    };
    Ok(Some(Instruction {
        program_id,
        accounts,
        data,
    }))
}

fn u64_field(value: &Value, key: &str) -> Result<u64> {
    value
        .get(key)
        .and_then(|item| {
            item.as_str()
                .and_then(|text| text.parse().ok())
                .or_else(|| item.as_u64())
        })
        .with_context(|| format!("jupiter field {key}"))
}

fn clip(body: &str) -> String {
    let mut text = body.trim().to_string();
    if text.len() > 300 {
        text.truncate(300);
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cached_leg_is_spent_once() {
        let key = SwapKey {
            user: Pubkey::new_unique(),
            input: Pubkey::new_unique(),
            output: Pubkey::new_unique(),
            amount: 10,
            slippage_bps: 100,
        };
        legs().lock().expect("legs").insert(
            key.clone(),
            CachedLeg {
                leg: Leg {
                    instructions: Vec::new(),
                    lookup_tables: Vec::new(),
                    min_out: 7,
                },
                at: Instant::now(),
                served: false,
            },
        );
        assert_eq!(take(&key).expect("fresh leg").min_out, 7);
        assert!(take(&key).is_none());
    }

    #[test]
    fn a_served_leg_is_reused_while_refresh_runs() {
        let key = SwapKey {
            user: Pubkey::new_unique(),
            input: Pubkey::new_unique(),
            output: Pubkey::new_unique(),
            amount: 11,
            slippage_bps: 100,
        };
        legs().lock().expect("legs").insert(
            key.clone(),
            CachedLeg {
                leg: Leg {
                    instructions: Vec::new(),
                    lookup_tables: Vec::new(),
                    min_out: 7,
                },
                at: Instant::now() - Duration::from_secs(6),
                served: true,
            },
        );
        assert!(take(&key).is_none());
        assert_eq!(reuse_while_refresh(&key).expect("stale leg").min_out, 7);
        legs().lock().expect("legs").get_mut(&key).expect("leg").at =
            Instant::now() - Duration::from_secs(13);
        assert!(reuse_while_refresh(&key).is_none());
    }

    #[test]
    fn a_quote_rate_is_reused() {
        let mint = Pubkey::new_unique();
        remember_rate(&mint, 3.5);
        assert_eq!(tape_rate(&mint), Some(3.5));
        assert_eq!(cached_rate(&mint, RATE_FOR_ROUTE), Some(3.5));
    }

    #[test]
    fn lookup_tables_keep_the_response_order() {
        let first = *super::super::ids::WSOL_MINT;
        let second = solana_sdk::pubkey::Pubkey::default();
        let raw = format!(
            r#"{{"addressesByLookupTableAddress":{{"{first}":["{first}"],"{second}":["{second}"]}}}}"#
        );
        let json: Value = serde_json::from_str(&raw).unwrap();
        let tables = tables_from_build(&json);
        assert_eq!(tables[0].key, first);
        assert_eq!(tables[1].key, second);
    }

    #[test]
    fn build_response_keeps_lookup_tables() {
        let table = Pubkey::new_unique();
        let address = Pubkey::new_unique();
        let json = serde_json::json!({
            "addressesByLookupTableAddress": {
                table.to_string(): [address.to_string()],
            }
        });
        let tables = tables_from_build(&json);
        assert_eq!(tables.len(), 1);
        assert_eq!(tables[0].addresses, vec![address]);
        assert_eq!(
            alt_cache().lock().expect("alts").get(&table).cloned(),
            Some(vec![address])
        );
    }

    #[test]
    fn lookup_table_addresses_are_reused() {
        let key = Pubkey::new_unique();
        let addresses = vec![Pubkey::new_unique(), Pubkey::new_unique()];
        alt_cache()
            .lock()
            .expect("alts")
            .insert(key, addresses.clone());
        assert_eq!(
            alt_cache().lock().expect("alts").get(&key).cloned(),
            Some(addresses)
        );
    }
}

fn http() -> &'static reqwest::Client {
    static HTTP: OnceLock<reqwest::Client> = OnceLock::new();
    HTTP.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(4))
            .user_agent("bot_target")
            .build()
            .expect("http client")
    })
}
