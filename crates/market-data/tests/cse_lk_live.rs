//! Opt-in upstream smoke test for the undocumented cse.lk feed; never reads
//! portfolio data or credentials, and prints no prices.
#![allow(clippy::unwrap_used, clippy::panic, reason = "test code")]
use chrono::{Duration, Utc};
use rust_decimal::Decimal;
use wealthfolio_market_data::{
    CseLkProvider, InstrumentId, MarketDataProvider, ProviderInstrument, QuoteContext,
    QuoteIdentifiers,
};

fn context(symbol: &str) -> QuoteContext {
    QuoteContext {
        instrument: InstrumentId::Equity {
            ticker: symbol.into(),
            mic: Some("XCOL".into()),
        },
        identifiers: QuoteIdentifiers::default(),
        overrides: None,
        currency_hint: Some("LKR".into()),
        preferred_provider: Some("CSE_LK".into()),
        bond_metadata: None,
        custom_provider_code: None,
    }
}

#[tokio::test]
#[ignore = "requires live cse.lk HTTPS access; run explicitly to check the feed's format"]
async fn cse_lk_lookup_profile_and_a_year_of_history() {
    tokio::time::timeout(std::time::Duration::from_secs(120), async {
        let provider = CseLkProvider::new();
        let symbol = "JKH.N0000";

        // A bare ticker is looked up as its ordinary share.
        let results = provider.search("JKH").await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].symbol, symbol);
        assert_eq!(results[0].exchange_mic.as_deref(), Some("XCOL"));

        let profile = provider.get_profile(symbol).await.unwrap();
        assert!(profile.name.is_some());
        assert!(profile
            .isin
            .as_deref()
            .is_some_and(|isin| isin.starts_with("LK")));
        assert_eq!(profile.currency.as_deref(), Some("LKR"));

        let end = Utc::now();
        let history = provider
            .get_historical_quotes(
                &context(symbol),
                ProviderInstrument::EquitySymbol {
                    symbol: symbol.into(),
                },
                end - Duration::days(400),
                end,
            )
            .await
            .unwrap();
        // About a year of sessions, oldest first, recent and in LKR.
        assert!(history.len() > 150, "only {} quotes", history.len());
        assert!(history
            .windows(2)
            .all(|pair| pair[0].timestamp.date_naive() < pair[1].timestamp.date_naive()));
        assert!(history.last().unwrap().timestamp > end - Duration::days(10));
        assert!(history
            .iter()
            .all(|quote| quote.close > Decimal::ZERO && quote.currency == "LKR"));
    })
    .await
    .unwrap();
}
