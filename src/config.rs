/// Credential and configuration management using OS-native keyring.
///
/// Stores Spotify and Last.fm API keys securely in the system's
/// credential manager (macOS Keychain, Windows Credential Manager,
/// or Linux Secret Service).

use anyhow::{Context, Result};
use std::io::{self, Write};

use serde::{Deserialize, Serialize};

const SERVICE_NAME: &str = "onyx";

/// Key we store in the system keyring.
const KEY_CONFIG: &str = "api_keys";

/// Holds the three API keys required by Onyx.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub spotify_client_id: String,
    pub spotify_client_secret: String,
    pub lastfm_api_key: String,
}

// ---------------------------------------------------------------------------
// Keyring helpers
// ---------------------------------------------------------------------------

/// Read the config from the OS keyring. Returns `None` if not found.
fn keyring_get() -> Option<String> {
    let entry = keyring::Entry::new(SERVICE_NAME, KEY_CONFIG).ok()?;
    entry.get_password().ok()
}

/// Write the config to the OS keyring.
fn keyring_set(value: &str) -> Result<()> {
    let entry = keyring::Entry::new(SERVICE_NAME, KEY_CONFIG)
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
    /// Attempt to load the keys from the OS keyring.
    pub fn load() -> Option<Self> {
        let json_str = keyring_get()?;
        serde_json::from_str(&json_str).ok()
    }

    /// Persist all keys to the OS keyring as a single JSON blob.
    pub fn save(&self) -> Result<()> {
        let json_str = serde_json::to_string(self)?;
        keyring_set(&json_str)?;
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
