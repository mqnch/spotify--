/// Credential and configuration management using OS-native keyring.
///
/// Stores Spotify and Last.fm API keys securely in the system's
/// credential manager (macOS Keychain, Windows Credential Manager,
/// or Linux Secret Service).

use anyhow::{Context, Result};
use std::io::{self, Write};

const SERVICE_NAME: &str = "onyx";

/// Keys we store in the system keyring.
const KEY_SPOTIFY_CLIENT_ID: &str = "spotify_client_id";
const KEY_SPOTIFY_CLIENT_SECRET: &str = "spotify_client_secret";
const KEY_LASTFM_API_KEY: &str = "lastfm_api_key";

/// Holds the three API keys required by Onyx.
#[derive(Debug, Clone)]
pub struct AppConfig {
    pub spotify_client_id: String,
    pub spotify_client_secret: String,
    pub lastfm_api_key: String,
}

// ---------------------------------------------------------------------------
// Keyring helpers
// ---------------------------------------------------------------------------

/// Read a single key from the OS keyring. Returns `None` if not found.
fn keyring_get(key: &str) -> Option<String> {
    let entry = keyring::Entry::new(SERVICE_NAME, key).ok()?;
    entry.get_password().ok()
}

/// Write a single key to the OS keyring.
fn keyring_set(key: &str, value: &str) -> Result<()> {
    let entry = keyring::Entry::new(SERVICE_NAME, key)
        .context("Failed to create keyring entry")?;
    entry
        .set_password(value)
        .context("Failed to store credential in keyring")?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

impl AppConfig {
    /// Attempt to load all three keys from the OS keyring.
    /// Returns `None` if any key is missing.
    pub fn load() -> Option<Self> {
        let spotify_client_id = keyring_get(KEY_SPOTIFY_CLIENT_ID)?;
        let spotify_client_secret = keyring_get(KEY_SPOTIFY_CLIENT_SECRET)?;
        let lastfm_api_key = keyring_get(KEY_LASTFM_API_KEY)?;

        Some(Self {
            spotify_client_id,
            spotify_client_secret,
            lastfm_api_key,
        })
    }

    /// Persist all keys to the OS keyring.
    pub fn save(&self) -> Result<()> {
        keyring_set(KEY_SPOTIFY_CLIENT_ID, &self.spotify_client_id)?;
        keyring_set(KEY_SPOTIFY_CLIENT_SECRET, &self.spotify_client_secret)?;
        keyring_set(KEY_LASTFM_API_KEY, &self.lastfm_api_key)?;
        Ok(())
    }

    /// Interactively prompt the user for each key, then save to keyring.
    pub fn prompt_and_save() -> Result<Self> {
        println!();
        println!("╔══════════════════════════════════════════════╗");
        println!("║       Onyx — First-Run Configuration        ║");
        println!("╠══════════════════════════════════════════════╣");
        println!("║  You'll need:                               ║");
        println!("║  • Spotify Client ID & Secret               ║");
        println!("║    (developer.spotify.com/dashboard)        ║");
        println!("║  • Last.fm API Key                          ║");
        println!("║    (last.fm/api/account/create)             ║");
        println!("╚══════════════════════════════════════════════╝");
        println!();

        let spotify_client_id = prompt("Spotify Client ID")?;
        let spotify_client_secret = prompt("Spotify Client Secret")?;
        let lastfm_api_key = prompt("Last.fm API Key")?;

        let config = Self {
            spotify_client_id,
            spotify_client_secret,
            lastfm_api_key,
        };

        config.save()?;
        println!("\n✓ Credentials saved to system keyring.");

        Ok(config)
    }

    /// Load from keyring if available, otherwise prompt interactively.
    pub fn ensure_configured() -> Result<Self> {
        if let Some(config) = Self::load() {
            println!("✓ Credentials loaded from system keyring.");
            return Ok(config);
        }

        Self::prompt_and_save()
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Print a prompt and read a non-empty trimmed line from stdin.
fn prompt(label: &str) -> Result<String> {
    loop {
        print!("  → {}: ", label);
        io::stdout().flush()?;

        let mut buf = String::new();
        io::stdin().read_line(&mut buf)?;
        let value = buf.trim().to_string();

        if value.is_empty() {
            println!("    ✗ Value cannot be empty. Try again.");
            continue;
        }

        return Ok(value);
    }
}
