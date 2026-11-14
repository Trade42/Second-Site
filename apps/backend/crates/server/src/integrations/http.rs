//! The HTTP half of `apps/api/app/integrations/base.py`: `classify_http_error` and
//! `with_retries`, over a blocking `ureq` agent (the domain provider traits are synchronous; the
//! callers run them on tokio's blocking pool, see [`super::blocking`]).
//!
//! Same behaviour as Python's `httpx` usage:
//! - one timeout value applies to connect, read and write separately (`httpx.Client(timeout=t)`);
//! - redirects are not followed (httpx's default) and any non-2xx status is an error
//!   (`raise_for_status`): 401/403 AUTH, 404 NOT_FOUND, 429 RATE_LIMITED, other >= 500
//!   UNAVAILABLE, anything else BAD_RESPONSE, message `HTTP {code}`;
//! - a timeout is TIMEOUT, any other transport failure UNAVAILABLE;
//! - RATE_LIMITED, TIMEOUT and UNAVAILABLE are retried up to `attempts` times with
//!   `backoff_s * 2^attempt` between attempts; other kinds are raised at once.
//!
//! Differences: Python also sleeps after the last failed attempt before raising; that sleep only
//! delays the error and is skipped here. Transport error messages name the failure class and the
//! I/O cause but never the URL. Credentials are sent only as headers and never appear in a
//! message.

use std::io::Read;
use std::time::Duration;

use ss_domain::integrations::base::{
    kind_for_http_status, ProviderError, ProviderErrorKind, RetryPolicy,
};

/// Bodies larger than this are refused (BAD_RESPONSE) rather than buffered without bound.
pub const MAX_BODY_BYTES: u64 = 64 * 1024 * 1024;

/// A response with a 2xx status and its body read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    /// The raw `Content-Type` header ("" when absent, like `headers.get("content-type", "")`).
    pub content_type: String,
    pub body: Vec<u8>,
}

impl HttpResponse {
    /// `response.json()`; a body that is not JSON is BAD_RESPONSE (httpx raises a
    /// `JSONDecodeError`, which `classify_http_error` maps to BAD_RESPONSE).
    pub fn json(&self, provider: &str) -> Result<serde_json::Value, ProviderError> {
        serde_json::from_slice(&self.body).map_err(|e| {
            ProviderError::new(
                provider,
                ProviderErrorKind::BadResponse,
                format!("response is not JSON: {e}"),
            )
        })
    }
}

/// A blocking HTTP client for one provider, with that provider's retry policy and fixed headers.
pub struct HttpClient {
    provider: &'static str,
    policy: RetryPolicy,
    agent: ureq::Agent,
    headers: Vec<(String, String)>,
}

impl std::fmt::Debug for HttpClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Header values carry credentials: list the names only.
        f.debug_struct("HttpClient")
            .field("provider", &self.provider)
            .field("policy", &self.policy)
            .field(
                "headers",
                &self.headers.iter().map(|(k, _)| k).collect::<Vec<_>>(),
            )
            .finish()
    }
}

fn timeout(policy: &RetryPolicy) -> Duration {
    if policy.timeout_s.is_finite() && policy.timeout_s > 0.0 {
        Duration::from_secs_f64(policy.timeout_s)
    } else {
        Duration::from_secs(20)
    }
}

fn is_timeout(t: &ureq::Transport) -> bool {
    let mut cur: Option<&(dyn std::error::Error + 'static)> = std::error::Error::source(t);
    while let Some(e) = cur {
        if let Some(io) = e.downcast_ref::<std::io::Error>() {
            if matches!(
                io.kind(),
                std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
            ) {
                return true;
            }
        }
        cur = e.source();
    }
    false
}

/// `classify_http_error` for a transport failure.
fn classify_transport(provider: &str, t: &ureq::Transport) -> ProviderError {
    if is_timeout(t) {
        return ProviderError::new(provider, ProviderErrorKind::Timeout, "timed out");
    }
    let mut msg = t.kind().to_string();
    if let Some(src) = std::error::Error::source(t) {
        msg.push_str(": ");
        msg.push_str(&src.to_string());
    } else if let Some(m) = t.message() {
        msg.push_str(": ");
        msg.push_str(m);
    }
    ProviderError::new(provider, ProviderErrorKind::Unavailable, msg)
}

fn status_error(provider: &str, code: u16) -> ProviderError {
    ProviderError::new(provider, kind_for_http_status(code), format!("HTTP {code}"))
}

fn read_body(provider: &str, resp: ureq::Response) -> Result<HttpResponse, ProviderError> {
    let status = resp.status();
    let content_type = resp.header("content-type").unwrap_or("").to_owned();
    let mut body = Vec::new();
    resp.into_reader()
        .take(MAX_BODY_BYTES + 1)
        .read_to_end(&mut body)
        .map_err(|e| {
            if matches!(
                e.kind(),
                std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
            ) {
                ProviderError::new(provider, ProviderErrorKind::Timeout, "timed out")
            } else {
                ProviderError::new(
                    provider,
                    ProviderErrorKind::Unavailable,
                    format!("reading the response failed: {e}"),
                )
            }
        })?;
    if body.len() as u64 > MAX_BODY_BYTES {
        return Err(ProviderError::new(
            provider,
            ProviderErrorKind::BadResponse,
            format!("response larger than {MAX_BODY_BYTES} bytes"),
        ));
    }
    Ok(HttpResponse {
        status,
        content_type,
        body,
    })
}

impl HttpClient {
    /// `httpx.Client(timeout=policy.timeout_s, headers=headers)`.
    pub fn new(
        provider: &'static str,
        policy: RetryPolicy,
        headers: Vec<(String, String)>,
    ) -> Self {
        let t = timeout(&policy);
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(t)
            .timeout_read(t)
            .timeout_write(t)
            .redirects(0)
            .user_agent(concat!("second-site-backend/", env!("CARGO_PKG_VERSION")))
            .build();
        Self {
            provider,
            policy,
            agent,
            headers,
        }
    }

    pub fn provider(&self) -> &'static str {
        self.provider
    }

    pub fn policy(&self) -> RetryPolicy {
        self.policy
    }

    /// `with_retries(provider, policy, fn)`.
    pub fn with_retries<T>(
        &self,
        mut call: impl FnMut() -> Result<T, ProviderError>,
    ) -> Result<T, ProviderError> {
        let mut last: Option<ProviderError> = None;
        for attempt in 0..self.policy.attempts {
            match call() {
                Ok(v) => return Ok(v),
                Err(e) if !e.retryable() => return Err(e),
                Err(e) => last = Some(e),
            }
            if attempt + 1 < self.policy.attempts {
                let pause = self.policy.backoff_s * f64::from(2u32.saturating_pow(attempt));
                if pause.is_finite() && pause > 0.0 {
                    std::thread::sleep(Duration::from_secs_f64(pause));
                }
            }
        }
        Err(last.unwrap_or_else(|| {
            ProviderError::new(
                self.provider,
                ProviderErrorKind::Unavailable,
                "retry policy allows no attempts",
            )
        }))
    }

    fn finish(
        &self,
        r: Result<ureq::Response, ureq::Error>,
    ) -> Result<HttpResponse, ProviderError> {
        match r {
            Ok(resp) if (200..300).contains(&resp.status()) => read_body(self.provider, resp),
            Ok(resp) => Err(status_error(self.provider, resp.status())),
            Err(ureq::Error::Status(code, _)) => Err(status_error(self.provider, code)),
            Err(ureq::Error::Transport(t)) => Err(classify_transport(self.provider, &t)),
        }
    }

    fn request(&self, method: &str, url: &str) -> ureq::Request {
        let mut req = self.agent.request(method, url);
        for (k, v) in &self.headers {
            req = req.set(k, v);
        }
        req
    }

    /// One `GET url?params` attempt (no retries).
    pub fn get_once(
        &self,
        url: &str,
        params: &[(&str, &str)],
    ) -> Result<HttpResponse, ProviderError> {
        let mut req = self.request("GET", url);
        for (k, v) in params {
            req = req.query(k, v);
        }
        self.finish(req.call())
    }

    /// One `POST url` attempt with a JSON body (httpx `json=`), no retries.
    pub fn post_json_once(
        &self,
        url: &str,
        body: &serde_json::Value,
    ) -> Result<HttpResponse, ProviderError> {
        let text = serde_json::to_string(body).map_err(|e| {
            ProviderError::new(
                self.provider,
                ProviderErrorKind::BadResponse,
                format!("request body: {e}"),
            )
        })?;
        let req = self
            .request("POST", url)
            .set("Content-Type", "application/json");
        self.finish(req.send_string(&text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client(attempts: u32) -> HttpClient {
        HttpClient::new(
            "test",
            RetryPolicy {
                attempts,
                backoff_s: 0.0,
                timeout_s: 1.0,
            },
            vec![],
        )
    }

    #[test]
    fn retries_only_retryable_kinds() {
        let c = client(3);
        let mut n = 0;
        let r: Result<(), _> = c.with_retries(|| {
            n += 1;
            Err(ProviderError::new(
                "test",
                ProviderErrorKind::Unavailable,
                "x",
            ))
        });
        assert_eq!(n, 3);
        assert_eq!(r.unwrap_err().kind, ProviderErrorKind::Unavailable);
        let mut n = 0;
        let r: Result<(), _> = c.with_retries(|| {
            n += 1;
            Err(ProviderError::new("test", ProviderErrorKind::Auth, "x"))
        });
        assert_eq!(n, 1);
        assert_eq!(r.unwrap_err().kind, ProviderErrorKind::Auth);
        let mut n = 0;
        let r = c.with_retries(|| {
            n += 1;
            if n < 2 {
                Err(ProviderError::new(
                    "test",
                    ProviderErrorKind::RateLimited,
                    "x",
                ))
            } else {
                Ok(n)
            }
        });
        assert_eq!(r.unwrap(), 2);
        let r: Result<(), _> = client(0).with_retries(|| Ok(()));
        assert!(r.is_err());
    }

    #[test]
    fn status_kinds() {
        assert_eq!(status_error("p", 301).kind, ProviderErrorKind::BadResponse);
        assert_eq!(status_error("p", 403).kind, ProviderErrorKind::Auth);
        assert_eq!(status_error("p", 503).message, "HTTP 503");
    }
}
