use crate::events::PoolDescriptor;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::fs;
use tokio::sync::Mutex;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct PositionPrices {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry_signal_price: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_entry_price: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actual_entry_fill_price: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_mark_price: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_signal_price: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_exit_price: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actual_exit_fill_price: Option<f64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JournalRecord {
    pub event: Option<String>,
    #[serde(default)]
    pub strategy: Option<String>,
    pub descriptor: Option<PoolDescriptor>,
    pub lifecycle: Option<String>,
    #[serde(default)]
    pub actual_token_amount: Option<String>,
    #[serde(default)]
    pub actual_entry_sol_amount: Option<String>,
    #[serde(default)]
    pub buy_lamports: Option<String>,
    #[serde(default)]
    pub entry_processed_ms: Option<u64>,
    #[serde(default)]
    pub signature: Option<String>,
    #[serde(default)]
    pub buy_signature: Option<String>,
    #[serde(default)]
    pub prices: Option<JournalPrices>,
    #[serde(default)]
    pub fill: Option<JournalFill>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JournalPrices {
    #[serde(default)]
    pub actual_entry_fill_price: Option<f64>,
    #[serde(default)]
    pub current_mark_price: Option<f64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JournalFill {
    #[serde(default)]
    pub sol_amount: Option<String>,
    #[serde(default)]
    pub token_amount: Option<String>,
    #[serde(default)]
    pub price: Option<f64>,
}

pub struct RecoveryJournal {
    path: PathBuf,
    pending: Arc<Mutex<()>>,
}

impl RecoveryJournal {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into(), pending: Arc::new(Mutex::new(())) }
    }

    /// Fire-and-forget append, matching bot_target RecoveryJournal.record().
    pub fn record(&self, value: Value) {
        let path = self.path.clone();
        let pending = Arc::clone(&self.pending);
        tokio::spawn(async move {
            let line = format!("{value}\n");
            let _guard = pending.lock().await;
            let _ = append_line(&path, &line).await;
        });
    }

    /// Await flush for critical lifecycle lines (`buy_sent`, fills, closes).
    pub async fn record_critical(&self, value: Value) {
        let line = format!("{value}\n");
        let _guard = self.pending.lock().await;
        if let Err(err) = append_line(&self.path, &line).await {
            tracing::error!(error = %err, "[JOURNAL] Critical lifecycle append failed");
        }
    }

    pub async fn read(&self) -> Result<Vec<JournalRecord>> {
        let _guard = self.pending.lock().await;
        let text = match fs::read_to_string(&self.path).await {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(err.into()),
        };
        Ok(text
            .lines()
            .filter(|line| !line.is_empty())
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect())
    }
}

async fn append_line(path: &Path, line: &str) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).await?;
    }
    let mut file = fs::OpenOptions::new().create(true).append(true).open(path).await?;
    file.write_all(line.as_bytes()).await?;
    file.flush().await?;
    Ok(())
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PnlRecord {
    pub timestamp: String,
    pub timestamp_ms: u64,
    pub mint: String,
    pub pool: String,
    pub program_id: String,
    pub venue: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_program: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quote_mint: Option<String>,
    pub outcome: String,
    pub buy_lamports: String,
    pub sell_lamports: String,
    pub pnl_lamports: String,
    pub buy_sol: f64,
    pub sell_sol: f64,
    pub pnl_sol: f64,
    pub pnl_pct: f64,
    pub token_amount: String,
    pub entry_price: f64,
    pub exit_price: f64,
    pub entry_market_cap_sol: f64,
    pub exit_market_cap_sol: f64,
    pub exit_reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub buy_signature: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sell_signature: Option<String>,
    pub entry_processed_ms: u64,
    pub holding_time_ms: u64,
    pub prices: PositionPrices,
}

pub struct ClosedTradePnlInput {
    pub closed_at_ms: u64,
    pub descriptor: PoolDescriptor,
    pub buy_lamports: u64,
    pub sell_lamports: u64,
    pub token_amount: u64,
    pub entry_price: f64,
    pub exit_price: f64,
    pub prices: PositionPrices,
    pub exit_reason: String,
    pub buy_signature: Option<String>,
    pub sell_signature: Option<String>,
    pub entry_processed_ms: u64,
}

pub fn create_pnl_record(input: ClosedTradePnlInput) -> Result<PnlRecord> {
    if input.buy_lamports == 0 {
        anyhow::bail!("confirmed BUY SOL amount must be positive");
    }
    let pnl = input.sell_lamports as i64 - input.buy_lamports as i64;
    let buy_sol = input.buy_lamports as f64 / crate::events::LAMPORTS_PER_SOL as f64;
    let sell_sol = input.sell_lamports as f64 / crate::events::LAMPORTS_PER_SOL as f64;
    Ok(PnlRecord {
        timestamp: chrono_like(input.closed_at_ms),
        timestamp_ms: input.closed_at_ms,
        mint: input.descriptor.mint,
        pool: input.descriptor.pool,
        program_id: input.descriptor.program_id,
        venue: input.descriptor.venue,
        token_program: input.descriptor.token_program,
        quote_mint: input.descriptor.quote_mint,
        outcome: if pnl > 0 { "win" } else if pnl < 0 { "loss" } else { "flat" }.to_string(),
        buy_lamports: input.buy_lamports.to_string(),
        sell_lamports: input.sell_lamports.to_string(),
        pnl_lamports: pnl.to_string(),
        buy_sol,
        sell_sol,
        pnl_sol: pnl as f64 / crate::events::LAMPORTS_PER_SOL as f64,
        pnl_pct: pnl as f64 / input.buy_lamports as f64 * 100.0,
        token_amount: input.token_amount.to_string(),
        entry_price: input.entry_price,
        exit_price: input.exit_price,
        entry_market_cap_sol: input.entry_price * crate::events::MARKET_CAP_MULTIPLIER,
        exit_market_cap_sol: input.exit_price * crate::events::MARKET_CAP_MULTIPLIER,
        exit_reason: input.exit_reason,
        buy_signature: input.buy_signature,
        sell_signature: input.sell_signature,
        entry_processed_ms: input.entry_processed_ms,
        holding_time_ms: input.closed_at_ms.saturating_sub(input.entry_processed_ms),
        prices: input.prices,
    })
}

fn chrono_like(ms: u64) -> String {
    // ISO-8601 without pulling chrono. Seconds resolution plus milliseconds.
    let secs = ms / 1000;
    let millis = ms % 1000;
    let days = secs / 86_400;
    let tod = secs % 86_400;
    let hour = tod / 3600;
    let min = (tod % 3600) / 60;
    let sec = tod % 60;
    let (year, month, day) = civil_from_days(days as i64);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{min:02}:{sec:02}.{millis:03}Z")
}

fn civil_from_days(z: i64) -> (i32, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y as i32, m as u32, d as u32)
}

pub struct PnlJournal {
    path: PathBuf,
    pending: Mutex<()>,
}

impl PnlJournal {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into(), pending: Mutex::new(()) }
    }

    pub async fn record(&self, input: ClosedTradePnlInput) -> Result<()> {
        let line = serde_json::to_string(&create_pnl_record(input)?)? + "\n";
        let _guard = self.pending.lock().await;
        append_line(&self.path, &line).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_camel_case_recovery_line() {
        let line = r#"{"event":"buy_confirmed","descriptor":{"mint":"m","pool":"p","programId":"prog","venue":"pump","relevantAccounts":["p"]},"actualTokenAmount":"10","buyLamports":"400000000","prices":{"actualEntryFillPrice":1.5}}"#;
        let record: JournalRecord = serde_json::from_str(line).unwrap();
        assert_eq!(record.actual_token_amount.as_deref(), Some("10"));
        assert_eq!(record.descriptor.unwrap().program_id, "prog");
        assert_eq!(record.prices.unwrap().actual_entry_fill_price, Some(1.5));
    }
}

pub fn workflow_record(event: &str, extra: Value) -> Value {
    let mut value = extra;
    if let Value::Object(map) = &mut value {
        map.insert("event".to_string(), json!(event));
    }
    value
}
