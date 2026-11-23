-- Colombo Stock Exchange (XCOL) daily prices from the public feed on cse.lk.
-- The quote client only builds providers that have a row here. Disabled until
-- the user turns it on, so no request reaches cse.lk before then.
INSERT OR IGNORE INTO market_data_providers (id, name, description, url, priority, enabled, logo_filename, last_synced_at, last_sync_status, last_sync_error)
VALUES
    ('CSE_LK', 'Colombo Stock Exchange', 'Daily prices for securities on the Colombo Stock Exchange, from the public feed on cse.lk. Unofficial source. No API key required.', 'https://www.cse.lk/', 16, FALSE, 'cse-lk.svg', NULL, NULL, NULL);
