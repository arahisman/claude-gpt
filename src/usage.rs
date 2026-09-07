use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use codex_protocol::protocol::RateLimitSnapshot;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{BridgeError, Result};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageSnapshot {
    pub rate_limits: Vec<RateLimitSnapshot>,
    pub fetched_at_unix_seconds: u64,
}

impl UsageSnapshot {
    pub fn new(rate_limits: Vec<RateLimitSnapshot>) -> Self {
        let fetched_at_unix_seconds = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        Self {
            rate_limits,
            fetched_at_unix_seconds,
        }
    }

    pub fn from_json(value: Value) -> Result<Self> {
        #[derive(Deserialize)]
        struct WireSnapshot {
            rate_limits: Vec<RateLimitSnapshot>,
        }

        let wire = serde_json::from_value::<WireSnapshot>(value).map_err(|source| {
            BridgeError::InvalidRequest {
                path: "usage".to_string(),
                message: source.to_string(),
            }
        })?;
        Ok(Self::new(wire.rate_limits))
    }
}

#[derive(Debug)]
pub struct UsageCache {
    ttl: Duration,
    last_refresh: Option<Instant>,
    snapshot: Option<UsageSnapshot>,
}

impl UsageCache {
    pub fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            last_refresh: None,
            snapshot: None,
        }
    }

    pub fn should_refresh(&self, now: Instant) -> bool {
        self.last_refresh
            .is_none_or(|last_refresh| now.duration_since(last_refresh) >= self.ttl)
    }

    pub fn record(&mut self, snapshot: UsageSnapshot, now: Instant) {
        self.snapshot = Some(snapshot);
        self.last_refresh = Some(now);
    }

    pub fn snapshot(&self) -> Option<&UsageSnapshot> {
        self.snapshot.as_ref()
    }
}

pub fn render_usage(snapshot: &UsageSnapshot) -> String {
    if snapshot.rate_limits.is_empty() {
        return "Codex subscription usage: no rate-limit windows were returned.".to_string();
    }
    let mut lines = vec![
        "## Codex subscription usage".to_string(),
        String::new(),
        "| Model | Window | Used | Remaining | Resets (UTC) |".to_string(),
        "|---|---|---:|---:|---|".to_string(),
    ];
    let mut credit_balances = Vec::new();
    for limit in &snapshot.rate_limits {
        let label = limit
            .limit_name
            .as_deref()
            .or(limit.limit_id.as_deref())
            .unwrap_or("Unnamed limit");
        if let Some(primary) = &limit.primary {
            lines.push(format_window_row(label, "Primary", primary));
        } else {
            lines.push(format!(
                "| {label} | Primary | unavailable | unavailable | unavailable |"
            ));
        }
        if let Some(secondary) = &limit.secondary {
            lines.push(format_window_row(label, "Secondary", secondary));
        }
        if let Some(credits) = &limit.credits {
            let value = if credits.unlimited {
                "unlimited".to_string()
            } else {
                credits
                    .balance
                    .clone()
                    .unwrap_or_else(|| "balance unavailable".to_string())
            };
            credit_balances.push(format!("**{label}:** {value}"));
        }
    }
    if !credit_balances.is_empty() {
        lines.push(String::new());
        lines.push(format!("Credits: {}", credit_balances.join(" · ")));
    }
    lines.join("\n")
}

fn format_window_row(
    model: &str,
    window_name: &str,
    window: &codex_protocol::protocol::RateLimitWindow,
) -> String {
    let remaining = (100.0 - window.used_percent).clamp(0.0, 100.0);
    let duration = window_duration(window.window_minutes);
    let reset = window.resets_at.map_or_else(
        || "unavailable".to_string(),
        |timestamp| {
            chrono::DateTime::from_timestamp(timestamp, 0).map_or_else(
                || "unavailable".to_string(),
                |date| date.format("%Y-%m-%d %H:%M").to_string(),
            )
        },
    );
    format!(
        "| {model} | {window_name} ({duration}) | {:.1}% | **{remaining:.1}%** | {reset} |",
        window.used_percent
    )
}

fn window_duration(minutes: Option<i64>) -> String {
    match minutes {
        Some(minutes) if minutes % 1_440 == 0 => format!("{}d", minutes / 1_440),
        Some(minutes) if minutes % 60 == 0 => format!("{}h", minutes / 60),
        Some(minutes) => format!("{minutes} min"),
        None => "—".to_string(),
    }
}
