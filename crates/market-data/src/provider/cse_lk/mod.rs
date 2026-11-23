//! Colombo Stock Exchange (CSE) market data provider.
//!
//! Prices securities listed on the CSE (MIC `XCOL`, always in LKR) from the
//! public JSON feed behind www.cse.lk. No API key required. The feed is not
//! documented as an API, so everything below is observed behavior:
//!
//! - `companyInfoSummery` (upstream's spelling) turns a full CSE symbol such
//!   as `JKH.N0000` into the numeric `reqSymbolInfo.id` that the chart call
//!   takes, and supplies the name and ISIN. An unknown symbol is HTTP 404.
//! - `companyChartDataByStock` with `period=5` returns about a year of daily
//!   rows, oldest first. `t` is epoch milliseconds at Colombo midnight of the
//!   trading day, `p` the close, `h`/`l` high and low, `q` the share volume.
//!   No open is published, and there are no date parameters.
//!
//! The provider is daily-only: a row for the current Colombo session is held
//! back until the session has closed and its prices have had time to settle
//! (see [`PUBLICATION_GRACE_MINUTES`]), and no bar is ever built from the
//! undated current-session values of `companyInfoSummery`. Prices are the
//! feed's raw values; splits are not back-adjusted.
//!
//! Every request takes a token from one provider-owned limiter, because a
//! history operation makes two requests and identifier lookups take no
//! registry token.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::str::FromStr;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, FixedOffset, NaiveDate, NaiveTime, TimeDelta, TimeZone, Utc};
use log::debug;
use reqwest::{Client, StatusCode};
use rust_decimal::Decimal;
use serde_json::Value;

use crate::errors::MarketDataError;
use crate::models::{
    AssetProfile, Coverage, InstrumentKind, ProviderId, ProviderInstrument, Quote, QuoteContext,
    SearchResult,
};
use crate::provider::{MarketDataProvider, ProviderCapabilities, RateLimit};
use crate::registry::{RateLimitConfig, RateLimiter};

/// Provider ID. Not `CSE`, which `exchanges.json` also uses for the Canadian
/// Securities Exchange; this ID is persisted in quotes, sync state and settings.
pub const PROVIDER_ID: &str = "CSE_LK";
/// Why a reset of provider history is refused for this provider.
pub const RESET_REFUSAL: &str = "Reset is unavailable for the Colombo Stock Exchange (cse.lk) because its feed only reaches back about one year, so replacing the stored history would permanently delete older quotes";

const MIC: &str = "XCOL";
const CURRENCY: &str = "LKR";
const BASE_URL: &str = "https://www.cse.lk/api";
/// Requests say who sends them rather than imitating a browser.
const USER_AGENT: &str = "Wealthfolio";
/// The chart period with the longest history found, about one year.
const HISTORY_PERIOD: &str = "5";
/// Class suffix of an ordinary voting share, tried for a bare ticker.
const ORDINARY_CLASS: &str = ".N0000";
/// Sri Lanka observes UTC+05:30 all year.
const COLOMBO_UTC_OFFSET_SECS: i32 = 5 * 3600 + 30 * 60;
/// End of the regular trading session (14:30 Colombo time), used as the
/// timestamp of daily bars so their date is the trading date in every common
/// timezone.
const SESSION_CLOSE: (u32, u32) = (14, 30);
/// How long after [`SESSION_CLOSE`] a current-session row is still treated as
/// unfinished. The closing price is calculated after the session, and the
/// feed's publication delay is unknown.
const PUBLICATION_GRACE_MINUTES: i64 = 60;

// ---------------------------------------------------------------------------
// Pure helpers (unit tested)
// ---------------------------------------------------------------------------

fn colombo() -> FixedOffset {
    FixedOffset::east_opt(COLOMBO_UTC_OFFSET_SECS).expect("valid Colombo offset")
}

fn session_close_utc(date: NaiveDate) -> DateTime<Utc> {
    let close = NaiveTime::from_hms_opt(SESSION_CLOSE.0, SESSION_CLOSE.1, 0).expect("valid time");
    colombo()
        .from_local_datetime(&date.and_time(close))
        .single()
        .expect("fixed offsets are unambiguous")
        .with_timezone(&Utc)
}

/// Whether the session of `date` has closed and had its grace period by `now`.
/// Earlier sessions always have; the current one only after the cutoff.
fn session_published(date: NaiveDate, now: DateTime<Utc>) -> bool {
    now >= session_close_utc(date) + TimeDelta::minutes(PUBLICATION_GRACE_MINUTES)
}

/// The Colombo trading date of a chart row's `t`, which is epoch
/// milliseconds at Colombo midnight (so its UTC date is one day early).
fn trading_date(epoch_ms: i64) -> Option<NaiveDate> {
    DateTime::from_timestamp_millis(epoch_ms).map(|t| t.with_timezone(&colombo()).date_naive())
}

fn decimal(value: Option<&Value>) -> Option<Decimal> {
    match value? {
        // The JSON literal, not an f64, so 18.7 stays 18.7.
        Value::Number(number) => {
            let literal = number.to_string();
            Decimal::from_str(&literal)
                .or_else(|_| Decimal::from_scientific(&literal))
                .ok()
        }
        Value::String(text) => Decimal::from_str(text.trim()).ok(),
        _ => None,
    }
}

fn epoch_ms(value: Option<&Value>) -> Option<i64> {
    let value = value?;
    value.as_i64().or_else(|| {
        value
            .as_f64()
            .filter(|ms| ms.is_finite() && ms.fract() == 0.0)
            .map(|ms| ms as i64)
    })
}

fn text(value: Option<&Value>) -> Option<&str> {
    value?
        .as_str()
        .map(str::trim)
        .filter(|text| !text.is_empty())
}

/// A full CSE symbol, `JKH.N0000`: an issuer code, a dot, then a class letter
/// (`N` ordinary, `X` non-voting, `R` right, `U` fund unit, ...) and four
/// digits. Upper-cased; `None` for anything else.
fn full_symbol(query: &str) -> Option<String> {
    let symbol = query.trim().to_ascii_uppercase();
    let (code, class) = symbol.split_once('.')?;
    let valid_code =
        (1..=10).contains(&code.len()) && code.bytes().all(|b| b.is_ascii_alphanumeric());
    let valid_class = class.len() == 5
        && class.as_bytes()[0].is_ascii_alphabetic()
        && class.as_bytes()[1..].iter().all(u8::is_ascii_digit);
    (valid_code && valid_class).then_some(symbol)
}

/// The symbol a search looks up: a full symbol exactly, or a bare ticker of
/// 2-6 letters and digits as its ordinary share. `None` makes no request.
fn looked_up_symbol(query: &str) -> Option<String> {
    full_symbol(query).or_else(|| {
        let ticker = query.trim();
        ((2..=6).contains(&ticker.len()) && ticker.bytes().all(|b| b.is_ascii_alphanumeric()))
            .then(|| format!("{}{}", ticker.to_ascii_uppercase(), ORDINARY_CLASS))
    })
}

/// Fetches never guess: anything but a full symbol is not found.
fn instrument_symbol(instrument: &ProviderInstrument) -> Result<String, MarketDataError> {
    match instrument {
        ProviderInstrument::EquitySymbol { symbol } => {
            full_symbol(symbol).ok_or_else(|| MarketDataError::SymbolNotFound(symbol.to_string()))
        }
        other => Err(MarketDataError::UnsupportedAssetType(format!(
            "{:?}",
            other
        ))),
    }
}

/// Listed fund units (class `U`) are funds; everything else is reported as
/// equity.
fn quote_type(symbol: &str) -> &'static str {
    match symbol
        .split_once('.')
        .and_then(|(_, class)| class.chars().next())
    {
        Some('U') => "MUTUALFUND",
        _ => "EQUITY",
    }
}

/// The security a `companyInfoSummery` response describes.
#[derive(Debug, PartialEq)]
struct SecurityInfo {
    /// `reqSymbolInfo.id`, the chart call's `stockId`.
    stock_id: String,
    symbol: String,
    name: Option<String>,
    isin: Option<String>,
}

/// Read `reqSymbolInfo` for `requested`. A response for another symbol (say
/// the ordinary share when a non-voting one was asked for) is not a match.
fn security_info(body: &Value, requested: &str) -> Option<SecurityInfo> {
    let info = body.get("reqSymbolInfo")?;
    let stock_id = match info.get("id")? {
        Value::Number(id) => id.as_u64()?.to_string(),
        Value::String(id) if !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()) => id.clone(),
        _ => return None,
    };
    let symbol = match text(info.get("symbol")) {
        Some(symbol) if !symbol.eq_ignore_ascii_case(requested) => return None,
        _ => requested.to_string(),
    };
    Some(SecurityInfo {
        stock_id,
        symbol,
        name: text(info.get("name")).map(str::to_string),
        isin: text(info.get("isin")).map(str::to_string),
    })
}

/// Daily quotes of a `companyChartDataByStock` response by trading date,
/// skipping rows without a positive close and a current session that is not
/// yet published at `now`.
fn chart_quotes(
    body: &Value,
    now: DateTime<Utc>,
) -> Result<BTreeMap<NaiveDate, Quote>, MarketDataError> {
    let rows = body
        .get("chartData")
        .and_then(Value::as_array)
        .ok_or_else(|| provider_error("unexpected chart response".to_string()))?;
    let mut quotes = BTreeMap::new();
    for row in rows {
        let Some(date) = epoch_ms(row.get("t")).and_then(trading_date) else {
            continue;
        };
        let Some(close) = decimal(row.get("p")).filter(|close| *close > Decimal::ZERO) else {
            continue;
        };
        if !session_published(date, now) {
            continue;
        }
        quotes.insert(
            date,
            Quote {
                timestamp: session_close_utc(date),
                open: None,
                high: decimal(row.get("h")),
                low: decimal(row.get("l")),
                close,
                volume: decimal(row.get("q")),
                currency: CURRENCY.to_string(),
                source: PROVIDER_ID.to_string(),
            },
        );
    }
    Ok(quotes)
}

fn search_result(info: &SecurityInfo) -> SearchResult {
    SearchResult::new(
        &info.symbol,
        info.name.as_deref().unwrap_or(&info.symbol),
        MIC,
        quote_type(&info.symbol),
    )
    .with_exchange_mic(MIC)
    .with_exchange_name("Colombo Stock Exchange")
    .with_currency(CURRENCY)
    .with_data_source(PROVIDER_ID)
}

/// The limit on requests to cse.lk, shared by every operation: a token bucket
/// of two that refills at 30 a minute. The registry takes one token per
/// operation, but history makes two requests and identifier lookups take
/// none, so each request is metered here.
fn request_limit() -> RateLimitConfig {
    RateLimitConfig {
        requests_per_minute: 30,
        burst_capacity: 2.0,
    }
}

fn provider_error(message: String) -> MarketDataError {
    MarketDataError::ProviderError {
        provider: PROVIDER_ID.to_string(),
        message,
    }
}

fn request_error(error: reqwest::Error) -> MarketDataError {
    if error.is_timeout() {
        MarketDataError::Timeout {
            provider: PROVIDER_ID.to_string(),
        }
    } else {
        provider_error(format!("HTTP request failed: {error}"))
    }
}

// ---------------------------------------------------------------------------
// Provider
// ---------------------------------------------------------------------------

pub struct CseLkProvider {
    client: Client,
    base_url: Cow<'static, str>,
    request_limiter: RateLimiter,
    /// The current time; tests fix it to either side of a session's cutoff.
    clock: fn() -> DateTime<Utc>,
}

impl Default for CseLkProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl CseLkProvider {
    pub fn new() -> Self {
        Self::with_base_url(Cow::Borrowed(BASE_URL))
    }

    fn with_base_url(base_url: Cow<'static, str>) -> Self {
        let client = crate::provider::http_client_builder()
            .user_agent(USER_AGENT)
            .build()
            .unwrap_or_else(|_| wealthfolio_http::client());
        let request_limiter = RateLimiter::new();
        request_limiter.configure(&Cow::Borrowed(PROVIDER_ID), request_limit());
        Self {
            client,
            base_url,
            request_limiter,
            clock: Utc::now,
        }
    }

    /// POST a form to `endpoint` and read the JSON reply. Every request to
    /// cse.lk goes through here, so each takes a token from the limiter.
    async fn post_form(
        &self,
        endpoint: &str,
        form: &[(&str, &str)],
        symbol: &str,
    ) -> Result<Value, MarketDataError> {
        let provider_id: ProviderId = Cow::Borrowed(PROVIDER_ID);
        self.request_limiter.acquire(&provider_id).await;
        debug!("CSE_LK {} for {}", endpoint, symbol);
        let response = self
            .client
            .post(format!("{}/{}", self.base_url, endpoint))
            .form(form)
            .send()
            .await
            .map_err(request_error)?;
        match response.status() {
            StatusCode::NOT_FOUND => {
                return Err(MarketDataError::SymbolNotFound(format!(
                    "{}@{}",
                    symbol, MIC
                )))
            }
            StatusCode::TOO_MANY_REQUESTS => {
                return Err(MarketDataError::RateLimited {
                    provider: PROVIDER_ID.to_string(),
                })
            }
            status if !status.is_success() => {
                return Err(provider_error(format!("HTTP {status}")));
            }
            _ => {}
        }
        response.json::<Value>().await.map_err(|error| {
            if error.is_timeout() {
                request_error(error)
            } else {
                provider_error("unreadable JSON response".to_string())
            }
        })
    }

    /// One request: the security behind a full symbol.
    async fn security(&self, symbol: &str) -> Result<SecurityInfo, MarketDataError> {
        let body = self
            .post_form("companyInfoSummery", &[("symbol", symbol)], symbol)
            .await?;
        security_info(&body, symbol)
            .ok_or_else(|| MarketDataError::SymbolNotFound(format!("{}@{}", symbol, MIC)))
    }

    /// Two requests: every published daily quote the feed holds for a full
    /// symbol, by trading date.
    async fn daily_quotes(
        &self,
        symbol: &str,
    ) -> Result<BTreeMap<NaiveDate, Quote>, MarketDataError> {
        let security = self.security(symbol).await?;
        let body = self
            .post_form(
                "companyChartDataByStock",
                &[
                    ("stockId", security.stock_id.as_str()),
                    ("period", HISTORY_PERIOD),
                ],
                symbol,
            )
            .await?;
        chart_quotes(&body, (self.clock)())
    }
}

#[async_trait]
impl MarketDataProvider for CseLkProvider {
    fn id(&self) -> &'static str {
        PROVIDER_ID
    }

    fn priority(&self) -> u8 {
        16
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            instrument_kinds: &[InstrumentKind::Equity],
            coverage: Coverage::cse_lk(),
            supports_latest: true,
            supports_historical: true,
            // Not a free-text search: only symbols are looked up, through
            // `looks_up_identifier`.
            supports_search: false,
            supports_profile: true,
            supports_dividends: false,
        }
    }

    /// The registry's per-operation token. HTTP requests are metered
    /// separately by the provider's own limiter.
    fn rate_limit(&self) -> RateLimit {
        RateLimit {
            requests_per_minute: 30,
            max_concurrency: 2,
            min_delay: Duration::from_millis(500),
        }
    }

    fn looks_up_identifier(&self, query: &str) -> bool {
        looked_up_symbol(query).is_some()
    }

    /// Look up a full symbol, or a bare ticker's ordinary share. The result
    /// carries the full symbol, so the user sees what was matched.
    async fn search(&self, query: &str) -> Result<Vec<SearchResult>, MarketDataError> {
        let Some(symbol) = looked_up_symbol(query) else {
            return Ok(Vec::new());
        };
        match self.security(&symbol).await {
            Ok(info) => Ok(vec![search_result(&info)]),
            Err(MarketDataError::SymbolNotFound(_)) => Ok(Vec::new()),
            Err(error) => Err(error),
        }
    }

    async fn get_profile(&self, symbol: &str) -> Result<AssetProfile, MarketDataError> {
        let symbol = full_symbol(symbol)
            .ok_or_else(|| MarketDataError::SymbolNotFound(symbol.to_string()))?;
        let info = self.security(&symbol).await?;
        let mut profile = AssetProfile {
            source: Some(PROVIDER_ID.to_string()),
            name: info.name.clone(),
            quote_type: Some(quote_type(&info.symbol).to_string()),
            currency: Some(CURRENCY.to_string()),
            ..Default::default()
        };
        if let Some(isin) = info.isin.as_deref() {
            profile = profile.isin(isin);
        }
        Ok(profile)
    }

    /// The newest published daily quote, from the same path as history.
    async fn get_latest_quote(
        &self,
        _context: &QuoteContext,
        instrument: ProviderInstrument,
    ) -> Result<Quote, MarketDataError> {
        let symbol = instrument_symbol(&instrument)?;
        self.daily_quotes(&symbol)
            .await?
            .pop_last()
            .map(|(_, quote)| quote)
            .ok_or(MarketDataError::NoDataForRange)
    }

    /// The feed's year of daily quotes, whatever the range, kept to the
    /// trading dates within `start..=end`.
    async fn get_historical_quotes(
        &self,
        _context: &QuoteContext,
        instrument: ProviderInstrument,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Result<Vec<Quote>, MarketDataError> {
        let symbol = instrument_symbol(&instrument)?;
        // Bars are stamped 09:00 UTC on their trading date, so the range's
        // UTC dates are the trading dates it covers.
        let (first, last) = (start.date_naive(), end.date_naive());
        if first > last {
            return Err(MarketDataError::NoDataForRange);
        }
        let quotes: Vec<Quote> = self
            .daily_quotes(&symbol)
            .await?
            .into_iter()
            .filter(|(date, _)| (first..=last).contains(date))
            .map(|(_, quote)| quote)
            .collect();
        if quotes.is_empty() {
            return Err(MarketDataError::NoDataForRange);
        }
        Ok(quotes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;
    use std::sync::{Arc, Mutex};

    // Trimmed responses in the shape observed on 2026-11-09 (see the module
    // docs); only the fields the provider reads, plus a few it must ignore.

    /// `companyInfoSummery` for JKH. `reqSymbolBetaInfo.securityId` is a
    /// different ID that returns an empty chart; only `reqSymbolInfo.id` works.
    const INFO_JKH: &str = r#"{"reqSymbolInfo":{"id":297,"symbol":"JKH.N0000","name":"JOHN KEELLS HOLDINGS PLC","isin":"LK0092N00003","lastTradedPrice":18.7,"closingPrice":18.7,"previousClose":18.6,"hiTrade":18.9,"lowTrade":18.5,"tdyShareVolume":1127730,"instrumentsDate":null},"reqSymbolBetaInfo":{"securityId":508}}"#;

    /// `companyChartDataByStock` for JKH, oldest first, ending on the 6 Nov
    /// 2026 session. `t` is Colombo midnight; one row has no close and one a
    /// zero close.
    const CHART_JKH: &str = r#"{"chartData":[
        {"t":1793212200000,"p":null,"h":null,"l":null,"q":0,"o":null,"c":null,"pc":null,"s":0},
        {"t":1793644200000,"p":18.4,"h":18.6,"l":18.3,"q":905114,"o":null,"c":null,"pc":null,"s":0},
        {"t":1793730600000,"p":0,"h":0,"l":0,"q":0,"o":null,"c":null,"pc":null,"s":0},
        {"t":1793817000000,"p":18.6,"h":18.7,"l":18.4,"q":1002345,"o":null,"c":null,"pc":null,"s":0},
        {"t":1793903400000,"p":18.7,"h":18.9,"l":18.5,"q":1127730,"o":null,"c":null,"pc":null,"s":0}
    ]}"#;

    /// A security whose last trade was on 30 Oct: older dated rows, and no row
    /// for the latest session.
    const CHART_WITHOUT_LATEST_SESSION: &str = r#"{"chartData":[
        {"t":1793212200000,"p":7.1,"h":7.2,"l":7.0,"q":120,"o":null,"c":null,"pc":null,"s":0},
        {"t":1793298600000,"p":7.2,"h":7.2,"l":7.1,"q":64,"o":null,"c":null,"pc":null,"s":0}
    ]}"#;

    const EMPTY_CHART: &str = r#"{"chartData":[]}"#;

    fn utc(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text)
            .expect("valid timestamp")
            .with_timezone(&Utc)
    }

    fn date(text: &str) -> NaiveDate {
        NaiveDate::parse_from_str(text, "%Y-%m-%d").expect("valid date")
    }

    /// Long after the 6 Nov 2026 session.
    fn later() -> DateTime<Utc> {
        utc("2026-11-09T12:00:00Z")
    }

    /// 15:29:59 Colombo on 6 Nov 2026: that session closed under an hour ago.
    fn before_cutoff() -> DateTime<Utc> {
        utc("2026-11-06T09:59:59Z")
    }

    /// 15:30 Colombo on 6 Nov 2026: the session's grace period is over.
    fn at_cutoff() -> DateTime<Utc> {
        utc("2026-11-06T10:00:00Z")
    }

    fn info(id: u32, symbol: &str, name: &str, isin: &str) -> String {
        format!(
            r#"{{"reqSymbolInfo":{{"id":{id},"symbol":"{symbol}","name":"{name}","isin":"{isin}"}},"reqSymbolBetaInfo":{{"securityId":{}}}}}"#,
            id + 1
        )
    }

    // -----------------------------------------------------------------------
    // Pure helpers
    // -----------------------------------------------------------------------

    #[test]
    fn chart_time_is_colombo_midnight_of_the_trading_day() {
        // 2026-11-05T18:30Z is the 6 Nov session, not 5 Nov.
        assert_eq!(trading_date(1_793_903_400_000), Some(date("2026-11-06")));
        assert_eq!(
            session_close_utc(date("2026-11-06")),
            utc("2026-11-06T09:00:00Z")
        );
    }

    #[test]
    fn chart_rows_become_daily_quotes_at_the_session_close() {
        let body: Value = serde_json::from_str(CHART_JKH).expect("fixture");
        let quotes = chart_quotes(&body, later()).expect("chart");

        // No close and a zero close are skipped.
        assert_eq!(
            quotes.keys().copied().collect::<Vec<_>>(),
            [date("2026-11-03"), date("2026-11-05"), date("2026-11-06")]
        );
        let quote = &quotes[&date("2026-11-06")];
        assert_eq!(quote.timestamp, utc("2026-11-06T09:00:00Z"));
        assert_eq!(quote.timestamp.date_naive(), date("2026-11-06"));
        assert_eq!(quote.close, dec!(18.7));
        assert_eq!(quote.high, Some(dec!(18.9)));
        assert_eq!(quote.low, Some(dec!(18.5)));
        assert_eq!(quote.volume, Some(dec!(1127730)));
        assert_eq!(quote.open, None);
        assert_eq!(quote.currency, "LKR");
        assert_eq!(quote.source, "CSE_LK");
    }

    #[test]
    fn a_chart_without_chart_data_is_a_provider_error() {
        let body: Value = serde_json::from_str(r#"{"message":"maintenance"}"#).expect("json");
        assert!(matches!(
            chart_quotes(&body, later()),
            Err(MarketDataError::ProviderError { .. })
        ));
    }

    #[test]
    fn the_current_session_is_published_an_hour_after_the_close() {
        let today = date("2026-11-06");
        assert!(!session_published(today, utc("2026-11-06T04:00:00Z")));
        assert!(!session_published(today, before_cutoff()));
        assert!(session_published(today, at_cutoff()));
        // Earlier sessions keep their date whatever the time of day.
        assert!(session_published(
            date("2026-11-05"),
            utc("2026-11-06T04:00:00Z")
        ));
        assert_eq!(PUBLICATION_GRACE_MINUTES, 60);
    }

    #[test]
    fn symbols_are_full_cse_symbols_or_bare_tickers() {
        for (query, looked_up) in [
            ("JKH.N0000", Some("JKH.N0000")),
            (" jkh.n0000 ", Some("JKH.N0000")),
            ("COMB.X0000", Some("COMB.X0000")),
            ("SFIN.R0000", Some("SFIN.R0000")),
            ("CALC.U0000", Some("CALC.U0000")),
            ("JKH", Some("JKH.N0000")),
            ("comb", Some("COMB.N0000")),
            ("AB", Some("AB.N0000")),
            ("ABCDEF", Some("ABCDEF.N0000")),
            ("A", None),
            ("ABCDEFG", None),
            ("John Keells", None),
            ("JKH.N", None),
            ("JKH.N000", None),
            ("JKH.00000", None),
            ("JKH-N0000", None),
            ("JKH.N0000.X", None),
            ("", None),
        ] {
            assert_eq!(looked_up_symbol(query).as_deref(), looked_up, "{query}");
        }
        // Fetches take only a full symbol.
        assert_eq!(full_symbol("JKH"), None);
    }

    #[test]
    fn stock_id_comes_from_req_symbol_info() {
        let body: Value = serde_json::from_str(INFO_JKH).expect("fixture");
        let info = security_info(&body, "JKH.N0000").expect("info");
        assert_eq!(info.stock_id, "297");
        assert_eq!(info.symbol, "JKH.N0000");
        assert_eq!(info.name.as_deref(), Some("JOHN KEELLS HOLDINGS PLC"));
        assert_eq!(info.isin.as_deref(), Some("LK0092N00003"));

        let string_id: Value =
            serde_json::from_str(r#"{"reqSymbolInfo":{"id":"297","symbol":"JKH.N0000"}}"#)
                .expect("json");
        assert_eq!(
            security_info(&string_id, "JKH.N0000").map(|info| info.stock_id),
            Some("297".to_string())
        );
    }

    #[test]
    fn a_response_for_another_class_is_not_a_match() {
        let ordinary: Value = serde_json::from_str(INFO_JKH).expect("fixture");
        assert_eq!(security_info(&ordinary, "JKH.X0000"), None);
        let without_id: Value =
            serde_json::from_str(r#"{"reqSymbolInfo":{"symbol":"JKH.N0000"}}"#).expect("json");
        assert_eq!(security_info(&without_id, "JKH.N0000"), None);
        let empty: Value = serde_json::from_str("{}").expect("json");
        assert_eq!(security_info(&empty, "JKH.N0000"), None);
    }

    #[test]
    fn fund_units_are_funds() {
        assert_eq!(quote_type("CALC.U0000"), "MUTUALFUND");
        assert_eq!(quote_type("JKH.N0000"), "EQUITY");
        assert_eq!(quote_type("COMB.X0000"), "EQUITY");
        assert_eq!(quote_type("SFIN.R0000"), "EQUITY");
    }

    // -----------------------------------------------------------------------
    // Requests, against a local server
    // -----------------------------------------------------------------------

    /// A recorded request: the endpoint, the form body, and the raw head.
    #[derive(Clone, Debug)]
    struct Request {
        endpoint: String,
        body: String,
        head: String,
    }

    type Requests = Arc<Mutex<Vec<Request>>>;

    /// `None` holds the connection open without answering.
    type Reply = Option<(u16, String)>;

    /// A provider whose cse.lk is a local server. The server answers each
    /// request's endpoint and form body with `respond` and records it. Its
    /// limiter never makes a test wait; metering tests configure their own.
    async fn local_provider(
        respond: impl Fn(&str, &str) -> Reply + Send + Sync + 'static,
    ) -> (CseLkProvider, Requests) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a local port");
        let base = format!(
            "http://{}/api",
            listener.local_addr().expect("local address")
        );
        let requests = Requests::default();
        let log = Arc::clone(&requests);
        let respond = Arc::new(respond);
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let log = Arc::clone(&log);
                let respond = Arc::clone(&respond);
                tokio::spawn(async move {
                    let mut data = Vec::new();
                    let mut buffer = [0; 1024];
                    let head_end = loop {
                        if let Some(end) = data.windows(4).position(|w| w == b"\r\n\r\n") {
                            break end + 4;
                        }
                        match socket.read(&mut buffer).await {
                            Ok(0) | Err(_) => return,
                            Ok(read) => data.extend_from_slice(&buffer[..read]),
                        }
                    };
                    let head = String::from_utf8_lossy(&data[..head_end]).to_string();
                    let length = head
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().ok())?
                        })
                        .unwrap_or(0);
                    while data.len() < head_end + length {
                        match socket.read(&mut buffer).await {
                            Ok(0) | Err(_) => break,
                            Ok(read) => data.extend_from_slice(&buffer[..read]),
                        }
                    }
                    let body = String::from_utf8_lossy(&data[head_end..]).to_string();
                    let target = head.split_whitespace().nth(1).unwrap_or_default();
                    let endpoint = target.trim_start_matches("/api/").to_string();
                    let reply = respond(&endpoint, &body);
                    log.lock().expect("request log").push(Request {
                        endpoint,
                        body,
                        head: head.clone(),
                    });
                    let Some((status, reply)) = reply else {
                        tokio::time::sleep(Duration::from_secs(5)).await;
                        return;
                    };
                    let response = format!(
                        "HTTP/1.1 {status} Canned\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                        reply.len()
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                });
            }
        });

        let mut provider = CseLkProvider::with_base_url(Cow::Owned(base));
        provider.client = wealthfolio_http::client_builder()
            .no_proxy()
            .user_agent(USER_AGENT)
            .build()
            .expect("local client");
        provider.request_limiter.configure(
            &Cow::Borrowed(PROVIDER_ID),
            RateLimitConfig {
                requests_per_minute: 60_000,
                burst_capacity: 1_000.0,
            },
        );
        provider.clock = later;
        (provider, requests)
    }

    /// A local cse.lk that knows JKH and answers its chart with `chart`.
    async fn jkh_provider(chart: &'static str) -> (CseLkProvider, Requests) {
        local_provider(move |endpoint, body| match (endpoint, body) {
            ("companyInfoSummery", "symbol=JKH.N0000") => Some((200, INFO_JKH.to_string())),
            ("companyChartDataByStock", "stockId=297&period=5") => Some((200, chart.to_string())),
            _ => Some((404, "{}".to_string())),
        })
        .await
    }

    /// Make the limiter refill too slowly to matter during a test, so the
    /// tokens an operation used can be read off it.
    fn slow_refill(provider: &CseLkProvider) {
        provider.request_limiter.configure(
            &Cow::Borrowed(PROVIDER_ID),
            RateLimitConfig {
                requests_per_minute: 1,
                burst_capacity: 20.0,
            },
        );
        // Create the bucket, so the count read before the first request is
        // its own rather than the limiter's default.
        assert!(provider
            .request_limiter
            .try_acquire(&Cow::Borrowed(PROVIDER_ID)));
    }

    fn tokens(provider: &CseLkProvider) -> f64 {
        provider
            .request_limiter
            .remaining_tokens(&Cow::Borrowed(PROVIDER_ID))
    }

    fn requests(log: &Requests) -> Vec<Request> {
        log.lock().expect("request log").clone()
    }

    fn equity(symbol: &str) -> ProviderInstrument {
        ProviderInstrument::EquitySymbol {
            symbol: symbol.into(),
        }
    }

    fn context(symbol: &str) -> QuoteContext {
        QuoteContext {
            instrument: crate::models::InstrumentId::Equity {
                ticker: symbol.into(),
                mic: Some(Cow::Borrowed(MIC)),
            },
            identifiers: crate::models::QuoteIdentifiers::default(),
            overrides: None,
            currency_hint: Some("LKR".into()),
            preferred_provider: Some(PROVIDER_ID.into()),
            bond_metadata: None,
            custom_provider_code: None,
        }
    }

    async fn history(
        provider: &CseLkProvider,
        symbol: &str,
        first: &str,
        last: &str,
    ) -> Result<Vec<Quote>, MarketDataError> {
        provider
            .get_historical_quotes(
                &context(symbol),
                equity(symbol),
                utc(&format!("{first}T00:00:00Z")),
                utc(&format!("{last}T23:59:59Z")),
            )
            .await
    }

    async fn latest(provider: &CseLkProvider, symbol: &str) -> Result<Quote, MarketDataError> {
        provider
            .get_latest_quote(&context(symbol), equity(symbol))
            .await
    }

    fn dates(quotes: &[Quote]) -> Vec<NaiveDate> {
        quotes
            .iter()
            .map(|quote| quote.timestamp.date_naive())
            .collect()
    }

    #[tokio::test]
    async fn history_looks_up_the_stock_id_then_fetches_the_years_chart() {
        let (provider, log) = jkh_provider(CHART_JKH).await;

        let quotes = history(&provider, "JKH.N0000", "2026-11-04", "2026-11-30")
            .await
            .expect("history");

        // Oldest first, only dates in range, and t = 1793903400000 is 6 Nov.
        assert_eq!(dates(&quotes), [date("2026-11-05"), date("2026-11-06")]);
        assert_eq!(quotes[1].close, dec!(18.7));
        let requests = requests(&log);
        assert_eq!(
            requests
                .iter()
                .map(|r| (r.endpoint.as_str(), r.body.as_str()))
                .collect::<Vec<_>>(),
            [
                ("companyInfoSummery", "symbol=JKH.N0000"),
                // reqSymbolInfo.id, not reqSymbolBetaInfo.securityId (508),
                // and the year-long period whatever the range.
                ("companyChartDataByStock", "stockId=297&period=5"),
            ]
        );
        for request in &requests {
            let head = request.head.to_ascii_lowercase();
            assert!(head.starts_with("post "), "{head}");
            assert!(head.contains("user-agent: wealthfolio\r\n"), "{head}");
            assert!(
                head.contains("content-type: application/x-www-form-urlencoded"),
                "{head}"
            );
        }
    }

    #[tokio::test]
    async fn history_outside_the_feed_or_range_is_no_data() {
        let (provider, log) = jkh_provider(CHART_JKH).await;

        // A holding older than the feed gets what the feed has.
        let all = history(&provider, "JKH.N0000", "2020-01-01", "2026-11-09")
            .await
            .expect("history");
        assert_eq!(
            dates(&all),
            [date("2026-11-03"), date("2026-11-05"), date("2026-11-06")]
        );
        // A later sync of the range before the feed is a neutral skip.
        assert!(matches!(
            history(&provider, "JKH.N0000", "2020-01-01", "2025-11-09").await,
            Err(MarketDataError::NoDataForRange)
        ));
        let before = requests(&log).len();
        // An inverted range asks for nothing.
        assert!(matches!(
            history(&provider, "JKH.N0000", "2026-11-09", "2026-11-01").await,
            Err(MarketDataError::NoDataForRange)
        ));
        assert_eq!(requests(&log).len(), before);
    }

    #[tokio::test]
    async fn each_request_takes_a_token_from_the_providers_limiter() {
        let (provider, log) = jkh_provider(CHART_JKH).await;
        slow_refill(&provider);

        let cases: [(&str, usize); 4] =
            [("history", 2), ("latest", 2), ("profile", 1), ("lookup", 1)];
        for (operation, expected) in cases {
            let before = (tokens(&provider), requests(&log).len());
            match operation {
                "history" => {
                    history(&provider, "JKH.N0000", "2026-11-01", "2026-11-09")
                        .await
                        .expect("history");
                }
                "latest" => {
                    latest(&provider, "JKH.N0000").await.expect("latest");
                }
                "profile" => {
                    provider.get_profile("JKH.N0000").await.expect("profile");
                }
                _ => {
                    assert_eq!(provider.search("JKH").await.expect("lookup").len(), 1);
                }
            }
            let sent = requests(&log).len() - before.1;
            let taken = (before.0 - tokens(&provider)).round() as usize;
            assert_eq!(sent, expected, "{operation} requests");
            assert_eq!(taken, expected, "{operation} tokens");
        }
    }

    #[tokio::test]
    async fn a_request_beyond_the_burst_waits_for_a_token() {
        let (provider, log) = jkh_provider(CHART_JKH).await;
        provider
            .request_limiter
            .configure(&Cow::Borrowed(PROVIDER_ID), request_limit());

        // The production bucket holds two tokens; history spends both.
        history(&provider, "JKH.N0000", "2026-11-01", "2026-11-09")
            .await
            .expect("history");
        assert!(tokens(&provider) < 1.0);

        // At 30 a minute the next token is about two seconds away.
        let profile = tokio::time::timeout(
            Duration::from_millis(300),
            provider.get_profile("JKH.N0000"),
        )
        .await;
        assert!(profile.is_err(), "the third request waited for a token");
        assert_eq!(requests(&log).len(), 2);
    }

    #[tokio::test]
    async fn the_current_session_waits_for_the_close_and_grace_period() {
        let (mut provider, _) = jkh_provider(CHART_JKH).await;

        provider.clock = before_cutoff;
        let quotes = history(&provider, "JKH.N0000", "2026-11-01", "2026-11-06")
            .await
            .expect("history");
        assert_eq!(dates(&quotes), [date("2026-11-03"), date("2026-11-05")]);
        let newest = latest(&provider, "JKH.N0000").await.expect("latest");
        assert_eq!(newest.timestamp, utc("2026-11-05T09:00:00Z"));
        assert_eq!(newest.close, dec!(18.6));

        provider.clock = at_cutoff;
        let quotes = history(&provider, "JKH.N0000", "2026-11-01", "2026-11-06")
            .await
            .expect("history");
        assert_eq!(dates(&quotes).last(), Some(&date("2026-11-06")));
        let newest = latest(&provider, "JKH.N0000").await.expect("latest");
        assert_eq!(newest.timestamp, utc("2026-11-06T09:00:00Z"));
    }

    #[tokio::test]
    async fn other_share_classes_keep_their_own_symbol_id_and_isin() {
        let (provider, log) = local_provider(|endpoint, body| {
            let reply = match (endpoint, body) {
                ("companyInfoSummery", "symbol=COMB.X0000") => info(
                    396,
                    "COMB.X0000",
                    "COMMERCIAL BANK OF CEYLON PLC",
                    "LK0053X00005",
                ),
                ("companyInfoSummery", "symbol=SFIN.R0000") => info(
                    2190,
                    "SFIN.R0000",
                    "SINGER FINANCE (LANKA) PLC",
                    "LK0410R00003",
                ),
                ("companyInfoSummery", "symbol=CALC.U0000") => info(
                    6297,
                    "CALC.U0000",
                    "CAL FIVE YEAR CLOSED END FUND",
                    "LK0614U00002",
                ),
                // The ordinary share exists, and must never stand in.
                ("companyInfoSummery", "symbol=COMB.N0000") => info(
                    395,
                    "COMB.N0000",
                    "COMMERCIAL BANK OF CEYLON PLC",
                    "LK0053N00005",
                ),
                ("companyChartDataByStock", "stockId=396&period=5")
                | ("companyChartDataByStock", "stockId=2190&period=5")
                | ("companyChartDataByStock", "stockId=6297&period=5") => CHART_JKH.to_string(),
                _ => return Some((404, "{}".to_string())),
            };
            Some((200, reply))
        })
        .await;

        for (symbol, stock_id, isin, quote_type) in [
            ("COMB.X0000", "396", "LK0053X00005", "EQUITY"),
            ("SFIN.R0000", "2190", "LK0410R00003", "EQUITY"),
            ("CALC.U0000", "6297", "LK0614U00002", "MUTUALFUND"),
        ] {
            let results = provider.search(symbol).await.expect("lookup");
            assert_eq!(results.len(), 1, "{symbol}");
            assert_eq!(results[0].symbol, symbol);
            assert_eq!(results[0].asset_type, quote_type, "{symbol}");
            assert_eq!(results[0].exchange_mic.as_deref(), Some("XCOL"));
            assert_eq!(results[0].currency.as_deref(), Some("LKR"));
            assert_eq!(results[0].data_source.as_deref(), Some("CSE_LK"));

            let profile = provider.get_profile(symbol).await.expect("profile");
            assert_eq!(profile.isin.as_deref(), Some(isin), "{symbol}");
            assert_eq!(profile.quote_type.as_deref(), Some(quote_type), "{symbol}");
            assert_eq!(profile.currency.as_deref(), Some("LKR"));

            let quotes = history(&provider, symbol, "2026-11-01", "2026-11-09")
                .await
                .expect("history");
            assert!(!quotes.is_empty(), "{symbol}");
            assert!(requests(&log)
                .iter()
                .any(|r| r.body == format!("stockId={stock_id}&period=5")));
        }
        assert!(
            requests(&log).iter().all(|r| !r.body.contains("N0000")),
            "no ordinary share was requested in place of another class"
        );
    }

    #[tokio::test]
    async fn a_bare_ticker_is_looked_up_as_its_ordinary_share() {
        let (provider, log) = jkh_provider(CHART_JKH).await;

        assert!(provider.looks_up_identifier("jkh"));
        let results = provider.search("jkh").await.expect("lookup");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].symbol, "JKH.N0000");
        assert_eq!(results[0].name, "JOHN KEELLS HOLDINGS PLC");
        // An unknown ticker finds nothing, without an error.
        assert!(provider.search("NOPE").await.expect("lookup").is_empty());
        assert_eq!(
            requests(&log)
                .iter()
                .map(|r| r.body.as_str())
                .collect::<Vec<_>>(),
            ["symbol=JKH.N0000", "symbol=NOPE.N0000"]
        );
    }

    #[tokio::test]
    async fn queries_that_are_not_symbols_send_nothing() {
        let (provider, log) = jkh_provider(CHART_JKH).await;

        for query in ["John Keells", "J", "TOOLONG", "JKH.N", "JKH-N0000", ""] {
            assert!(!provider.looks_up_identifier(query), "{query}");
            assert!(provider.search(query).await.expect("lookup").is_empty());
        }
        // Prices never guess a class for a bare ticker.
        assert!(matches!(
            history(&provider, "JKH", "2026-11-01", "2026-11-09").await,
            Err(MarketDataError::SymbolNotFound(_))
        ));
        assert!(matches!(
            latest(&provider, "JKH").await,
            Err(MarketDataError::SymbolNotFound(_))
        ));
        assert!(matches!(
            provider.get_profile("JKH").await,
            Err(MarketDataError::SymbolNotFound(_))
        ));
        assert!(requests(&log).is_empty());
    }

    #[tokio::test]
    async fn an_empty_chart_is_no_data_and_fabricates_nothing() {
        let (provider, _) = jkh_provider(EMPTY_CHART).await;

        // The profile lookup succeeds, but there is nothing to price.
        assert!(provider.get_profile("JKH.N0000").await.is_ok());
        assert!(matches!(
            history(&provider, "JKH.N0000", "2026-11-01", "2026-11-09").await,
            Err(MarketDataError::NoDataForRange)
        ));
        assert!(matches!(
            latest(&provider, "JKH.N0000").await,
            Err(MarketDataError::NoDataForRange)
        ));
    }

    #[tokio::test]
    async fn older_rows_without_the_latest_session_keep_their_dates() {
        let (provider, _) = jkh_provider(CHART_WITHOUT_LATEST_SESSION).await;

        // A sync of the recent range stores nothing new.
        assert!(matches!(
            history(&provider, "JKH.N0000", "2026-11-01", "2026-11-09").await,
            Err(MarketDataError::NoDataForRange)
        ));
        // The latest quote is the 30 Oct close, dated 30 Oct, not today.
        let newest = latest(&provider, "JKH.N0000").await.expect("latest");
        assert_eq!(newest.timestamp, utc("2026-10-30T09:00:00Z"));
        assert_eq!(newest.close, dec!(7.2));
        let older = history(&provider, "JKH.N0000", "2026-10-01", "2026-11-09")
            .await
            .expect("history");
        assert_eq!(dates(&older), [date("2026-10-29"), date("2026-10-30")]);
    }

    #[tokio::test]
    async fn upstream_failures_map_to_their_errors() {
        for (status, body, check) in [
            (
                404,
                "{}",
                (|e| matches!(e, MarketDataError::SymbolNotFound(_)))
                    as fn(&MarketDataError) -> bool,
            ),
            (429, "", |e| {
                matches!(e, MarketDataError::RateLimited { .. })
            }),
            (500, "oops", |e| {
                matches!(e, MarketDataError::ProviderError { .. })
            }),
            (200, "<html>not json", |e| {
                matches!(e, MarketDataError::ProviderError { .. })
            }),
        ] {
            let (provider, _) = local_provider(move |_, _| Some((status, body.to_string()))).await;
            let error = history(&provider, "JKH.N0000", "2026-11-01", "2026-11-09")
                .await
                .expect_err("failure");
            assert!(check(&error), "{status}: {error:?}");
            let error = provider
                .get_profile("JKH.N0000")
                .await
                .expect_err("failure");
            assert!(check(&error), "{status}: {error:?}");
        }

        // A malformed chart after a good lookup.
        let (provider, _) = local_provider(|endpoint, _| match endpoint {
            "companyInfoSummery" => Some((200, INFO_JKH.to_string())),
            _ => Some((200, "{\"chartData\":".to_string())),
        })
        .await;
        assert!(matches!(
            history(&provider, "JKH.N0000", "2026-11-01", "2026-11-09").await,
            Err(MarketDataError::ProviderError { .. })
        ));
    }

    #[tokio::test]
    async fn a_lookup_that_fails_upstream_is_an_error_not_an_empty_result() {
        let (provider, _) = local_provider(|_, _| Some((503, String::new()))).await;
        assert!(matches!(
            provider.search("JKH.N0000").await,
            Err(MarketDataError::ProviderError { .. })
        ));
    }

    #[tokio::test]
    async fn a_slow_response_is_a_timeout() {
        let (mut provider, _) = local_provider(|_, _| None).await;
        provider.client = wealthfolio_http::client_builder()
            .no_proxy()
            .timeout(Duration::from_millis(200))
            .build()
            .expect("local client");

        assert!(matches!(
            history(&provider, "JKH.N0000", "2026-11-01", "2026-11-09").await,
            Err(MarketDataError::Timeout { .. })
        ));
    }

    #[test]
    fn capabilities_cover_xcol_equities_without_search_or_dividends() {
        let provider = CseLkProvider::new();
        let capabilities = provider.capabilities();
        assert_eq!(provider.id(), "CSE_LK");
        assert_eq!(capabilities.instrument_kinds, &[InstrumentKind::Equity]);
        assert!(capabilities.supports_latest);
        assert!(capabilities.supports_historical);
        assert!(capabilities.supports_profile);
        assert!(!capabilities.supports_search);
        assert!(!capabilities.supports_dividends);
        assert!(
            capabilities.supports_instrument(&crate::models::InstrumentId::Equity {
                ticker: "JKH.N0000".into(),
                mic: Some(Cow::Borrowed("XCOL")),
            })
        );
        assert!(
            !capabilities.supports_instrument(&crate::models::InstrumentId::Equity {
                ticker: "JKH.N0000".into(),
                mic: None,
            })
        );
        let rate_limit = provider.rate_limit();
        assert_eq!(rate_limit.requests_per_minute, 30);
        assert_eq!(rate_limit.max_concurrency, 2);

        // The provider's own bucket lets two requests go at once.
        let id: ProviderId = Cow::Borrowed(PROVIDER_ID);
        assert!(provider.request_limiter.try_acquire(&id));
        assert!(provider.request_limiter.try_acquire(&id));
        assert!(!provider.request_limiter.try_acquire(&id));
    }
}
