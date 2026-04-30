/// Spotify OAuth2 Authorization Code flow.
///
/// Launches the user's browser to the Spotify authorization page,
/// spins up a local HTTP server to catch the callback, exchanges
/// the authorization code for access + refresh tokens, and returns
/// an authenticated `AuthCodeSpotify` client.

use anyhow::{anyhow, Context, Result};
use rspotify::{
    prelude::*,
    scopes, AuthCodeSpotify, Config, Credentials, OAuth,
};
use std::collections::HashMap;
use tiny_http::{Response, Server};
use url::Url;

use crate::config::AppConfig;

/// The redirect URI — must exactly match what's registered in the
/// Spotify Developer Dashboard.  Spotify no longer allows `localhost`;
/// a loopback IP literal is required.
const REDIRECT_URI: &str = "http://127.0.0.1:8888/callback";

/// Run the full OAuth2 flow and return an authenticated Spotify client.
pub async fn authenticate(config: &AppConfig) -> Result<AuthCodeSpotify> {
    // ── 1. Build rspotify credentials & OAuth ────────────────────────
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
        token_cached: true,
        cache_path: std::path::PathBuf::from(".spotify_token_cache.json"),
        ..Default::default()
    };

    let spotify = AuthCodeSpotify::with_config(creds, oauth, rspotify_config);
    let cache_only = crate::spotify_api::cache_only_mode();

    // ── 2. Try to use a cached token first ───────────────────────────
    // If a valid (or refreshable) token is on disk, skip the browser flow.
    let token_from_cache = spotify.read_token_cache(false).await;
    if let Ok(Some(token)) = token_from_cache {
        // Place the cached token into the client.
        *spotify.token.lock().await.unwrap() = Some(token);

        if cache_only {
            println!("✓ Spotify API disabled; using cached token without verification.");
            return Ok(spotify);
        }

        // Attempt a lightweight API call to verify the token is still good.
        if spotify.current_user().await.is_ok() {
            println!("✓ Spotify authenticated (cached token).");
            return Ok(spotify);
        }
        // Token was invalid/expired and couldn't refresh — fall through
        // to the full browser flow.
    }

    if cache_only {
        return Err(anyhow!(
            "ONYX_CACHE_ONLY=1 requires an existing cached Spotify token"
        ));
    }

    // ── 3. Generate the authorization URL ────────────────────────────
    let auth_url = spotify.get_authorize_url(false)?;

    println!();
    println!("  Opening Spotify login in your browser…");
    println!("  (If nothing opens, visit this URL manually:)");
    println!("  {}", auth_url);
    println!();

    // ── 4. Open the browser ──────────────────────────────────────────
    if let Err(e) = open::that(&auth_url) {
        eprintln!("  ⚠ Could not open browser automatically: {}", e);
    }

    // ── 5. Spin up local server & wait for the callback ──────────────
    let code = receive_callback_code()?;

    // ── 6. Exchange code for tokens ──────────────────────────────────
    spotify
        .request_token(&code)
        .await
        .context("Failed to exchange authorization code for tokens")?;

    // Persist the token to disk for next launch.
    spotify
        .write_token_cache()
        .await
        .context("Failed to cache Spotify token")?;

    println!("✓ Spotify authenticated successfully.");
    Ok(spotify)
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
