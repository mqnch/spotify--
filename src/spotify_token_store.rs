//! Persist rspotify user tokens in the OS keyring (same service as API keys).
//!
//! Replaces the previous `.spotify_token_cache.json` file cache: tokens survive
//! app moves and are not dropped when the working directory changes.

use anyhow::{Context, Result};
use rspotify::Token;
use std::path::Path;

/// Same service name as [`crate::config`] API key storage.
const SERVICE_NAME: &str = "onyx";

/// Keyring account label for the Spotify OAuth user token (JSON blob).
const KEY_SPOTIFY_USER_TOKEN: &str = "spotify_user_token";

/// Legacy on-disk cache (cwd-relative); migrated once into the keyring if empty.
pub const LEGACY_TOKEN_CACHE_PATH: &str = ".spotify_token_cache.json";

fn entry() -> Result<keyring::Entry> {
    keyring::Entry::new(SERVICE_NAME, KEY_SPOTIFY_USER_TOKEN)
        .context("Failed to create keyring entry for Spotify token")
}

/// Read the stored token JSON, if any.
pub fn load_token() -> Option<Token> {
    let json = entry().ok()?.get_password().ok()?;
    serde_json::from_str(&json).ok()
}

/// Serialize and store the token (access + refresh + expiry).
pub fn save_token(token: &Token) -> Result<()> {
    let json = serde_json::to_string(token).context("Failed to serialize Spotify token")?;
    entry()?.set_password(&json).context("Failed to write Spotify token to keyring")?;
    Ok(())
}

/// Remove stored credentials (e.g. revoked refresh token).
pub fn clear_token() -> Result<()> {
    match entry()?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(e).context("Failed to delete Spotify token from keyring"),
    }
}

/// If the keyring has no token but a legacy cache file exists, import it once.
pub fn migrate_legacy_file_if_needed() -> Result<()> {
    if load_token().is_some() {
        return Ok(());
    }
    let path = Path::new(LEGACY_TOKEN_CACHE_PATH);
    if !path.exists() {
        return Ok(());
    }
    let token = Token::from_cache(path).map_err(|e| anyhow::anyhow!("{}", e))?;
    save_token(&token)?;
    log::info!(
        "Migrated Spotify token from {} into the system keyring.",
        LEGACY_TOKEN_CACHE_PATH
    );
    Ok(())
}
