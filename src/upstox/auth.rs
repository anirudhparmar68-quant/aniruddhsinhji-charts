//! Upstox OAuth login and access-token cache.
//!
//! Upstox tokens die at ~03:30 IST the next day, so this is a once-a-day
//! interactive login. The redirect is caught by a one-shot local HTTP server
//! on the port from `UPSTOX_REDIRECT_URI`, so nothing has to be pasted by hand.

use crate::config::{token_path, Secrets};
use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Duration, FixedOffset, NaiveTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use std::time::Instant;

const AUTH_URL: &str = "https://api.upstox.com/v2/login/authorization/dialog";
const TOKEN_URL: &str = "https://api.upstox.com/v2/login/authorization/token";
const PROFILE_URL: &str = "https://api.upstox.com/v2/user/profile";

fn ist() -> FixedOffset {
    FixedOffset::east_opt(5 * 3600 + 30 * 60).expect("IST offset is valid")
}

/// Upstox invalidates tokens at 03:30 IST — return the next such boundary.
fn next_expiry() -> DateTime<FixedOffset> {
    let now = Utc::now().with_timezone(&ist());
    let boundary_naive = now
        .date_naive()
        .and_time(NaiveTime::from_hms_opt(3, 30, 0).expect("03:30 is a valid time"));
    let boundary = ist()
        .from_local_datetime(&boundary_naive)
        .single()
        .expect("fixed offset is never ambiguous");
    if now >= boundary {
        boundary + Duration::days(1)
    } else {
        boundary
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct CachedToken {
    access_token: String,
    expires_at: String,
    saved_at: String,
}

fn save_token(token: &str) -> Result<()> {
    let cached = CachedToken {
        access_token: token.to_string(),
        expires_at: next_expiry().to_rfc3339(),
        saved_at: Utc::now().with_timezone(&ist()).to_rfc3339(),
    };
    let path = token_path();
    std::fs::write(&path, serde_json::to_string_pretty(&cached)?)
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Cached token if it exists and has not passed its 03:30 IST boundary.
pub fn load_cached_token() -> Option<String> {
    let raw = std::fs::read_to_string(token_path()).ok()?;
    let cached: CachedToken = serde_json::from_str(&raw).ok()?;
    let expires = DateTime::parse_from_rfc3339(&cached.expires_at).ok()?;
    if Utc::now().with_timezone(&ist()) < expires {
        Some(cached.access_token)
    } else {
        None
    }
}

pub fn clear_cached_token() {
    let _ = std::fs::remove_file(token_path());
}

pub fn build_login_url(secrets: &Secrets) -> String {
    format!(
        "{AUTH_URL}?response_type=code&client_id={}&redirect_uri={}",
        urlencoding::encode(&secrets.api_key),
        urlencoding::encode(&secrets.redirect_uri),
    )
}

/// `true` when the token actually works. Cheap sanity check at startup.
pub async fn validate_token(client: &reqwest::Client, token: &str) -> bool {
    match client
        .get(PROFILE_URL)
        .bearer_auth(token)
        .header("Accept", "application/json")
        .send()
        .await
    {
        Ok(r) => r.status().is_success(),
        Err(_) => false,
    }
}

fn query_param(url: &str, key: &str) -> Option<String> {
    let query = url.split_once('?')?.1;
    for pair in query.split('&') {
        let (k, v) = pair.split_once('=')?;
        if k == key {
            return urlencoding::decode(v).ok().map(|s| s.into_owned());
        }
    }
    None
}

/// Blocking one-shot HTTP listener that grabs `?code=` off the OAuth redirect.
/// Loops because browsers also hit `/favicon.ico` on the same origin.
fn capture_code_blocking(host: &str, port: u16, timeout: std::time::Duration) -> Result<String> {
    let addr = format!("{host}:{port}");
    let server = tiny_http::Server::http(addr.as_str())
        .map_err(|e| anyhow!("could not bind {addr} for the OAuth redirect: {e}"))?;
    let deadline = Instant::now() + timeout;

    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| anyhow!("timed out waiting for the Upstox redirect"))?;

        let Some(request) = server.recv_timeout(remaining)? else {
            return Err(anyhow!("timed out waiting for the Upstox redirect"));
        };

        let code = query_param(request.url(), "code");
        let body = if code.is_some() {
            "<html><body style='font-family:system-ui;text-align:center;margin-top:18vh'>\
             <h2>Login successful</h2><p>You can close this tab and go back to Spider Charts.</p>\
             </body></html>"
        } else {
            "<html><body style='font-family:system-ui;text-align:center;margin-top:18vh'>\
             <h2>Waiting for the authorization code…</h2></body></html>"
        };
        let header = tiny_http::Header::from_bytes(
            &b"Content-Type"[..],
            &b"text/html; charset=utf-8"[..],
        )
        .expect("static header is valid");
        let _ = request.respond(tiny_http::Response::from_string(body).with_header(header));

        if let Some(code) = code {
            return Ok(code);
        }
    }
}

async fn exchange_code(
    client: &reqwest::Client,
    secrets: &Secrets,
    code: &str,
) -> Result<String> {
    let resp = client
        .post(TOKEN_URL)
        .header("Accept", "application/json")
        .form(&[
            ("code", code),
            ("client_id", secrets.api_key.as_str()),
            ("client_secret", secrets.api_secret.as_str()),
            ("redirect_uri", secrets.redirect_uri.as_str()),
            ("grant_type", "authorization_code"),
        ])
        .send()
        .await
        .context("token exchange request failed")?;

    let status = resp.status();
    let body: serde_json::Value = resp.json().await.context("token response was not JSON")?;
    if !status.is_success() {
        anyhow::bail!("Upstox token exchange failed ({status}): {body}");
    }
    body.get("access_token")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow!("no access_token in Upstox response: {body}"))
}

/// Full interactive login: opens the browser, catches the redirect, caches the token.
pub async fn login(client: &reqwest::Client, secrets: &Secrets) -> Result<String> {
    let url = build_login_url(secrets);
    let (host, port) = secrets.redirect_host_port();

    // Start listening before opening the browser so we cannot miss the redirect.
    let listener = tokio::task::spawn_blocking(move || {
        capture_code_blocking(&host, port, std::time::Duration::from_secs(300))
    });

    if let Err(e) = open::that_detached(&url) {
        eprintln!("[auth] could not open a browser ({e}). Open this URL manually:\n{url}");
    }

    let code = listener.await.context("redirect listener panicked")??;
    let token = exchange_code(client, secrets, &code).await?;
    save_token(&token)?;
    Ok(token)
}

/// Guarantee a working token: reuse the cached one if Upstox still accepts it,
/// otherwise run the interactive login.
pub async fn ensure_token(client: &reqwest::Client, secrets: &Secrets) -> Result<String> {
    if let Some(token) = load_cached_token() {
        if validate_token(client, &token).await {
            return Ok(token);
        }
        clear_cached_token();
    }
    login(client, secrets).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_code_from_redirect() {
        assert_eq!(
            query_param("/callback?code=abc123&state=x", "code").as_deref(),
            Some("abc123")
        );
        assert_eq!(query_param("/favicon.ico", "code"), None);
        assert_eq!(query_param("/callback?error=denied", "code"), None);
    }

    #[test]
    fn expiry_is_always_in_the_future() {
        let now = Utc::now().with_timezone(&ist());
        assert!(next_expiry() > now);
    }
}
