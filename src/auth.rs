use anyhow::{Context, Result};
use oauth2::basic::BasicClient;
use oauth2::{
    AuthUrl, AuthorizationCode, ClientId, ClientSecret, CsrfToken, PkceCodeChallenge,
    PkceCodeVerifier, RedirectUrl, RefreshToken, Scope, TokenResponse, TokenUrl,
};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::time::Duration;
use url::Url;

use crate::config::{self, Tokens};

const AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const MAX_RETRIES: u32 = 3;

fn create_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .connect_timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("Client should build")
}

pub async fn login(client_id: &str, client_secret: &str) -> Result<Tokens> {
    let listener = TcpListener::bind("127.0.0.1:0").context("Failed to bind to local port")?;
    let port = listener.local_addr()?.port();

    let client = BasicClient::new(ClientId::new(client_id.to_string()))
        .set_client_secret(ClientSecret::new(client_secret.to_string()))
        .set_auth_uri(AuthUrl::new(AUTH_URL.to_string())?)
        .set_token_uri(TokenUrl::new(TOKEN_URL.to_string())?)
        .set_redirect_uri(RedirectUrl::new(format!("http://localhost:{}", port))?);

    let http_client = create_http_client();
    let (pkce_challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();
    let pkce_secret = pkce_verifier.secret().to_string();

    let (auth_url, csrf_token) = client
        .authorize_url(CsrfToken::new_random)
        .add_scope(Scope::new(
            "https://www.googleapis.com/auth/youtube".to_string(),
        ))
        .set_pkce_challenge(pkce_challenge)
        .url();

    println!("Opening browser for authentication...");
    open::that(auth_url.as_str())?;

    let code = wait_for_callback(listener, csrf_token)?;

    let mut last_error = None;
    let mut token_result = None;

    for attempt in 0..MAX_RETRIES {
        if attempt > 0 {
            let delay = Duration::from_secs(1 << attempt);
            eprintln!("Retrying in {:?}...", delay);
            tokio::time::sleep(delay).await;
        }

        let verifier = PkceCodeVerifier::new(pkce_secret.clone());
        match client
            .exchange_code(code.clone())
            .set_pkce_verifier(verifier)
            .request_async(&http_client)
            .await
        {
            Ok(result) => {
                token_result = Some(result);
                break;
            }
            Err(e) => {
                let err_str = format!("{:?}", e);
                if err_str.contains("timed out") || err_str.contains("Timeout") {
                    eprintln!(
                        "Token exchange timed out (attempt {}/{})",
                        attempt + 1,
                        MAX_RETRIES
                    );
                    last_error = Some(e);
                } else {
                    return Err(e).context("Failed to exchange code for token");
                }
            }
        }
    }

    let token_result = token_result
        .ok_or_else(|| last_error.unwrap())
        .context("Failed to exchange code for token after retries")?;

    let tokens = Tokens {
        access_token: token_result.access_token().secret().to_string(),
        refresh_token: token_result
            .refresh_token()
            .map(|t| t.secret().to_string())
            .ok_or_else(|| anyhow::anyhow!("No refresh token received"))?,
    };

    config::save_tokens(&tokens)?;
    Ok(tokens)
}

fn wait_for_callback(listener: TcpListener, expected_csrf: CsrfToken) -> Result<AuthorizationCode> {
    let port = listener.local_addr()?.port();
    println!("Waiting for OAuth callback on port {}...", port);

    let (mut stream, _) = listener.accept()?;
    let mut reader = BufReader::new(&stream);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;

    let redirect_url = request_line
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("Invalid request"))?;

    let url = Url::parse(&format!("http://localhost{}", redirect_url))?;

    let code = url
        .query_pairs()
        .find(|(key, _)| key == "code")
        .map(|(_, value)| AuthorizationCode::new(value.into_owned()))
        .ok_or_else(|| anyhow::anyhow!("No code in callback"))?;

    let state = url
        .query_pairs()
        .find(|(key, _)| key == "state")
        .map(|(_, value)| CsrfToken::new(value.into_owned()))
        .ok_or_else(|| anyhow::anyhow!("No state in callback"))?;

    if state.secret() != expected_csrf.secret() {
        anyhow::bail!("CSRF token mismatch");
    }

    let response = "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\r\n<html><body><h1>Authentication successful!</h1><p>You can close this window.</p></body></html>";
    stream.write_all(response.as_bytes())?;

    Ok(code)
}

pub async fn refresh_token(client_id: &str, client_secret: &str, refresh: &str) -> Result<Tokens> {
    let client = BasicClient::new(ClientId::new(client_id.to_string()))
        .set_client_secret(ClientSecret::new(client_secret.to_string()))
        .set_auth_uri(AuthUrl::new(AUTH_URL.to_string())?)
        .set_token_uri(TokenUrl::new(TOKEN_URL.to_string())?);

    let http_client = create_http_client();
    let mut last_error = None;

    for attempt in 0..MAX_RETRIES {
        if attempt > 0 {
            let delay = Duration::from_secs(1 << attempt);
            eprintln!("Retrying in {:?}...", delay);
            tokio::time::sleep(delay).await;
        }

        match client
            .exchange_refresh_token(&RefreshToken::new(refresh.to_string()))
            .request_async(&http_client)
            .await
        {
            Ok(result) => {
                let tokens = Tokens {
                    access_token: result.access_token().secret().to_string(),
                    refresh_token: result
                        .refresh_token()
                        .map(|t| t.secret().to_string())
                        .unwrap_or_else(|| refresh.to_string()),
                };
                config::save_tokens(&tokens)?;
                return Ok(tokens);
            }
            Err(e) => {
                let err_str = format!("{:?}", e);
                if err_str.contains("timed out") || err_str.contains("Timeout") {
                    eprintln!(
                        "Token refresh timed out (attempt {}/{})",
                        attempt + 1,
                        MAX_RETRIES
                    );
                    last_error = Some(e);
                } else {
                    return Err(e).context("Failed to refresh token");
                }
            }
        }
    }

    Err(last_error.unwrap()).context("Failed to refresh token after retries")
}
