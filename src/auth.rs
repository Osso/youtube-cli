use anyhow::{Context, Result};
#[cfg(not(test))]
use oauth2::basic::BasicClient;
#[cfg(not(test))]
use oauth2::{
    AuthUrl, AuthorizationCode, ClientId, ClientSecret, CsrfToken, PkceCodeChallenge,
    PkceCodeVerifier, RedirectUrl, RefreshToken, Scope, TokenResponse, TokenUrl,
};
use std::future::Future;
#[cfg(not(test))]
use std::io::{BufRead, BufReader, Write};
#[cfg(not(test))]
use std::net::TcpListener;
use std::time::Duration;
#[cfg(not(test))]
use url::Url;

#[cfg(not(test))]
use crate::config;
use crate::config::Tokens;

#[cfg(not(test))]
const AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
#[cfg(not(test))]
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const MAX_RETRIES: u32 = 3;

#[cfg(not(test))]
fn create_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .connect_timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("Client should build")
}

#[cfg(not(test))]
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
        .add_scope(youtube_scope())
        .set_pkce_challenge(pkce_challenge)
        .url();

    open_auth_browser(auth_url.as_str())?;
    let code = wait_for_callback(listener, csrf_token)?;
    let token_result = retry_request(
        "Failed to exchange code for token",
        "Token exchange",
        || {
            let verifier = PkceCodeVerifier::new(pkce_secret.clone());
            client
                .exchange_code(code.clone())
                .set_pkce_verifier(verifier)
                .request_async(&http_client)
        },
    )
    .await?;

    let tokens = tokens_from_login_exchange(token_result)?;
    config::save_tokens(&tokens)?;
    Ok(tokens)
}

#[cfg(not(test))]
fn youtube_scope() -> Scope {
    Scope::new("https://www.googleapis.com/auth/youtube".to_string())
}

#[cfg(not(test))]
fn open_auth_browser(auth_url: &str) -> Result<()> {
    println!("Opening browser for authentication...");
    open::that(auth_url)?;
    Ok(())
}

#[cfg(not(test))]
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

#[cfg(not(test))]
pub async fn refresh_token(client_id: &str, client_secret: &str, refresh: &str) -> Result<Tokens> {
    let client = BasicClient::new(ClientId::new(client_id.to_string()))
        .set_client_secret(ClientSecret::new(client_secret.to_string()))
        .set_auth_uri(AuthUrl::new(AUTH_URL.to_string())?)
        .set_token_uri(TokenUrl::new(TOKEN_URL.to_string())?);

    let http_client = create_http_client();
    let refresh_token = RefreshToken::new(refresh.to_string());
    let token_result = retry_request("Failed to refresh token", "Token refresh", || {
        client
            .exchange_refresh_token(&refresh_token)
            .request_async(&http_client)
    })
    .await?;

    let tokens = Tokens {
        access_token: token_result.access_token().secret().to_string(),
        refresh_token: token_result
            .refresh_token()
            .map(|token| token.secret().to_string())
            .unwrap_or_else(|| refresh.to_string()),
    };

    config::save_tokens(&tokens)?;
    Ok(tokens)
}

#[cfg(test)]
pub async fn refresh_token(
    _client_id: &str,
    _client_secret: &str,
    refresh: &str,
) -> Result<Tokens> {
    Ok(Tokens {
        access_token: format!("refreshed-{refresh}"),
        refresh_token: refresh.to_string(),
    })
}

#[cfg(not(test))]
fn tokens_from_login_exchange<T: TokenResponse>(token_result: T) -> Result<Tokens> {
    let refresh_token = token_result
        .refresh_token()
        .map(|token| token.secret().to_string())
        .ok_or_else(|| anyhow::anyhow!("No refresh token received"))?;

    Ok(Tokens {
        access_token: token_result.access_token().secret().to_string(),
        refresh_token,
    })
}

async fn retry_request<T, E, F, Fut>(
    error_context: &str,
    timeout_label: &str,
    mut request: F,
) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = std::result::Result<T, E>>,
    E: std::error::Error + Send + Sync + 'static + std::fmt::Debug,
{
    let mut last_timeout = None;

    for attempt in 0..MAX_RETRIES {
        sleep_for_retry(attempt).await;

        match request().await {
            Ok(result) => return Ok(result),
            Err(error) if is_timeout_error(&error) => {
                log_timeout(timeout_label, attempt + 1);
                last_timeout = Some(format!("{error:?}"));
            }
            Err(error) => return Err(anyhow::Error::new(error)).context(error_context.to_string()),
        }
    }

    let fallback = "Unknown timeout".to_string();
    let timeout_details = last_timeout.unwrap_or(fallback);
    anyhow::bail!("{error_context} after retries: {timeout_details}")
}

async fn sleep_for_retry(attempt: u32) {
    if attempt == 0 {
        return;
    }

    let delay = Duration::from_secs(1 << attempt);
    eprintln!("Retrying in {:?}...", delay);
    tokio::time::sleep(delay).await;
}

fn log_timeout(timeout_label: &str, attempt: u32) {
    eprintln!(
        "{timeout_label} timed out (attempt {}/{})",
        attempt, MAX_RETRIES
    );
}

fn is_timeout_error(error: &impl std::fmt::Debug) -> bool {
    let error_text = format!("{error:?}");
    error_text.contains("timed out") || error_text.contains("Timeout")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fmt;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    #[derive(Debug)]
    struct TestError(&'static str);

    impl fmt::Display for TestError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "{}", self.0)
        }
    }

    impl std::error::Error for TestError {}

    #[test]
    fn timeout_detection_matches_debug_text() {
        assert!(is_timeout_error(&TestError("operation timed out")));
        assert!(is_timeout_error(&TestError("Timeout while connecting")));
        assert!(!is_timeout_error(&TestError("permission denied")));
    }

    #[tokio::test]
    async fn retry_request_returns_success_without_retry() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&attempts);

        let value = retry_request("context", "timeout", move || {
            let seen = Arc::clone(&seen);
            async move {
                seen.fetch_add(1, Ordering::SeqCst);
                Ok::<_, TestError>("ok")
            }
        })
        .await
        .unwrap();

        assert_eq!(value, "ok");
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn retry_request_wraps_non_timeout_error() {
        let err = retry_request("exchange failed", "timeout", || async {
            Err::<(), _>(TestError("bad request"))
        })
        .await
        .unwrap_err();

        assert!(err.to_string().contains("exchange failed"));
    }
}
