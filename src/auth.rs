/// Spotify OAuth2 Authorization Code flow.
///
/// Launches the user's browser to the Spotify authorization page,
/// spins up a local HTTP server to catch the callback, exchanges
/// the authorization code for access + refresh tokens, and returns
/// an authenticated `AuthCodeSpotify` client.
///
/// User tokens are persisted in the OS keyring (see `spotify_token_store`).
/// rspotify's `auth_headers` path already calls `auto_reauth` when
/// `token_refreshing` is true, so Web API calls refresh the access token
/// without wrapping each endpoint.

use anyhow::{anyhow, Context, Result};
use rspotify::{
    clients::OAuthClient,
    prelude::*,
    scopes, AuthCodeSpotify, CallbackError, Config, Credentials, OAuth, Token, TokenCallback,
};
use std::collections::HashMap;
use std::sync::Arc;
use tiny_http::{Response, Server};
use url::Url;

use crate::config::AppConfig;

/// The redirect URI — must exactly match what's registered in the
/// Spotify Developer Dashboard.  Spotify no longer allows `localhost`;
/// a loopback IP literal is required.
const REDIRECT_URI: &str = "http://127.0.0.1:8888/callback";

fn persist_token_callback() -> TokenCallback {
    TokenCallback(Box::new(|token: Token| {
        crate::spotify_token_store::save_token(&token).map_err(|e| {
            CallbackError::CustomizedError(format!("Failed to persist Spotify token: {e}"))
        })
    }))
}

/// Build an `AuthCodeSpotify` client with keyring-backed persistence (no disk cache file).
pub fn create_spotify_client(config: &AppConfig) -> AuthCodeSpotify {
    let creds = Credentials::new(
        &config.spotify_client_id,
        &config.spotify_client_secret,
    );

    let oauth = OAuth {
        redirect_uri: REDIRECT_URI.to_string(),
        scopes: scopes!(
            "user-read-playback-state",
            "user-modify-playback-state",
            "user-read-currently-playing",
            "user-library-read",
            "user-library-modify",
            "streaming",
            "playlist-read-private",
            "playlist-read-collaborative"
        ),
        ..Default::default()
    };

    let rspotify_config = Config {
        token_cached: false,
        token_refreshing: true,
        token_callback_fn: Arc::new(Some(persist_token_callback())),
        ..Default::default()
    };

    AuthCodeSpotify::with_config(creds, oauth, rspotify_config)
}

fn refresh_failed_hard(err: &rspotify::ClientError) -> bool {
    matches!(err, rspotify::ClientError::InvalidToken)
        || err.to_string().to_ascii_lowercase().contains("invalid_grant")
}

/// Load token from keyring (including one-time migration from the legacy JSON file),
/// refresh if expired, and verify with the Web API.
///
/// On revoked / invalid refresh token, clears the keyring entry and returns `Ok(false)`.
pub async fn try_restore_from_keyring(spotify: &AuthCodeSpotify) -> Result<bool> {
    crate::spotify_token_store::migrate_legacy_file_if_needed()?;

    let Some(token) = crate::spotify_token_store::load_token() else {
        return Ok(false);
    };

    *spotify.token.lock().await.unwrap() = Some(token);

    if spotify
        .token
        .lock()
        .await
        .unwrap()
        .as_ref()
        .is_some_and(|t| t.is_expired())
    {
        match spotify.refresh_token().await {
            Ok(()) => {}
            Err(e) => {
                log::warn!("Spotify token refresh failed on restore: {}", e);
                if refresh_failed_hard(&e) {
                    let _ = crate::spotify_token_store::clear_token();
                }
                return Ok(false);
            }
        }
    }

    match spotify.current_user().await {
        Ok(_) => Ok(true),
        Err(e) => {
            log::warn!("Spotify verify (current_user) failed: {}", e);
            match spotify.refresh_token().await {
                Ok(()) => match spotify.current_user().await {
                    Ok(_) => Ok(true),
                    Err(e2) => {
                        log::warn!("Spotify verify after refresh failed: {}", e2);
                        Ok(false)
                    }
                },
                Err(re) => {
                    log::warn!("Spotify refresh after failed verify: {}", re);
                    if refresh_failed_hard(&re) {
                        let _ = crate::spotify_token_store::clear_token();
                    }
                    Ok(false)
                }
            }
        }
    }
}

/// Browser OAuth only (used for first login and in-app reconnect). Persists via `token_callback_fn`.
pub async fn authenticate_interactive(spotify: &AuthCodeSpotify) -> Result<()> {
    let cache_only = crate::spotify_api::cache_only_mode();
    if cache_only {
        return Err(anyhow!(
            "ONYX_CACHE_ONLY=1 does not allow interactive Spotify login"
        ));
    }

    let auth_url = spotify.get_authorize_url(false)?;

    println!();
    println!("  Opening Spotify login in your browser…");
    println!("  (If nothing opens, visit this URL manually:)");
    println!("  {}", auth_url);
    println!();

    if let Err(e) = open::that(&auth_url) {
        eprintln!("  ⚠ Could not open browser automatically: {}", e);
    }

    let code = receive_callback_code()?;

    spotify
        .request_token(&code)
        .await
        .context("Failed to exchange authorization code for tokens")?;

    println!("✓ Spotify authenticated successfully.");
    Ok(())
}

/// Full restore + interactive OAuth (CLI / headless). Prefer
/// [`restore_spotify_session`] + GUI when launching the desktop app.
#[allow(dead_code)]
pub async fn authenticate(config: &AppConfig) -> Result<AuthCodeSpotify> {
    let spotify = create_spotify_client(config);
    let cache_only = crate::spotify_api::cache_only_mode();

    if try_restore_from_keyring(&spotify).await? {
        if cache_only {
            println!("✓ Spotify API disabled; using keyring token without verification.");
        } else {
            println!("✓ Spotify authenticated (restored from keyring).");
        }
        return Ok(spotify);
    }

    if cache_only {
        return Err(anyhow!(
            "ONYX_CACHE_ONLY=1 requires a valid Spotify token in the keyring"
        ));
    }

    authenticate_interactive(&spotify).await?;
    Ok(spotify)
}

/// Current Web API access token, if the client holds one.
pub async fn access_token(spotify: &AuthCodeSpotify) -> Option<String> {
    match spotify.token.lock().await {
        Ok(guard) => guard.as_ref().map(|t| t.access_token.clone()),
        Err(_) => {
            log::warn!("Spotify token mutex could not be locked");
            None
        }
    }
}

/// Restore only; returns `None` if the user must sign in again (no browser).
pub async fn restore_spotify_session(config: &AppConfig) -> Result<Option<AuthCodeSpotify>> {
    let spotify = create_spotify_client(config);
    if try_restore_from_keyring(&spotify).await? {
        Ok(Some(spotify))
    } else {
        Ok(None)
    }
}

// ---------------------------------------------------------------------------
// Local callback server
// ---------------------------------------------------------------------------

/// Start a one-shot HTTP server on 127.0.0.1:8888, wait for Spotify's
/// redirect, parse the `code` query parameter, and return it.
fn receive_callback_code() -> Result<String> {
    let server = Server::http("127.0.0.1:8888")
        .map_err(|e| anyhow!("Failed to start callback server: {}", e))?;

    println!("  Waiting for Spotify callback on {}…", REDIRECT_URI);

    // Block until the single callback request arrives.
    let request = server
        .recv()
        .context("Failed to receive callback request")?;

    // Parse the full URL to extract query params.
    let full_url = format!("http://127.0.0.1:8888{}", request.url());
    let parsed = Url::parse(&full_url).context("Failed to parse callback URL")?;

    let params: HashMap<String, String> = parsed.query_pairs().into_owned().collect();

    // Check for an error from Spotify (e.g., user denied access).
    if let Some(err) = params.get("error") {
        let html = format!(
            "<html><body><h2>Authentication Failed</h2><p>{}</p></body></html>",
            err
        );
        let response = Response::from_string(html)
            .with_header(
                "Content-Type: text/html"
                    .parse::<tiny_http::Header>()
                    .unwrap(),
            );
        let _ = request.respond(response);
        return Err(anyhow!("Spotify authorization denied: {}", err));
    }

    // Extract the authorization code.
    let code = params
        .get("code")
        .ok_or_else(|| anyhow!("No 'code' parameter in Spotify callback"))?
        .clone();

    // Respond to the browser with a friendly success page.
    let html = r#"
        <html>
        <head>
            <style>
                body {
                    font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif;
                    display: flex;
                    justify-content: center;
                    align-items: center;
                    height: 100vh;
                    margin: 0;
                    background: #121212;
                    color: #fff;
                }
                .card {
                    text-align: center;
                    padding: 3rem;
                    border-radius: 16px;
                    background: #1e1e1e;
                }
                h2 { color: #1db954; margin-bottom: 0.5rem; }
                p  { color: #b3b3b3; }
            </style>
        </head>
        <body>
            <div class="card">
                <h2>✓ Authenticated</h2>
                <p>You can close this tab and return to Onyx.</p>
            </div>
        </body>
        </html>
    "#;

    let response = Response::from_string(html).with_header(
        "Content-Type: text/html"
            .parse::<tiny_http::Header>()
            .unwrap(),
    );
    let _ = request.respond(response);

    Ok(code)
}
