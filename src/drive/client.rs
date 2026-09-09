//! HTTP client wrapper for Google Drive API calls.

use std::sync::Arc;
use std::time::{Duration, Instant};

use reqwest::header::AUTHORIZATION;
use reqwest::{Client, Method, RequestBuilder, Response};
use serde_json::Value;
use tokio::sync::Mutex;

use crate::error::OxidriveError;
use crate::utils::retry::retry_async;

/// Minimum spacing between outbound Drive requests (simple token bucket).
const DEFAULT_MIN_INTERVAL_MS: u64 = 100;

/// Authenticated Drive client with coarse rate limiting and retries on `429` / `503`.
#[derive(Clone)]
pub struct DriveClient {
    http: Client,
    access_token: String,
    limiter: Arc<Mutex<TokenBucket>>,
    drive_api_base: String,
    upload_api_base: String,
}

struct TokenBucket {
    min_interval: Duration,
    last: Option<Instant>,
}

async fn pace_limiter(limiter: &Arc<Mutex<TokenBucket>>) -> Result<(), OxidriveError> {
    let wait = {
        let mut g = limiter.lock().await;
        let now = Instant::now();
        let scheduled = g.last.map_or(now, |last| last.max(now));

        // Reserve the next slot immediately while holding the mutex so concurrent
        // tasks cannot claim the same request time.
        g.last = Some(scheduled + g.min_interval);
        scheduled.saturating_duration_since(now)
    };
    if wait > Duration::ZERO {
        tokio::time::sleep(wait).await;
    }
    Ok(())
}

impl DriveClient {
    /// Builds a client using the provided OAuth access token.
    pub fn new(access_token: String) -> Self {
        Self::with_base_url(access_token, "https://www.googleapis.com")
    }

    /// Builds a client and overrides the Google API origin (used by integration tests).
    pub fn with_base_url(access_token: String, base_url: impl AsRef<str>) -> Self {
        let http = Client::builder()
            .user_agent(concat!(
                env!("CARGO_PKG_NAME"),
                "/",
                env!("CARGO_PKG_VERSION")
            ))
            .build()
            .unwrap_or_else(|e| {
                tracing::warn!(
                    error = %e,
                    "could not build the HTTP client; using the default instead"
                );
                Client::new()
            });
        let normalized_base = base_url.as_ref().trim_end_matches('/').to_string();
        Self {
            http,
            access_token,
            limiter: Arc::new(Mutex::new(TokenBucket {
                min_interval: Duration::from_millis(DEFAULT_MIN_INTERVAL_MS),
                last: None,
            })),
            drive_api_base: format!("{normalized_base}/drive/v3"),
            upload_api_base: format!("{normalized_base}/upload/drive/v3"),
        }
    }

    /// Builds a Drive API URL from a path/query string (e.g. `/files?...`).
    pub fn drive_api_url(&self, path_and_query: &str) -> String {
        let suffix = path_and_query.trim_start_matches('/');
        format!("{}/{}", self.drive_api_base, suffix)
    }

    /// Builds a Drive upload API URL from a path/query string.
    pub fn upload_api_url(&self, path_and_query: &str) -> String {
        let suffix = path_and_query.trim_start_matches('/');
        format!("{}/{}", self.upload_api_base, suffix)
    }

    /// Performs an HTTP request with bearer auth, spacing, and retries on rate limits.
    ///
    /// `build` is invoked on each retry so headers and bodies can be reconstructed safely.
    pub async fn request(
        &self,
        method: Method,
        url: &str,
        build: impl Fn(RequestBuilder) -> RequestBuilder + Send + Sync + 'static,
    ) -> Result<Response, OxidriveError> {
        let resp = self.request_raw(method, url, build).await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp
                .text()
                .await
                .unwrap_or_else(|_| String::from("<body unavailable>"));
            return Err(OxidriveError::drive(format!("HTTP {status}: {body}")));
        }
        Ok(resp)
    }

    /// Performs an HTTP request with bearer auth, spacing, and retries on transient rate limits.
    ///
    /// Unlike [`DriveClient::request`], this method returns non-2xx responses as-is so callers can
    /// handle protocol-specific statuses such as `308 Resume Incomplete` (resumable uploads).
    pub async fn request_raw(
        &self,
        method: Method,
        url: &str,
        build: impl Fn(RequestBuilder) -> RequestBuilder + Send + Sync + 'static,
    ) -> Result<Response, OxidriveError> {
        let url_owned = url.to_string();
        let token = self.access_token.clone();
        let http = self.http.clone();
        let limiter = Arc::clone(&self.limiter);
        let build = Arc::new(build);

        retry_async(move || {
            let url_owned = url_owned.clone();
            let token = token.clone();
            let http = http.clone();
            let limiter = Arc::clone(&limiter);
            let method = method.clone();
            let build = Arc::clone(&build);
            async move {
                pace_limiter(&limiter).await?;
                let base = http
                    .request(method.clone(), &url_owned)
                    .header(AUTHORIZATION, format!("Bearer {token}"));
                let req = build(base);
                let resp = req
                    .send()
                    .await
                    .map_err(|e| OxidriveError::http(e.to_string()))?;
                let status = resp.status();
                if status.as_u16() == 429 || status.as_u16() == 503 {
                    let body = resp
                        .text()
                        .await
                        .unwrap_or_else(|_| String::from("<body unavailable>"));
                    return Err(OxidriveError::drive(format!(
                        "transient HTTP {status}: {body}"
                    )));
                }
                Ok(resp)
            }
        })
        .await
    }

    /// Returns the `about.user` payload for the authorized account.
    #[allow(dead_code)]
    pub async fn get_user_info(&self) -> Result<Value, OxidriveError> {
        let url = self.drive_api_url("/about?fields=user");
        let resp = self
            .request(Method::GET, &url, |b| b)
            .await?
            .json()
            .await
            .map_err(|e| OxidriveError::drive(format!("parse about: {e}")))?;
        Ok(resp)
    }
}
