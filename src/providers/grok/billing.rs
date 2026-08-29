//! Account-level plan usage for Grok.
//!
//! This is a different question from the token counters in the monitor. Those
//! say what a request cost; this says how close the account is to being cut
//! off. xAI meters the CLI against a rolling window — weekly on the current
//! plans — and reports progress through it as a percentage, never as tokens.
//!
//! The endpoint is the one the official CLI's `/usage` command calls. It is
//! undocumented, so every field is optional and read defensively: if xAI
//! renames or drops something, the result degrades to "unknown" instead of
//! failing the whole lookup.

use std::time::Duration;

use jiff::{Timestamp, Zoned, tz::TimeZone};
use serde_json::{Value, json};

use super::auth::manager::GrokAuthManager;
use super::auth::token_store::file_store;

const DEFAULT_BASE_URL: &str = "https://cli-chat-proxy.grok.com/v1";
const BILLING_QUERY: &str = "format=credits";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
/// Guards against an upstream that answers with something enormous; the real
/// payload is a few hundred bytes.
const MAX_RESPONSE_BYTES: usize = 256 * 1024;

/// How much of the plan window the account has burned through.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AccountUsage {
    pub used_percent: Option<f64>,
    pub period: Option<UsagePeriod>,
    pub products: Vec<ProductUsage>,
    pub on_demand_used: Option<f64>,
    pub on_demand_cap: Option<f64>,
    pub prepaid_balance: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct UsagePeriod {
    /// Normalised window name: `weekly`, `monthly`, `unknown`, ...
    pub kind: String,
    pub start: Option<Timestamp>,
    pub end: Option<Timestamp>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProductUsage {
    pub product: String,
    pub used_percent: Option<f64>,
}

impl UsagePeriod {
    /// Seconds until the window rolls over. `None` when the end is unknown,
    /// `Some(0)` once it has already passed.
    pub fn resets_in_seconds(&self, now: Timestamp) -> Option<i64> {
        let end = self.end?;
        Some((end.as_second() - now.as_second()).max(0))
    }
}

impl AccountUsage {
    pub fn to_json(&self) -> Value {
        self.to_json_at(Timestamp::now())
    }

    fn to_json_at(&self, now: Timestamp) -> Value {
        json!({
            "provider": "grok",
            "used_percent": self.used_percent,
            "window": self.period.as_ref().map(|period| json!({
                "kind": period.kind,
                "start": period.start.map(|value| value.to_string()),
                "end": period.end.map(|value| value.to_string()),
                "resets_in_seconds": period.resets_in_seconds(now),
            })),
            "products": self.products.iter().map(|product| json!({
                "product": product.product,
                "used_percent": product.used_percent,
            })).collect::<Vec<_>>(),
            "on_demand": {"used": self.on_demand_used, "cap": self.on_demand_cap},
            "prepaid_balance": self.prepaid_balance,
        })
    }

    pub fn summary(&self) -> String {
        self.summary_at(Timestamp::now(), TimeZone::system())
    }

    /// One-line form for the monitor header, where there is room for a few
    /// characters and nothing more.
    pub fn compact(&self) -> String {
        self.compact_at(Timestamp::now())
    }

    fn compact_at(&self, now: Timestamp) -> String {
        let used = self
            .used_percent
            .map_or_else(|| "?%".to_string(), format_percent);
        match self
            .period
            .as_ref()
            .and_then(|period| period.resets_in_seconds(now))
            .filter(|seconds| *seconds > 0)
        {
            Some(seconds) => format!("{used} · {}", format_span(seconds)),
            None => used,
        }
    }

    pub fn summary_at(&self, now: Timestamp, zone: TimeZone) -> String {
        let window = self
            .period
            .as_ref()
            .map_or("ventana", |period| window_label(&period.kind));
        let mut lines = vec![format!(
            "Grok · {window}: {} consumido",
            self.used_percent
                .map_or_else(|| "?%".to_string(), format_percent)
        )];

        if let Some(period) = self.period.as_ref()
            && let Some(end) = period.end
        {
            let renewal = Zoned::new(end, zone).strftime("%d/%m %H:%M").to_string();
            lines.push(match period.resets_in_seconds(now) {
                Some(0) | None => format!("Renueva el {renewal}"),
                Some(seconds) => format!("Renueva el {renewal} (faltan {})", format_span(seconds)),
            });
        }

        // Only worth a line when it says something the headline percentage does
        // not: a single product matching the total is noise.
        let detailed = self.products.len() > 1
            || self
                .products
                .first()
                .is_some_and(|product| product.used_percent != self.used_percent);
        if detailed {
            let products: Vec<_> = self
                .products
                .iter()
                .map(|product| {
                    format!(
                        "{} {}",
                        product.product,
                        product
                            .used_percent
                            .map_or_else(|| "?%".to_string(), format_percent)
                    )
                })
                .collect();
            lines.push(products.join(" · "));
        }

        let mut extras = Vec::new();
        if let (Some(used), Some(cap)) = (self.on_demand_used, self.on_demand_cap)
            && cap > 0.0
        {
            extras.push(format!(
                "on-demand {} de {}",
                format_amount(used),
                format_amount(cap)
            ));
        }
        if let Some(balance) = self.prepaid_balance.filter(|balance| *balance > 0.0) {
            extras.push(format!("saldo prepago {}", format_amount(balance)));
        }
        if !extras.is_empty() {
            lines.push(extras.join(" · "));
        }

        lines.join("\n")
    }
}

/// Reads the plan window straight from xAI. Needs a valid Grok login; the auth
/// manager refreshes the token when it is close to expiring.
pub async fn fetch_account_usage() -> anyhow::Result<AccountUsage> {
    let url = billing_url(&crate::config::grok_base_url())?;
    let auth = GrokAuthManager::new(file_store())?
        .get_auth()
        .await
        .map_err(|_| {
            anyhow::anyhow!("Sin autenticacion de Grok. Corre: claude-code-proxy grok auth login")
        })?;

    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .timeout(REQUEST_TIMEOUT)
        .build()?;
    let response = client
        .get(url)
        .header("authorization", format!("Bearer {}", auth.access))
        .send()
        .await
        .map_err(|_| anyhow::anyhow!("No se pudo contactar el endpoint de facturacion de Grok"))?;

    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!(
            "El endpoint de facturacion de Grok respondio HTTP {}",
            status.as_u16()
        );
    }
    if body.len() > MAX_RESPONSE_BYTES {
        anyhow::bail!("La respuesta de facturacion de Grok excede el limite de tamano");
    }
    let payload: Value = serde_json::from_str(&body)
        .map_err(|_| anyhow::anyhow!("Respuesta de facturacion de Grok ilegible"))?;
    Ok(parse_account_usage(&payload))
}

fn billing_url(base_url: &str) -> anyhow::Result<String> {
    let base_url = if base_url.trim().is_empty() {
        DEFAULT_BASE_URL
    } else {
        base_url.trim()
    };
    let mut url = reqwest::Url::parse(base_url)?;
    let path = url.path().trim_end_matches('/');
    url.set_path(&format!("{path}/billing"));
    url.set_query(Some(BILLING_QUERY));
    Ok(url.to_string())
}

pub fn parse_account_usage(payload: &Value) -> AccountUsage {
    // The live response nests everything under `config`; tolerate a flat one.
    let config = payload.get("config").unwrap_or(payload);
    AccountUsage {
        used_percent: amount(config.get("creditUsagePercent")),
        period: parse_period(config),
        products: config
            .get("productUsage")
            .and_then(Value::as_array)
            .map(|items| items.iter().filter_map(parse_product).collect())
            .unwrap_or_default(),
        on_demand_used: amount(config.get("onDemandUsed")),
        on_demand_cap: amount(config.get("onDemandCap")),
        prepaid_balance: amount(config.get("prepaidBalance")),
    }
}

fn parse_period(config: &Value) -> Option<UsagePeriod> {
    let current = config.get("currentPeriod");
    let kind = current
        .and_then(|period| period.get("type"))
        .and_then(Value::as_str)
        .map_or_else(|| "unknown".to_string(), period_kind);
    let start = current
        .and_then(|period| period.get("start"))
        .or_else(|| config.get("billingPeriodStart"))
        .and_then(timestamp);
    let end = current
        .and_then(|period| period.get("end"))
        .or_else(|| config.get("billingPeriodEnd"))
        .and_then(timestamp);
    if kind == "unknown" && start.is_none() && end.is_none() {
        return None;
    }
    Some(UsagePeriod { kind, start, end })
}

fn parse_product(entry: &Value) -> Option<ProductUsage> {
    let product = entry.get("product").and_then(Value::as_str)?;
    Some(ProductUsage {
        product: product.to_string(),
        used_percent: amount(entry.get("usagePercent")),
    })
}

/// Amounts arrive either bare or wrapped in `{"val": ...}`, and either as a
/// JSON number or as a string, so both shapes are accepted.
fn amount(value: Option<&Value>) -> Option<f64> {
    let value = value?;
    let inner = value.get("val").unwrap_or(value);
    match inner {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }
}

fn timestamp(value: &Value) -> Option<Timestamp> {
    value.as_str()?.parse().ok()
}

fn period_kind(raw: &str) -> String {
    raw.strip_prefix("USAGE_PERIOD_TYPE_")
        .unwrap_or(raw)
        .to_ascii_lowercase()
}

fn window_label(kind: &str) -> &str {
    match kind {
        "weekly" => "ventana semanal",
        "monthly" => "ventana mensual",
        "daily" => "ventana diaria",
        _ => "ventana",
    }
}

fn format_percent(value: f64) -> String {
    if (value - value.round()).abs() < 0.05 {
        format!("{:.0}%", value)
    } else {
        format!("{value:.1}%")
    }
}

fn format_amount(value: f64) -> String {
    if (value - value.round()).abs() < 0.005 {
        format!("{:.0}", value)
    } else {
        format!("{value:.2}")
    }
}

fn format_span(seconds: i64) -> String {
    let days = seconds / 86_400;
    let hours = (seconds % 86_400) / 3_600;
    let minutes = (seconds % 3_600) / 60;
    if days > 0 {
        return format!("{days}d {hours}h");
    }
    if hours > 0 {
        return format!("{hours}h {minutes}m");
    }
    format!("{minutes}m")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Captured verbatim from the live endpoint.
    fn live_payload() -> Value {
        json!({"config":{
            "currentPeriod":{
                "type":"USAGE_PERIOD_TYPE_WEEKLY",
                "start":"2026-07-24T14:13:23.762067+00:00",
                "end":"2026-07-31T14:13:23.762067+00:00"
            },
            "creditUsagePercent":18.0,
            "onDemandCap":{"val":0},
            "onDemandUsed":{"val":0},
            "productUsage":[{"product":"GrokBuild","usagePercent":18.0}],
            "isUnifiedBillingUser":true,
            "prepaidBalance":{"val":0},
            "topUpMethod":"TOP_UP_METHOD_SAVED_PAYMENT_METHOD",
            "billingPeriodStart":"2026-07-24T14:13:23.762067+00:00",
            "billingPeriodEnd":"2026-07-31T14:13:23.762067+00:00"
        }})
    }

    fn at(text: &str) -> Timestamp {
        text.parse().unwrap()
    }

    #[test]
    fn parses_the_live_response() {
        let usage = parse_account_usage(&live_payload());

        assert_eq!(usage.used_percent, Some(18.0));
        let period = usage.period.as_ref().unwrap();
        assert_eq!(period.kind, "weekly");
        assert_eq!(period.end, Some(at("2026-07-31T14:13:23.762067Z")));
        assert_eq!(
            usage.products,
            vec![ProductUsage {
                product: "GrokBuild".to_string(),
                used_percent: Some(18.0),
            }]
        );
        assert_eq!(usage.on_demand_cap, Some(0.0));
        assert_eq!(usage.prepaid_balance, Some(0.0));
    }

    #[test]
    fn summarises_the_window_and_what_is_left_of_it() {
        let usage = parse_account_usage(&live_payload());

        let summary = usage.summary_at(at("2026-07-25T12:00:00Z"), TimeZone::UTC);

        assert_eq!(
            summary,
            "Grok · ventana semanal: 18% consumido\nRenueva el 31/07 14:13 (faltan 6d 2h)"
        );
    }

    #[test]
    fn a_single_product_matching_the_headline_adds_no_line() {
        let usage = parse_account_usage(&live_payload());
        assert!(!usage.summary().contains("GrokBuild"));
    }

    #[test]
    fn per_product_detail_shows_when_it_differs_from_the_total() {
        let mut payload = live_payload();
        payload["config"]["productUsage"] = json!([
            {"product":"GrokBuild","usagePercent":18.0},
            {"product":"GrokChat","usagePercent":62.5}
        ]);

        let summary =
            parse_account_usage(&payload).summary_at(at("2026-07-25T12:00:00Z"), TimeZone::UTC);

        assert!(
            summary.contains("GrokBuild 18% · GrokChat 62.5%"),
            "{summary}"
        );
    }

    #[test]
    fn on_demand_and_prepaid_only_show_when_they_carry_a_balance() {
        let mut payload = live_payload();
        payload["config"]["onDemandCap"] = json!({"val": "50"});
        payload["config"]["onDemandUsed"] = json!({"val": 12.5});
        payload["config"]["prepaidBalance"] = json!({"val": 4});

        let usage = parse_account_usage(&payload);
        assert_eq!(usage.on_demand_cap, Some(50.0));
        assert_eq!(usage.on_demand_used, Some(12.5));

        let summary = usage.summary_at(at("2026-07-25T12:00:00Z"), TimeZone::UTC);
        assert!(
            summary.contains("on-demand 12.50 de 50 · saldo prepago 4"),
            "{summary}"
        );
    }

    #[test]
    fn an_unrecognisable_payload_degrades_instead_of_failing() {
        let usage = parse_account_usage(&json!({"config": {"somethingElse": true}}));

        assert_eq!(usage, AccountUsage::default());
        assert_eq!(
            usage.summary_at(at("2026-07-25T12:00:00Z"), TimeZone::UTC),
            "Grok · ventana: ?% consumido"
        );
    }

    #[test]
    fn an_elapsed_window_reports_no_remaining_time() {
        let usage = parse_account_usage(&live_payload());
        let period = usage.period.as_ref().unwrap();

        assert_eq!(
            period.resets_in_seconds(at("2026-08-02T00:00:00Z")),
            Some(0)
        );
        assert!(
            !usage
                .summary_at(at("2026-08-02T00:00:00Z"), TimeZone::UTC)
                .contains("faltan")
        );
    }

    #[test]
    fn json_carries_the_window_and_the_countdown() {
        let value = parse_account_usage(&live_payload()).to_json_at(at("2026-07-25T12:00:00Z"));

        assert_eq!(value["used_percent"], 18.0);
        assert_eq!(value["window"]["kind"], "weekly");
        assert_eq!(value["window"]["resets_in_seconds"], 526_403);
        assert_eq!(value["products"][0]["product"], "GrokBuild");
    }

    #[test]
    fn the_compact_form_fits_a_header() {
        let usage = parse_account_usage(&live_payload());

        assert_eq!(usage.compact_at(at("2026-07-25T12:00:00Z")), "18% · 6d 2h");
        // Once the window has closed there is no countdown left to show.
        assert_eq!(usage.compact_at(at("2026-08-02T00:00:00Z")), "18%");
        assert_eq!(
            AccountUsage::default().compact_at(at("2026-07-25T12:00:00Z")),
            "?%"
        );
    }

    #[test]
    fn billing_url_hangs_off_the_configured_base() {
        assert_eq!(
            billing_url("https://cli-chat-proxy.grok.com/v1").unwrap(),
            "https://cli-chat-proxy.grok.com/v1/billing?format=credits"
        );
        assert_eq!(
            billing_url("  ").unwrap(),
            "https://cli-chat-proxy.grok.com/v1/billing?format=credits"
        );
        assert!(billing_url(":invalid").is_err());
    }

    #[test]
    fn spans_read_in_the_largest_useful_unit() {
        assert_eq!(format_span(526_403), "6d 2h");
        assert_eq!(format_span(7_500), "2h 5m");
        assert_eq!(format_span(90), "1m");
    }
}
