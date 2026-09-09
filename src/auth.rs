//! Google OAuth 2.0 (installed app / loopback) authentication and token persistence.

use std::collections::HashMap;
use std::io::ErrorKind;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use chrono::{Duration as ChronoDuration, Utc};
use oauth2::basic::BasicClient;
use oauth2::TokenResponse as _;
use oauth2::{
    AuthUrl, AuthorizationCode, ClientId, ClientSecret, CsrfToken, EndpointNotSet, EndpointSet,
    PkceCodeChallenge, RedirectUrl, RefreshToken, Scope, TokenUrl,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tracing::{debug, info, warn};

use crate::error::OxidriveError;

/// Google OAuth client with authorization + token endpoints configured (redirect URI is applied per flow).
type GoogleOAuthClient =
    BasicClient<EndpointSet, EndpointNotSet, EndpointNotSet, EndpointNotSet, EndpointSet>;

/// Serializable OAuth token bundle stored at the configured `token_path` (typically `token.json`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenResponse {
    /// Bearer access token for Google APIs.
    pub access_token: String,
    /// Token type (usually `Bearer`).
    #[serde(default)]
    pub token_type: Option<String>,
    /// Refresh token, when granted by the authorization server.
    #[serde(default)]
    pub refresh_token: Option<String>,
    /// `expires_in` seconds from the token response, if provided.
    #[serde(default)]
    pub expires_in: Option<u64>,
    /// Granted scopes, if returned by the server.
    #[serde(default)]
    pub scope: Option<String>,
    /// Absolute expiry instant for the access token (UTC), if known.
    #[serde(default, with = "chrono::serde::ts_seconds_option")]
    pub expires_at: Option<chrono::DateTime<Utc>>,
}

/// Errors specific to the OAuth / token lifecycle.
#[derive(Debug, Error)]
pub enum AuthError {
    /// Misconfigured OAuth endpoints or redirect URL.
    #[error("OAuth configuration error: {0}")]
    Configuration(String),
    /// Failure opening the system browser or local HTTP server.
    #[error("local OAuth loopback error: {0}")]
    Loopback(String),
    /// Could not parse the OAuth callback request.
    #[error("invalid OAuth callback request")]
    OAuthCallbackParse,
    /// Authorization server returned an explicit OAuth error (for example, user denied consent).
    #[error("OAuth authorization denied: {0}")]
    OAuthDenied(String),
    /// CSRF `state` did not match the value issued at authorization time.
    #[error("OAuth state mismatch (possible CSRF)")]
    StateMismatch,
    /// Token exchange or refresh failed.
    #[error("token request failed: {0}")]
    TokenRequest(String),
    /// Token file missing or unusable.
    #[error("not authorized (missing or invalid token)")]
    NotAuthorized,
    /// I/O while reading/writing the token file.
    #[error("token storage I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// JSON (de)serialization of the token file.
    #[error("token JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

impl From<AuthError> for OxidriveError {
    fn from(value: AuthError) -> Self {
        OxidriveError::Auth(value.to_string())
    }
}

impl From<oauth2::url::ParseError> for AuthError {
    fn from(value: oauth2::url::ParseError) -> Self {
        AuthError::Configuration(value.to_string())
    }
}

/// Manages Google OAuth credentials, loopback authorization, and JSON token persistence.
pub struct AuthManager {
    token_path: PathBuf,
    client_id: String,
    client_secret: String,
    http: reqwest::Client,
}

impl AuthManager {
    /// Creates a manager for the given OAuth client credentials and token storage path.
    ///
    /// Google authorization and token endpoints are applied when starting a flow or refreshing tokens.
    pub fn new(
        client_id: impl Into<String>,
        client_secret: impl Into<String>,
        token_path: impl Into<PathBuf>,
    ) -> Self {
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            token_path: token_path.into(),
            client_id: client_id.into(),
            client_secret: client_secret.into(),
            http,
        }
    }

    /// Builds a [`BasicClient`] with Google’s OAuth endpoints and this manager’s credentials.
    fn base_oauth_client(&self) -> Result<GoogleOAuthClient, AuthError> {
        let auth_url = AuthUrl::new("https://accounts.google.com/o/oauth2/v2/auth".to_string())?;
        let token_url = TokenUrl::new("https://oauth2.googleapis.com/token".to_string())?;
        Ok(BasicClient::new(ClientId::new(self.client_id.clone()))
            .set_client_secret(ClientSecret::new(self.client_secret.clone()))
            .set_auth_uri(auth_url)
            .set_token_uri(token_url))
    }

    /// Runs the interactive browser + loopback flow, exchanges the authorization code, and saves [`TokenResponse`] JSON to disk.
    pub async fn setup(&self) -> Result<(), OxidriveError> {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .map_err(|e| AuthError::Loopback(format!("failed to bind loopback listener: {e}")))?;
        let port = listener
            .local_addr()
            .map_err(|e| AuthError::Loopback(format!("failed to read local addr: {e}")))?;
        let redirect = RedirectUrl::new(format!("http://127.0.0.1:{}/", port.port()))
            .map_err(AuthError::from)?;

        let oauth_client = self.base_oauth_client()?.set_redirect_uri(redirect);
        let (pkce_challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();
        let (auth_url, csrf) = oauth_client
            .authorize_url(CsrfToken::new_random)
            .add_scope(Scope::new(
                "https://www.googleapis.com/auth/drive".to_string(),
            ))
            .set_pkce_challenge(pkce_challenge)
            .url();

        info!(url = %auth_url, "opening browser for Google OAuth consent");
        open_browser(auth_url.as_str())
            .map_err(|e| AuthError::Loopback(format!("failed to open browser: {e}")))?;

        let code = match tokio::time::timeout(Duration::from_secs(10 * 60), async {
            accept_oauth_callback(listener, csrf.secret().as_str()).await
        })
        .await
        {
            Ok(res) => res?,
            Err(_) => {
                return Err(
                    AuthError::Loopback("timed out waiting for OAuth callback".into()).into(),
                );
            }
        };

        let token = oauth_client
            .exchange_code(AuthorizationCode::new(code))
            .set_pkce_verifier(pkce_verifier)
            .request_async(&self.http)
            .await
            .map_err(|e| AuthError::TokenRequest(e.to_string()))?;

        let stored = token_response_from_oauth(&token);
        self.save_token(&stored)?;
        info!(path = %self.token_path.display(), "saved OAuth token");
        Ok(())
    }

    /// Loads a valid access token, refreshing with the refresh token when expired or near expiry.
    pub async fn get_access_token(&self) -> Result<String, OxidriveError> {
        let token = self.load_token()?;
        if access_token_usable(&token) {
            return Ok(token.access_token);
        }
        let Some(ref refresh) = token.refresh_token else {
            return Err(AuthError::NotAuthorized.into());
        };
        debug!("access token expired or near expiry; refreshing");
        let refreshed = self
            .base_oauth_client()?
            .exchange_refresh_token(&RefreshToken::new(refresh.clone()))
            .request_async(&self.http)
            .await
            .map_err(|e| AuthError::TokenRequest(e.to_string()))?;
        let mut updated = token_response_from_oauth(&refreshed);
        if updated.refresh_token.is_none() {
            updated.refresh_token = token.refresh_token.clone();
        }
        self.save_token(&updated)?;
        Ok(updated.access_token)
    }

    /// Reads the token file from disk.
    pub fn load_token(&self) -> Result<TokenResponse, OxidriveError> {
        let bytes = match std::fs::read(&self.token_path) {
            Ok(b) => b,
            Err(e) if e.kind() == ErrorKind::NotFound => {
                return Err(AuthError::NotAuthorized.into());
            }
            Err(e) => return Err(AuthError::from(e).into()),
        };
        let mut t: TokenResponse = serde_json::from_slice(&bytes).map_err(AuthError::from)?;
        if t.expires_at.is_none() {
            t.expires_at = infer_expires_at_from_file_mtime(&self.token_path, t.expires_in)?;
        }
        Ok(t)
    }

    /// Persists `token` as JSON at the configured path (staging file then rename).
    pub fn save_token(&self, token: &TokenResponse) -> Result<(), OxidriveError> {
        if let Some(parent) = self.token_path.parent() {
            std::fs::create_dir_all(parent).map_err(AuthError::from)?;
        }
        let data = serde_json::to_vec_pretty(token).map_err(AuthError::from)?;
        let tmp = self.token_path.with_extension("json.part");
        let mut opts = std::fs::OpenOptions::new();
        opts.create(true).truncate(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut file = opts.open(&tmp).map_err(AuthError::from)?;
        use std::io::Write as _;
        file.write_all(&data).map_err(AuthError::from)?;
        file.sync_all().map_err(AuthError::from)?;
        std::fs::rename(&tmp, &self.token_path).map_err(AuthError::from)?;
        Ok(())
    }
}

fn access_token_usable(token: &TokenResponse) -> bool {
    match token.expires_at {
        Some(exp) => Utc::now() < exp - ChronoDuration::seconds(60),
        None => true,
    }
}

fn infer_expires_at_from_file_mtime(
    path: &std::path::Path,
    expires_in: Option<u64>,
) -> Result<Option<chrono::DateTime<Utc>>, OxidriveError> {
    let Some(expires_in) = expires_in else {
        return Ok(None);
    };
    let modified = std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .map_err(AuthError::from)?;
    let modified_at = chrono::DateTime::<Utc>::from(modified);
    let secs = i64::try_from(expires_in).unwrap_or(i64::MAX);
    Ok(Some(modified_at + ChronoDuration::seconds(secs)))
}

fn token_response_from_oauth(token: &oauth2::basic::BasicTokenResponse) -> TokenResponse {
    let access_token = token.access_token().secret().to_string();
    let refresh_token = token.refresh_token().map(|t| t.secret().to_string());
    let expires_at = token.expires_in().map(|d| {
        let secs = i64::try_from(d.as_secs()).unwrap_or(i64::MAX);
        Utc::now() + ChronoDuration::seconds(secs)
    });
    let scope = token.scopes().and_then(|scopes| {
        let joined = scopes
            .iter()
            .map(|s| s.as_ref())
            .collect::<Vec<_>>()
            .join(" ");
        if joined.is_empty() {
            None
        } else {
            Some(joined)
        }
    });
    TokenResponse {
        access_token,
        token_type: Some("Bearer".to_string()),
        refresh_token,
        expires_in: token.expires_in().map(|d| d.as_secs()),
        scope,
        expires_at,
    }
}

fn open_browser(url: &str) -> Result<(), std::io::Error> {
    #[cfg(target_os = "macos")]
    {
        let status = Command::new("open").arg(url).status()?;
        if !status.success() {
            return Err(std::io::Error::other("`open` exited with non-zero status"));
        }
        Ok(())
    }
    #[cfg(target_os = "windows")]
    {
        let status = Command::new("cmd")
            .args(["/C", "start", ""])
            .arg(url)
            .status()?;
        if !status.success() {
            return Err(std::io::Error::other(
                "`cmd /c start` exited with non-zero status",
            ));
        }
        Ok(())
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        use std::process::Stdio;
        let status = Command::new("xdg-open")
            .arg(url)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?;
        if !status.success() {
            return Err(std::io::Error::other(
                "`xdg-open` exited with non-zero status",
            ));
        }
        Ok(())
    }
}

async fn accept_oauth_callback(
    listener: TcpListener,
    expected_state: &str,
) -> Result<String, AuthError> {
    const MAX_INVALID_CALLBACKS: usize = 16;
    let mut invalid_callbacks = 0usize;

    loop {
        let (mut stream, peer) = listener
            .accept()
            .await
            .map_err(|e| AuthError::Loopback(format!("accept failed: {e}")))?;
        debug!(%peer, "accepted OAuth callback connection");

        let req = read_callback_request(&mut stream).await?;
        match parse_callback_code(&req, expected_state) {
            Ok(code) => {
                let body = b"<!doctype html><meta charset=\"utf-8\"><title>oxidrive</title><p>Authorization complete. You can close this window.</p>";
                write_callback_response(&mut stream, "200 OK", body).await?;
                return Ok(code);
            }
            Err(AuthError::OAuthDenied(message)) => {
                let body = format!(
                    "<!doctype html><meta charset=\"utf-8\"><title>oxidrive</title><p>Authorization failed: {}</p>",
                    html_escape(&message)
                );
                let _ =
                    write_callback_response(&mut stream, "400 Bad Request", body.as_bytes()).await;
                return Err(AuthError::OAuthDenied(message));
            }
            Err(AuthError::OAuthCallbackParse) | Err(AuthError::StateMismatch) => {
                invalid_callbacks += 1;
                let _ = write_callback_response(
                    &mut stream,
                    "400 Bad Request",
                    b"<!doctype html><meta charset=\"utf-8\"><title>oxidrive</title><p>Invalid OAuth callback. You can close this window.</p>",
                )
                .await;
                if invalid_callbacks >= MAX_INVALID_CALLBACKS {
                    return Err(AuthError::Loopback(
                        "too many invalid OAuth callback attempts".to_string(),
                    ));
                }
            }
            Err(err) => return Err(err),
        }
    }
}

async fn read_callback_request(stream: &mut tokio::net::TcpStream) -> Result<String, AuthError> {
    const MAX_REQUEST_BYTES: usize = 64 * 1024;
    let mut request = Vec::with_capacity(4096);
    let mut read_buf = [0u8; 4096];

    loop {
        let n = stream
            .read(&mut read_buf)
            .await
            .map_err(|e| AuthError::Loopback(format!("read failed: {e}")))?;
        if n == 0 {
            break;
        }
        request.extend_from_slice(&read_buf[..n]);
        if request.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        if request.len() >= MAX_REQUEST_BYTES {
            return Err(AuthError::OAuthCallbackParse);
        }
    }

    std::str::from_utf8(&request)
        .map(str::to_string)
        .map_err(|_| AuthError::OAuthCallbackParse)
}

fn parse_callback_code(req: &str, expected_state: &str) -> Result<String, AuthError> {
    let first = req.lines().next().ok_or(AuthError::OAuthCallbackParse)?;
    let path = first
        .split_whitespace()
        .nth(1)
        .ok_or(AuthError::OAuthCallbackParse)?;
    let query = path
        .split_once('?')
        .map(|(_, q)| q)
        .ok_or(AuthError::OAuthCallbackParse)?;
    let params = parse_query(query);
    let state = params
        .get("state")
        .ok_or(AuthError::OAuthCallbackParse)?
        .as_str();
    if state != expected_state {
        warn!("oauth callback state mismatch");
        return Err(AuthError::StateMismatch);
    }
    if let Some(error) = params.get("error") {
        let message = params
            .get("error_description")
            .cloned()
            .unwrap_or_else(|| error.clone());
        return Err(AuthError::OAuthDenied(message));
    }
    params
        .get("code")
        .cloned()
        .ok_or(AuthError::OAuthCallbackParse)
}

async fn write_callback_response(
    stream: &mut tokio::net::TcpStream,
    status: &str,
    body: &[u8],
) -> Result<(), AuthError> {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream
        .write_all(response.as_bytes())
        .await
        .map_err(|e| AuthError::Loopback(format!("write failed: {e}")))?;
    stream
        .write_all(body)
        .await
        .map_err(|e| AuthError::Loopback(format!("write failed: {e}")))?;
    Ok(())
}

fn parse_query(query: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for pair in query.split('&') {
        let Some((k, v)) = pair.split_once('=') else {
            continue;
        };
        out.insert(url_decode(k), url_decode(v));
    }
    out
}

fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hex = &s[i + 1..i + 3];
                if let Ok(v) = u8::from_str_radix(hex, 16) {
                    out.push(v);
                    i += 3;
                } else {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

fn html_escape(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use tempfile::tempdir;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    #[test]
    fn parse_query_round_trip_simple() {
        let q = "code=abc&state=xyz";
        let m = parse_query(q);
        assert_eq!(m.get("code").map(String::as_str), Some("abc"));
        assert_eq!(m.get("state").map(String::as_str), Some("xyz"));
    }

    #[test]
    fn token_response_serde_round_trip() {
        let t = TokenResponse {
            access_token: "a".into(),
            token_type: Some("Bearer".into()),
            refresh_token: Some("r".into()),
            expires_in: Some(3600),
            scope: Some("drive".into()),
            expires_at: Some(Utc::now()),
        };
        let v = serde_json::to_vec(&t).expect("ser");
        let back: TokenResponse = serde_json::from_slice(&v).expect("de");
        assert_eq!(back.access_token, t.access_token);
        assert_eq!(back.refresh_token, t.refresh_token);
    }

    #[test]
    fn access_token_without_expiry_is_treated_as_usable() {
        let token = TokenResponse {
            access_token: "a".into(),
            token_type: Some("Bearer".into()),
            refresh_token: None,
            expires_in: None,
            scope: Some("drive".into()),
            expires_at: None,
        };
        assert!(access_token_usable(&token));
    }

    #[test]
    fn load_token_infers_expires_at_from_file_mtime() {
        let dir = tempdir().expect("tempdir");
        let token_path = dir.path().join("token.json");
        std::fs::write(
            &token_path,
            r#"{"access_token":"a","token_type":"Bearer","expires_in":3600}"#,
        )
        .expect("write token");

        let auth = AuthManager::new("id", "secret", token_path);
        let token = auth.load_token().expect("load token");
        assert!(token.expires_at.is_some());
        assert!(access_token_usable(&token));
    }

    #[test]
    fn url_decode_handles_utf8_and_invalid_percent_sequences() {
        assert_eq!(url_decode("caf%C3%A9"), "café");
        assert_eq!(url_decode("bad%2Gvalue"), "bad%2Gvalue");
    }

    #[test]
    fn html_escape_escapes_special_characters() {
        assert_eq!(
            html_escape(r#"<script>"x" & 'y'</script>"#),
            "&lt;script&gt;&quot;x&quot; &amp; &#39;y&#39;&lt;/script&gt;"
        );
    }

    #[test]
    fn auth_manager_new_returns_self() {
        let _m = AuthManager::new("id", "secret", PathBuf::from("/tmp/tok.json"));
    }

    #[cfg(unix)]
    #[test]
    fn save_token_restricts_file_permissions() {
        let dir = tempdir().expect("tempdir");
        let token_path = dir.path().join("token.json");
        let auth = AuthManager::new("id", "secret", token_path.clone());
        let token = TokenResponse {
            access_token: "a".into(),
            token_type: Some("Bearer".into()),
            refresh_token: Some("r".into()),
            expires_in: Some(3600),
            scope: Some("drive".into()),
            expires_at: Some(Utc::now()),
        };

        auth.save_token(&token).expect("save token");
        let mode = std::fs::metadata(token_path)
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0);
    }

    #[tokio::test]
    async fn accept_oauth_callback_ignores_invalid_first_connection() {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind listener");
        let addr = listener.local_addr().expect("listener addr");
        let server = tokio::spawn(async move { accept_oauth_callback(listener, "expected").await });

        let mut stray = TcpStream::connect(addr).await.expect("connect stray");
        stray
            .write_all(
                b"GET /?code=nope&state=wrong HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
            )
            .await
            .expect("write stray request");
        let mut stray_response = Vec::new();
        stray
            .read_to_end(&mut stray_response)
            .await
            .expect("read stray response");
        assert!(String::from_utf8_lossy(&stray_response).contains("400 Bad Request"));

        let mut valid = TcpStream::connect(addr).await.expect("connect valid");
        valid
            .write_all(
                b"GET /?code=good-code&state=expected HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
            )
            .await
            .expect("write valid request");
        let mut valid_response = Vec::new();
        valid
            .read_to_end(&mut valid_response)
            .await
            .expect("read valid response");
        assert!(String::from_utf8_lossy(&valid_response).contains("200 OK"));

        let code = server
            .await
            .expect("join callback task")
            .expect("accept callback");
        assert_eq!(code, "good-code");
    }
}
