use anyhow::{bail, Result};
use reqwest::Client;
use serde_json::Value;
use std::time::Duration;

#[derive(Debug, Clone, Copy)]
pub struct SenderTiming {
    pub signal_mono_ms: f64,
    pub send_start_mono_ms: f64,
    pub response_mono_ms: f64,
}

#[derive(Debug, thiserror::Error)]
#[error("Helius Sender unavailable: HTTP {status_code}")]
pub struct HeliusSenderUnavailable {
    pub status_code: u16,
}

pub fn signature_from_sender_body(status_code: u16, body: &str) -> Result<String> {
    let trimmed = body.trim_start();
    if trimmed.starts_with('<') {
        return Err(HeliusSenderUnavailable { status_code }.into());
    }
    let json: Value = match serde_json::from_str(trimmed) {
        Ok(json) => json,
        Err(_) => return Err(HeliusSenderUnavailable { status_code }.into()),
    };
    if let Some(result) = json.get("result").and_then(Value::as_str) {
        return Ok(result.to_string());
    }
    if status_code >= 500 {
        return Err(HeliusSenderUnavailable { status_code }.into());
    }
    let message = json.pointer("/error/message").and_then(Value::as_str).unwrap_or("rejected");
    bail!("Helius Sender rejected transaction: {message}")
}

pub struct HeliusSender {
    url: String,
    swqos_only: bool,
    client: Client,
}

impl HeliusSender {
    pub fn new(url: String, swqos_only: bool) -> Self {
        let client = Client::builder()
            .pool_max_idle_per_host(8)
            .pool_idle_timeout(Duration::from_secs(60))
            .build()
            .unwrap_or_else(|_| Client::new());
        Self { url, swqos_only, client }
    }

    pub async fn send(&self, base64_transaction: &str, signal_mono_ms: f64) -> Result<(String, SenderTiming)> {
        let send_start = crate::events::mono_ms();
        let mut url = reqwest::Url::parse(&self.url)?;
        url.query_pairs_mut().append_pair("swqos_only", if self.swqos_only { "true" } else { "false" });
        let body = format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"sendTransaction","params":["{base64_transaction}",{{"encoding":"base64","skipPreflight":true,"maxRetries":0}}]}}"#
        );
        let response = self.client.post(url).header("content-type", "application/json").body(body).send().await?;
        let status = response.status().as_u16();
        let text = response.text().await?;
        match signature_from_sender_body(status, &text) {
            Ok(signature) => Ok((
                signature,
                SenderTiming { signal_mono_ms, send_start_mono_ms: send_start, response_mono_ms: crate::events::mono_ms() },
            )),
            Err(err) => Err(err),
        }
    }
}

pub fn is_non_retryable_buy_error(message: &str, venue: &str) -> bool {
    (venue == "pump" && message.contains("\"Custom\":6002")) || (venue == "pumpswap" && message.contains("\"Custom\":6004"))
}
