//! rutracker-http — async HTTP client over reqwest + cp1251 decoding + login-redirect recovery.
//!
//! Phase 3A deliverable. Cookie extraction from Brave lives in the `rutracker-cookies-macos`
//! crate (Phase 3B). This crate accepts a `HashMap<String, String>` of cookies via
//! [`Client::with_cookies`] and is cookie-source-agnostic so it can be tested with synthetic
//! values.

use encoding_rs::WINDOWS_1251;
use reqwest::StatusCode;
use reqwest::{
    header::{HeaderMap, HeaderValue, ACCEPT, ACCEPT_ENCODING, ACCEPT_LANGUAGE, COOKIE, REFERER},
    Url,
};
use std::collections::HashMap;
use thiserror::Error;

pub mod urls;
pub mod user_agents;

const DEFAULT_ACCEPT: &str =
    "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,*/*;q=0.8";
const DEFAULT_ACCEPT_LANGUAGE: &str = "ru-RU,ru;q=0.9,en-US;q=0.8,en;q=0.7";
const DEFAULT_ACCEPT_ENCODING: &str = "gzip";

#[derive(Debug, Error)]
pub enum Error {
    #[error("reqwest failure: {0}")]
    Reqwest(#[from] reqwest::Error),

    #[error("invalid URL: {0}")]
    InvalidUrl(String),

    #[error("login required — auth cookies missing or expired")]
    LoginRequired,

    #[error("unexpected status: {0}")]
    Status(StatusCode),
}

impl From<reqwest::Url> for Error {
    fn from(_: reqwest::Url) -> Self {
        Error::InvalidUrl("unreachable".to_string())
    }
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone)]
pub struct Client {
    inner: reqwest::Client,
    base_url: Url,
    cookies: HashMap<String, String>,
    user_agent: &'static str,
}

impl Client {
    pub fn new(base_url: &str) -> Result<Self> {
        let user_agent = user_agents::pick_by_hour();
        let inner = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::limited(5))
            .default_headers(default_headers(user_agent))
            .build()?;
        let base_url = Url::parse(base_url).map_err(|e| Error::InvalidUrl(e.to_string()))?;
        Ok(Self {
            inner,
            base_url,
            cookies: HashMap::new(),
            user_agent,
        })
    }

    pub fn with_cookies(mut self, cookies: HashMap<String, String>) -> Self {
        self.cookies = cookies;
        self
    }

    pub fn set_cookie(&mut self, name: impl Into<String>, value: impl Into<String>) {
        self.cookies.insert(name.into(), value.into());
    }

    pub fn user_agent(&self) -> &'static str {
        self.user_agent
    }

    pub fn base(&self) -> &str {
        self.base_url.as_str()
    }

    fn cookie_header(&self) -> Option<HeaderValue> {
        if self.cookies.is_empty() {
            return None;
        }
        let mut pairs: Vec<(&String, &String)> = self.cookies.iter().collect();
        pairs.sort_by(|a, b| a.0.cmp(b.0));
        let joined = pairs
            .into_iter()
            .map(|(k, v)| format!("{}={}", k, v))
            .collect::<Vec<_>>()
            .join("; ");
        HeaderValue::from_str(&joined).ok()
    }

    fn headers(&self, referer: Option<&str>) -> HeaderMap {
        let mut h = HeaderMap::new();
        if let Some(cookie) = self.cookie_header() {
            h.insert(COOKIE, cookie);
        }
        if let Some(referer) = referer.and_then(|value| HeaderValue::from_str(value).ok()) {
            h.insert(REFERER, referer);
        }
        h
    }

    /// Fetch `path` relative to `base_url`, decoding the cp1251 response body as UTF-8.
    ///
    /// Detects the rutracker login redirect and maps it to [`Error::LoginRequired`].
    /// Callers that want raw bytes (e.g. `dl.php`) should use [`Client::get_bytes`].
    pub async fn get_text(&self, path: &str, query: &[(&str, &str)]) -> Result<String> {
        self.get_text_with_referer(path, query, None).await
    }

    pub async fn get_text_with_referer(
        &self,
        path: &str,
        query: &[(&str, &str)],
        referer: Option<&str>,
    ) -> Result<String> {
        let url = self.build_url(path, query)?;
        tracing::debug!(%url, "GET text");
        let resp = self
            .inner
            .get(url)
            .headers(self.headers(referer))
            .send()
            .await?;

        let final_url = resp.url().clone();
        let status = resp.status();
        let bytes = resp.bytes().await?;

        if detect_login_redirect(&final_url, &bytes) {
            return Err(Error::LoginRequired);
        }
        if !status.is_success() {
            return Err(Error::Status(status));
        }

        let (cow, _, _) = WINDOWS_1251.decode(&bytes);
        Ok(cow.into_owned())
    }

    /// Fetch `path` as raw bytes. No cp1251 decode. Used for `.torrent` downloads (Phase 4).
    pub async fn get_bytes(&self, path: &str, query: &[(&str, &str)]) -> Result<Vec<u8>> {
        let url = self.build_url(path, query)?;
        tracing::debug!(%url, "GET bytes");
        let resp = self
            .inner
            .get(url)
            .headers(self.headers(None))
            .send()
            .await?;
        let status = resp.status();
        let bytes = resp.bytes().await?;
        if !status.is_success() {
            return Err(Error::Status(status));
        }
        Ok(bytes.to_vec())
    }

    fn build_url(&self, path: &str, query: &[(&str, &str)]) -> Result<Url> {
        let mut url = self
            .base_url
            .join(path)
            .map_err(|e| Error::InvalidUrl(e.to_string()))?;
        if !query.is_empty() {
            let mut pairs = url.query_pairs_mut();
            for (k, v) in query {
                pairs.append_pair(k, v);
            }
        }
        Ok(url)
    }
}

fn detect_login_redirect(url: &Url, body: &[u8]) -> bool {
    if url.path().contains("login.php") {
        return true;
    }
    // Guard against parse cost: only peek the first 4 KB.
    let head = &body[..body.len().min(4096)];
    let head_str = std::str::from_utf8(head).unwrap_or("");
    head_str.contains(r#"id="login-form""#)
}

fn default_headers(user_agent: &'static str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(ACCEPT, HeaderValue::from_static(DEFAULT_ACCEPT));
    headers.insert(
        ACCEPT_LANGUAGE,
        HeaderValue::from_static(DEFAULT_ACCEPT_LANGUAGE),
    );
    headers.insert(
        ACCEPT_ENCODING,
        HeaderValue::from_static(DEFAULT_ACCEPT_ENCODING),
    );
    headers.insert(
        reqwest::header::USER_AGENT,
        HeaderValue::from_static(user_agent),
    );
    headers
}

#[cfg(test)]
mod tests {
    use super::*;
    use encoding_rs::WINDOWS_1251;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn test_cp1251_decode_and_parse() {
        let server = MockServer::start().await;
        let body_utf8 = "Привет, мир! Это тест cp1251 декодирования.";
        let (cp1251, _, _) = WINDOWS_1251.encode(body_utf8);
        Mock::given(method("GET"))
            .and(path("/forum/tracker.php"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(cp1251.into_owned())
                    .insert_header("content-type", "text/html; charset=windows-1251"),
            )
            .mount(&server)
            .await;

        let client = Client::new(&format!("{}/forum/", server.uri())).unwrap();
        let text = client
            .get_text("tracker.php", &[("nm", "test")])
            .await
            .unwrap();
        assert!(
            text.contains("Привет, мир"),
            "decoded cp1251 body should contain cyrillic text"
        );
    }

    #[tokio::test]
    async fn test_302_login_triggers_error() {
        let server = MockServer::start().await;
        // Simulate login redirect: serve a login-form page directly (reqwest will follow 302s).
        Mock::given(method("GET"))
            .and(path("/forum/viewtopic.php"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"<html><form id="login-form"></form></html>"#),
            )
            .mount(&server)
            .await;

        let client = Client::new(&format!("{}/forum/", server.uri())).unwrap();
        let err = client
            .get_text("viewtopic.php", &[("t", "1")])
            .await
            .unwrap_err();
        matches!(err, Error::LoginRequired);
    }

    #[tokio::test]
    async fn test_dl_returns_bytes() {
        let server = MockServer::start().await;
        let raw = &[0xd4, 0xd5, 0x02, 0x00, 0x00, 0x00, 0xff, 0xfe][..]; // invalid utf-8
        Mock::given(method("GET"))
            .and(path("/forum/dl.php"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(raw.to_vec())
                    .insert_header("content-type", "application/x-bittorrent"),
            )
            .mount(&server)
            .await;

        let client = Client::new(&format!("{}/forum/", server.uri())).unwrap();
        let bytes = client.get_bytes("dl.php", &[("t", "123")]).await.unwrap();
        assert_eq!(
            bytes, raw,
            "bytes must be preserved exactly (no utf-8 decode)"
        );
    }

    #[tokio::test]
    async fn test_cookie_header_sent() {
        // Deterministic order: cookie_header sorts by name alphabetically
        // (bb_guid before bb_session).
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/forum/tracker.php"))
            .and(wiremock::matchers::header(
                "cookie",
                "bb_guid=xyz; bb_session=abc123",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
            .mount(&server)
            .await;

        let mut client = Client::new(&format!("{}/forum/", server.uri())).unwrap();
        client.set_cookie("bb_session", "abc123");
        client.set_cookie("bb_guid", "xyz");
        let text = client.get_text("tracker.php", &[]).await.unwrap();
        assert_eq!(text, "ok");
    }
}
