//! Provider-isolated live ChatGPT Codex model discovery and cache.
//!
//! The catalog uses only the credentials and endpoint owned by
//! codex_auth. It never reads xAI auth.json or the shared model cache. The
//! cache is account-scoped and a completed request is checked again before it
//! is published, so a logout or account switch cannot expose stale metadata.

use crate::codex_auth::{self, CODEX_ORIGINATOR, CodexCredentials};
use anyhow::{Context, anyhow};
use async_trait::async_trait;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use reqwest::header::{ETAG, IF_NONE_MATCH, USER_AGENT};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use url::Url;

pub const CODEX_MODELS_CACHE_FILE: &str = "codex_models_cache.json";
pub const CODEX_CLIENT_VERSION_ENV: &str = "GROK_CODEX_CLIENT_VERSION";
pub const DEFAULT_CODEX_CLIENT_VERSION: &str = "0.144.5";
const CODEX_MODELS_CACHE_TTL: Duration = Duration::from_secs(300);
const CODEX_MODELS_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_EFFECTIVE_CONTEXT_WINDOW_PERCENT: i64 = 95;

/// Resolve the whole semantic version sent to the Codex models endpoint.
pub fn codex_client_version() -> String {
    match std::env::var(CODEX_CLIENT_VERSION_ENV) {
        Ok(value) => normalize_whole_semver(&value)
            .unwrap_or_else(|| DEFAULT_CODEX_CLIENT_VERSION.to_owned()),
        Err(_) => DEFAULT_CODEX_CLIENT_VERSION.to_owned(),
    }
}

fn normalize_whole_semver(value: &str) -> Option<String> {
    let value = value.trim().strip_prefix('v').unwrap_or(value.trim());
    let version = semver::Version::parse(value).ok()?;
    Some(format!("{}.{}.{}", version.major, version.minor, version.patch))
}

/// Visibility supplied by the Codex catalog.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CodexModelVisibility {
    List,
    Hide,
    #[default]
    None,
}

impl CodexModelVisibility {
    pub fn is_list_visible(self) -> bool {
        self == Self::List
    }
}

/// A reasoning option returned by the Codex catalog.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodexReasoningLevel {
    pub effort: String,
    #[serde(default)]
    pub description: String,
}

/// One live Codex model, kept independent from the xAI model types.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CodexCatalogModel {
    pub slug: String,
    pub display_name: String,
    #[serde(default)]
    pub description: Option<String>,
    pub priority: i32,
    pub visibility: CodexModelVisibility,
    /// The server flag is retained for diagnostics, but this transport remains
    /// OAuth-only because no Codex API-key provider is implemented here.
    pub server_supported_in_api: bool,
    pub supported_in_api: bool,
    pub context_window: Option<u64>,
    pub raw_context_window: Option<u64>,
    pub effective_context_window_percent: i64,
    #[serde(default)]
    pub default_reasoning_level: Option<String>,
    #[serde(default)]
    pub supported_reasoning_levels: Vec<CodexReasoningLevel>,
    #[serde(default)]
    pub supports_search_tool: bool,
    #[serde(default)]
    pub tool_mode: Option<String>,
}

impl CodexCatalogModel {
    pub fn is_visible(&self) -> bool {
        self.visibility.is_list_visible()
    }
}

/// Account-scoped catalog snapshot returned by the live endpoint or cache.
#[derive(Clone, Debug, PartialEq)]
pub struct CodexModelsCatalog {
    pub models: Vec<CodexCatalogModel>,
    pub etag: Option<String>,
    account_fingerprint: String,
}

impl CodexModelsCatalog {
    pub fn is_authoritative(&self) -> bool {
        self.models.iter().any(CodexCatalogModel::is_visible)
    }

    pub fn list_visible_models(&self) -> impl Iterator<Item = &CodexCatalogModel> {
        self.models.iter().filter(|model| model.is_visible())
    }

    pub fn account_fingerprint(&self) -> &str {
        &self.account_fingerprint
    }
}

#[derive(Clone, Debug, Deserialize)]
struct CodexModelsResponse {
    #[serde(default)]
    models: Vec<CodexWireModel>,
}

#[derive(Clone, Debug, Deserialize)]
struct CodexWireModel {
    slug: String,
    #[serde(default)]
    display_name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    default_reasoning_level: Option<String>,
    #[serde(default)]
    supported_reasoning_levels: Vec<CodexReasoningLevel>,
    #[serde(default)]
    visibility: CodexModelVisibility,
    #[serde(default)]
    supported_in_api: bool,
    #[serde(default)]
    priority: i32,
    #[serde(default)]
    context_window: Option<i64>,
    #[serde(default)]
    max_context_window: Option<i64>,
    #[serde(default = "default_effective_context_window_percent")]
    effective_context_window_percent: i64,
    #[serde(default)]
    supports_search_tool: bool,
    #[serde(default)]
    tool_mode: Option<String>,
}

const fn default_effective_context_window_percent() -> i64 {
    DEFAULT_EFFECTIVE_CONTEXT_WINDOW_PERCENT
}

#[derive(Debug, Serialize, Deserialize)]
struct CodexModelsCache {
    fetched_at: DateTime<Utc>,
    grok_version: String,
    client_version: String,
    base_origin: String,
    base_url: String,
    account_fingerprint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    etag: Option<String>,
    models: Vec<CodexCatalogModel>,
}

impl CodexModelsCache {
    fn is_fresh(&self, ttl: Duration) -> bool {
        let Ok(ttl) = ChronoDuration::from_std(ttl) else {
            return false;
        };
        let age = Utc::now().signed_duration_since(self.fetched_at);
        age >= ChronoDuration::zero() && age < ttl
    }

    fn into_catalog(self) -> CodexModelsCatalog {
        CodexModelsCatalog {
            models: self.models,
            etag: self.etag,
            account_fingerprint: self.account_fingerprint,
        }
    }
}

#[async_trait]
pub(crate) trait CodexModelsAuthSource: fmt::Debug + Send + Sync {
    fn current_credentials(&self) -> anyhow::Result<Option<CodexCredentials>>;
    async fn fresh_credentials(&self) -> anyhow::Result<Option<CodexCredentials>>;
    async fn force_refresh(&self) -> anyhow::Result<Option<CodexCredentials>>;
}

#[derive(Debug)]
struct ProductionCodexModelsAuthSource;

#[async_trait]
impl CodexModelsAuthSource for ProductionCodexModelsAuthSource {
    fn current_credentials(&self) -> anyhow::Result<Option<CodexCredentials>> {
        codex_auth::load_credentials().map_err(Into::into)
    }

    async fn fresh_credentials(&self) -> anyhow::Result<Option<CodexCredentials>> {
        codex_auth::fresh_credentials().await
    }

    async fn force_refresh(&self) -> anyhow::Result<Option<CodexCredentials>> {
        codex_auth::force_refresh().await
    }
}

/// Codex models transport and its provider-local cache policy.
#[derive(Clone, Debug)]
pub struct CodexModelsClient {
    http: reqwest::Client,
    cache_path: PathBuf,
    base_url: String,
    grok_version: String,
    client_version: String,
    cache_ttl: Duration,
    auth: Arc<dyn CodexModelsAuthSource>,
}

impl CodexModelsClient {
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::new(),
            cache_path: crate::util::grok_home::grok_home().join(CODEX_MODELS_CACHE_FILE),
            base_url: codex_inference_base_url(),
            grok_version: xai_grok_version::VERSION.to_owned(),
            client_version: codex_client_version(),
            cache_ttl: CODEX_MODELS_CACHE_TTL,
            auth: Arc::new(ProductionCodexModelsAuthSource),
        }
    }

    pub fn cache_path(&self) -> &Path {
        &self.cache_path
    }

    /// Use only a fresh cache for the current account and endpoint.
    pub fn load_fresh_cache(&self) -> Option<CodexModelsCatalog> {
        let credentials = self.auth.current_credentials().ok().flatten()?;
        self.load_fresh_cache_for(&credentials)
    }

    /// Fetch a live catalog, retrying once after a Codex-only 401 refresh.
    ///
    /// A matching cached catalog with an ETag turns this into a conditional
    /// request; a 304 revalidates the cache and extends its TTL without
    /// re-parsing a body.
    pub async fn fetch_and_cache(&self) -> anyhow::Result<Option<CodexModelsCatalog>> {
        let Some(mut credentials) = self.auth.fresh_credentials().await? else {
            return Ok(None);
        };
        // Freshness is irrelevant here: even a stale cache may be revalidated,
        // but only one recorded for this exact endpoint, version pair, and
        // account fingerprint.
        let cached = self.load_matching_cache(&credentials);

        let outcome = match self
            .fetch_once(
                &credentials,
                revalidation_etag(cached.as_ref(), &credentials).as_deref(),
            )
            .await
        {
            Ok(outcome) => outcome,
            Err(CodexModelsRequestError::Unauthorized) => {
                credentials = self
                    .auth
                    .force_refresh()
                    .await?
                    .ok_or_else(|| anyhow!("Codex login is no longer available"))?;
                // The fingerprint is rechecked against the refreshed
                // credentials so another account's ETag is never replayed.
                self.fetch_once(
                    &credentials,
                    revalidation_etag(cached.as_ref(), &credentials).as_deref(),
                )
                .await
                .map_err(CodexModelsRequestError::into_anyhow)?
            }
            Err(error) => return Err(error.into_anyhow()),
        };

        let catalog = match outcome {
            CodexModelsFetchOutcome::Fresh(catalog) => catalog,
            CodexModelsFetchOutcome::NotModified => {
                let Some(cache) = cached else {
                    // fetch_once only reports 304 for a conditional request,
                    // which requires the cache loaded above.
                    return Err(anyhow!(
                        "Codex models endpoint returned 304 without a cached catalog"
                    ));
                };
                cache.into_catalog()
            }
        };

        if self.catalog_matches_current_account(&catalog) {
            self.persist(&catalog, &credentials, Utc::now())?;
        } else {
            tracing::debug!("skipping Codex catalog cache write after account change");
        }
        Ok(Some(catalog))
    }

    pub async fn load_fresh_or_fetch(&self) -> anyhow::Result<Option<CodexModelsCatalog>> {
        if let Some(catalog) = self.load_fresh_cache() {
            return Ok(Some(catalog));
        }
        self.fetch_and_cache().await
    }

    /// Recheck the stable account identity before publishing a completed fetch.
    pub fn catalog_matches_current_account(&self, catalog: &CodexModelsCatalog) -> bool {
        self.auth
            .current_credentials()
            .ok()
            .flatten()
            .and_then(|credentials| account_fingerprint(&credentials))
            .is_some_and(|fingerprint| fingerprint == catalog.account_fingerprint)
    }

    /// Remove only the Codex catalog cache.
    pub fn invalidate_cache(&self) {
        match std::fs::remove_file(&self.cache_path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => tracing::warn!(
                path = %self.cache_path.display(),
                %error,
                "Codex models cache could not be removed"
            ),
        }
    }

    fn load_fresh_cache_for(&self, credentials: &CodexCredentials) -> Option<CodexModelsCatalog> {
        let cache = self.load_matching_cache(credentials)?;
        if !cache.is_fresh(self.cache_ttl) {
            return None;
        }
        Some(cache.into_catalog())
    }

    /// Load the cache entry bound to this endpoint, version pair, and account
    /// fingerprint, regardless of freshness. Callers decide whether to serve
    /// it directly (fresh) or revalidate it conditionally (any age).
    fn load_matching_cache(&self, credentials: &CodexCredentials) -> Option<CodexModelsCache> {
        let data = std::fs::read(&self.cache_path).ok()?;
        let cache: CodexModelsCache = serde_json::from_slice(&data).ok()?;
        let (base_origin, base_url) = self.cache_endpoint_identity().ok()?;

        if cache.grok_version != self.grok_version
            || cache.client_version != self.client_version
            || cache.base_origin != base_origin
            || cache.base_url != base_url
            || cache.account_fingerprint != account_fingerprint(credentials)?
        {
            return None;
        }
        Some(cache)
    }

    async fn fetch_once(
        &self,
        credentials: &CodexCredentials,
        if_none_match: Option<&str>,
    ) -> Result<CodexModelsFetchOutcome, CodexModelsRequestError> {
        let request_account = account_fingerprint(credentials).ok_or_else(|| {
            CodexModelsRequestError::Other(anyhow!(
                "Codex credentials have no stable account identity"
            ))
        })?;
        let url = self.models_url().map_err(CodexModelsRequestError::Other)?;
        let mut request = self
            .http
            .get(url)
            .timeout(CODEX_MODELS_REQUEST_TIMEOUT)
            .bearer_auth(&credentials.access_token)
            .header("originator", CODEX_ORIGINATOR)
            .header(USER_AGENT, self.codex_user_agent())
            .header("version", &self.client_version);
        if let Some(account_id) = credentials.account_id.as_deref() {
            request = request.header("ChatGPT-Account-ID", account_id);
        }
        if credentials.account_is_fedramp {
            request = request.header("X-OpenAI-Fedramp", "true");
        }
        if let Some(etag) = if_none_match {
            request = request.header(IF_NONE_MATCH, etag);
        }

        let response = request.send().await.map_err(|error| {
            CodexModelsRequestError::Other(anyhow!(error).context("Codex models request failed"))
        })?;
        if response.status() == StatusCode::UNAUTHORIZED {
            return Err(CodexModelsRequestError::Unauthorized);
        }
        if response.status() == StatusCode::NOT_MODIFIED {
            if if_none_match.is_some() {
                return Ok(CodexModelsFetchOutcome::NotModified);
            }
            return Err(CodexModelsRequestError::Other(anyhow!(
                "Codex models request returned 304 Not Modified to an unconditional request"
            )));
        }
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(CodexModelsRequestError::Other(anyhow!(
                "Codex models request returned {status}: {}",
                safe_error_excerpt(&body)
            )));
        }

        let etag = response
            .headers()
            .get(ETAG)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let body = response.bytes().await.map_err(|error| {
            CodexModelsRequestError::Other(
                anyhow!(error).context("Codex models response could not be read"),
            )
        })?;
        let wire: CodexModelsResponse = serde_json::from_slice(&body).map_err(|error| {
            CodexModelsRequestError::Other(anyhow!(error).context("Codex models response invalid"))
        })?;

        let mut models = wire
            .models
            .into_iter()
            .filter_map(convert_model)
            .collect::<Vec<_>>();
        models.sort_by_key(|model| model.priority);
        Ok(CodexModelsFetchOutcome::Fresh(CodexModelsCatalog {
            models,
            etag,
            account_fingerprint: request_account,
        }))
    }

    fn models_url(&self) -> anyhow::Result<Url> {
        let mut url = Url::parse(&self.base_url).context("Codex models base URL is invalid")?;
        let path = format!("{}/models", url.path().trim_end_matches('/'));
        url.set_path(&path);
        url.query_pairs_mut()
            .append_pair("client_version", &self.client_version);
        Ok(url)
    }

    fn cache_endpoint_identity(&self) -> anyhow::Result<(String, String)> {
        let url = Url::parse(&self.base_url).context("Codex models base URL is invalid")?;
        Ok((
            url.origin().ascii_serialization(),
            self.normalized_base_url(),
        ))
    }

    fn normalized_base_url(&self) -> String {
        self.base_url.trim_end_matches('/').to_owned()
    }

    fn codex_user_agent(&self) -> String {
        format!("{CODEX_ORIGINATOR}/{}", self.client_version)
    }

    fn persist(
        &self,
        catalog: &CodexModelsCatalog,
        credentials: &CodexCredentials,
        fetched_at: DateTime<Utc>,
    ) -> anyhow::Result<()> {
        let (base_origin, base_url) = self.cache_endpoint_identity()?;
        let cache = CodexModelsCache {
            fetched_at,
            grok_version: self.grok_version.clone(),
            client_version: self.client_version.clone(),
            base_origin,
            base_url,
            account_fingerprint: account_fingerprint(credentials)
                .ok_or_else(|| anyhow!("Codex credentials have no stable account identity"))?,
            etag: catalog.etag.clone(),
            models: catalog.models.clone(),
        };
        let data = serde_json::to_vec_pretty(&cache).context("serialize Codex models cache")?;
        let parent = self
            .cache_path
            .parent()
            .ok_or_else(|| anyhow!("Codex models cache path has no parent"))?;
        std::fs::create_dir_all(parent).context("create Codex models cache directory")?;

        let temporary_path = self.cache_path.with_file_name(format!(
            ".{}.{}.tmp",
            CODEX_MODELS_CACHE_FILE,
            std::process::id()
        ));
        let result = (|| -> anyhow::Result<()> {
            let file = crate::util::secure_file::open_secure_file(&temporary_path)?;
            let mut writer = std::io::BufWriter::new(file);
            writer.write_all(&data)?;
            writer.flush()?;
            writer
                .into_inner()
                .map_err(|error| error.into_error())?
                .sync_all()?;
            std::fs::rename(&temporary_path, &self.cache_path)
                .context("replace Codex models cache")?;
            crate::util::secure_file::ensure_owner_only_permissions(&self.cache_path)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temporary_path);
        }
        result
    }

    #[cfg(test)]
    pub(crate) fn for_test(
        cache_path: PathBuf,
        base_url: String,
        grok_version: String,
        client_version: String,
        cache_ttl: Duration,
        auth: Arc<dyn CodexModelsAuthSource>,
    ) -> Self {
        Self {
            http: reqwest::Client::new(),
            cache_path,
            base_url,
            grok_version,
            client_version,
            cache_ttl,
            auth,
        }
    }
}

impl Default for CodexModelsClient {
    fn default() -> Self {
        Self::new()
    }
}

/// Result of one Codex models request: a full catalog, or a 304 confirming
/// the conditional request's cached catalog is still current.
#[derive(Debug)]
enum CodexModelsFetchOutcome {
    Fresh(CodexModelsCatalog),
    NotModified,
}

/// The cached ETag, but only when the cache belongs to exactly the account
/// that will make the request. A refreshed credential set is rechecked so a
/// stale account's ETag can never be traded for a 304 on a new account.
fn revalidation_etag(
    cache: Option<&CodexModelsCache>,
    credentials: &CodexCredentials,
) -> Option<String> {
    let cache = cache?;
    if account_fingerprint(credentials)? != cache.account_fingerprint {
        return None;
    }
    cache
        .etag
        .as_deref()
        .map(str::trim)
        .filter(|etag| !etag.is_empty())
        .map(str::to_owned)
}

#[derive(Debug)]
enum CodexModelsRequestError {
    Unauthorized,
    Other(anyhow::Error),
}

impl CodexModelsRequestError {
    fn into_anyhow(self) -> anyhow::Error {
        match self {
            Self::Unauthorized => anyhow!("Codex rejected the OAuth token"),
            Self::Other(error) => error,
        }
    }
}

fn codex_inference_base_url() -> String {
    codex_auth::inference_base_url()
}

fn convert_model(wire: CodexWireModel) -> Option<CodexCatalogModel> {
    let slug = wire.slug.trim();
    if slug.is_empty() {
        return None;
    }
    let raw_context_window = wire
        .context_window
        .or(wire.max_context_window)
        .and_then(|value| u64::try_from(value).ok());
    let context_window = raw_context_window.and_then(|raw| {
        if wire.effective_context_window_percent <= 0 {
            return None;
        }
        let effective = raw.saturating_mul(wire.effective_context_window_percent as u64) / 100;
        (effective > 0).then_some(effective)
    });
    Some(CodexCatalogModel {
        slug: slug.to_owned(),
        display_name: if wire.display_name.trim().is_empty() {
            slug.to_owned()
        } else {
            wire.display_name.trim().to_owned()
        },
        description: wire
            .description
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty()),
        priority: wire.priority,
        visibility: wire.visibility,
        server_supported_in_api: wire.supported_in_api,
        supported_in_api: false,
        context_window,
        raw_context_window,
        effective_context_window_percent: wire.effective_context_window_percent,
        default_reasoning_level: wire
            .default_reasoning_level
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty()),
        supported_reasoning_levels: wire
            .supported_reasoning_levels
            .into_iter()
            .filter(|level| !level.effort.trim().is_empty())
            .collect(),
        supports_search_tool: wire.supports_search_tool,
        tool_mode: wire
            .tool_mode
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty()),
    })
}

fn account_fingerprint(credentials: &CodexCredentials) -> Option<String> {
    if credentials.account_id.is_none()
        && credentials.chatgpt_user_id.is_none()
        && credentials.email.is_none()
    {
        return None;
    }
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"grok-codex-model-cache-account-v1\0");
    hash_identity_component(&mut hasher, credentials.account_id.as_deref());
    hash_identity_component(&mut hasher, credentials.chatgpt_user_id.as_deref());
    hash_identity_component(&mut hasher, credentials.email.as_deref());
    hash_identity_component(&mut hasher, credentials.plan_type.as_deref());
    hasher.update(&[u8::from(credentials.is_workspace_account)]);
    hasher.update(&[u8::from(credentials.account_is_fedramp)]);
    Some(hasher.finalize().to_hex().to_string())
}

fn hash_identity_component(hasher: &mut blake3::Hasher, value: Option<&str>) {
    match value {
        Some(value) => {
            hasher.update(&[1]);
            hasher.update(&(value.len() as u64).to_be_bytes());
            hasher.update(value.as_bytes());
        }
        None => {
            hasher.update(&[0]);
        }
    }
}

fn safe_error_excerpt(body: &str) -> String {
    const LIMIT: usize = 512;
    let mut excerpt: String = body.chars().take(LIMIT).collect();
    if body.chars().count() > LIMIT {
        excerpt.push('…');
    }
    excerpt.replace(['\n', '\r'], " ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::{Query, State};
    use axum::http::{HeaderMap, StatusCode};
    use axum::response::{IntoResponse, Response};
    use axum::routing::get;
    use axum::Router;
    use serde_json::json;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;
    use tokio::net::TcpListener;
    use tokio::sync::Notify;

    fn credentials(token: &str, account: &str) -> CodexCredentials {
        CodexCredentials {
            access_token: token.to_owned(),
            account_id: Some(account.to_owned()),
            chatgpt_user_id: Some(format!("user-{account}")),
            email: Some(format!("{account}@example.com")),
            plan_type: Some("pro".to_owned()),
            is_workspace_account: false,
            account_is_fedramp: false,
        }
    }

    #[derive(Debug)]
    struct TestAuthSource {
        current: Mutex<Option<CodexCredentials>>,
        fresh: Option<CodexCredentials>,
        refreshed: Option<CodexCredentials>,
        force_calls: AtomicUsize,
    }

    #[async_trait]
    impl CodexModelsAuthSource for TestAuthSource {
        fn current_credentials(&self) -> anyhow::Result<Option<CodexCredentials>> {
            Ok(self.current.lock().unwrap().clone())
        }

        async fn fresh_credentials(&self) -> anyhow::Result<Option<CodexCredentials>> {
            Ok(self.fresh.clone())
        }

        async fn force_refresh(&self) -> anyhow::Result<Option<CodexCredentials>> {
            self.force_calls.fetch_add(1, Ordering::SeqCst);
            let credentials = self.refreshed.clone();
            *self.current.lock().unwrap() = credentials.clone();
            Ok(credentials)
        }
    }

    #[derive(Clone, Debug)]
    struct ObservedRequest {
        authorization: Option<String>,
        account_id: Option<String>,
        fedramp: Option<String>,
        originator: Option<String>,
        user_agent: Option<String>,
        version: Option<String>,
        if_none_match: Option<String>,
    }

    #[derive(Clone)]
    struct ServerState {
        observed: Arc<Mutex<Vec<ObservedRequest>>>,
        statuses: Arc<Mutex<VecDeque<StatusCode>>>,
        body: serde_json::Value,
        etag: Option<String>,
        gate: Option<(Arc<Notify>, Arc<Notify>)>,
    }

    async fn models_handler(
        State(state): State<ServerState>,
        Query(_query): Query<std::collections::HashMap<String, String>>,
        headers: HeaderMap,
    ) -> Response {
        let value = |name: &str| {
            headers
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned)
        };
        state.observed.lock().unwrap().push(ObservedRequest {
            authorization: value("authorization"),
            account_id: value("chatgpt-account-id"),
            fedramp: value("x-openai-fedramp"),
            originator: value("originator"),
            user_agent: value("user-agent"),
            version: value("version"),
            if_none_match: value("if-none-match"),
        });
        if let Some((started, release)) = &state.gate {
            started.notify_one();
            release.notified().await;
        }
        let status = state
            .statuses
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(StatusCode::OK);
        let mut headers = HeaderMap::new();
        if let Some(etag) = &state.etag {
            headers.insert(axum::http::header::ETAG, etag.parse().unwrap());
        }
        if status == StatusCode::NOT_MODIFIED {
            (status, headers).into_response()
        } else {
            (status, headers, axum::Json(state.body.clone())).into_response()
        }
    }

    async fn spawn_server(
        statuses: impl IntoIterator<Item = StatusCode>,
        body: serde_json::Value,
        gate: Option<(Arc<Notify>, Arc<Notify>)>,
    ) -> (String, Arc<Mutex<Vec<ObservedRequest>>>, tokio::task::JoinHandle<()>) {
        spawn_server_with_etag(statuses, body, None, gate).await
    }

    async fn spawn_server_with_etag(
        statuses: impl IntoIterator<Item = StatusCode>,
        body: serde_json::Value,
        etag: Option<&str>,
        gate: Option<(Arc<Notify>, Arc<Notify>)>,
    ) -> (String, Arc<Mutex<Vec<ObservedRequest>>>, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let observed = Arc::new(Mutex::new(Vec::new()));
        let state = ServerState {
            observed: observed.clone(),
            statuses: Arc::new(Mutex::new(statuses.into_iter().collect())),
            body,
            etag: etag.map(str::to_owned),
            gate,
        };
        let app = Router::new()
            .route("/codex/models", get(models_handler))
            .with_state(state);
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{address}/codex"), observed, server)
    }

    fn test_client(
        temp: &tempfile::TempDir,
        base_url: String,
        auth: Arc<dyn CodexModelsAuthSource>,
    ) -> CodexModelsClient {
        CodexModelsClient::for_test(
            temp.path().join(CODEX_MODELS_CACHE_FILE),
            base_url,
            "test-grok".to_owned(),
            "1.2.3".to_owned(),
            Duration::from_secs(300),
            auth,
        )
    }

    fn model_response() -> serde_json::Value {
        json!({
            "models": [{
                "slug": "gpt-5.6-sol",
                "display_name": "GPT-5.6 Sol",
                "description": "Live Codex model",
                "visibility": "list",
                "supported_in_api": true,
                "priority": 1,
                "context_window": 372000,
                "effective_context_window_percent": 95,
                "supports_search_tool": true,
                "tool_mode": "code_mode_only",
                "supported_reasoning_levels": [
                    {"effort": "low", "description": "Fast"},
                    {"effort": "medium", "description": "Balanced"}
                ]
            }, {
                "slug": "hidden-model",
                "display_name": "Hidden",
                "visibility": "hide",
                "priority": 2,
                "context_window": 200000
            }]
        })
    }

    #[tokio::test]
    async fn fetch_sends_codex_headers_and_caches_live_metadata() {
        let (base_url, observed, server) =
            spawn_server([StatusCode::OK], model_response(), None).await;
        let temp = tempfile::tempdir().unwrap();
        let auth = Arc::new(TestAuthSource {
            current: Mutex::new(Some(credentials("codex-token", "account-1"))),
            fresh: Some(credentials("codex-token", "account-1")),
            refreshed: None,
            force_calls: AtomicUsize::new(0),
        });
        let client = test_client(&temp, base_url, auth);

        let catalog = client.fetch_and_cache().await.unwrap().unwrap();
        server.abort();

        let requests = observed.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].authorization.as_deref(), Some("Bearer codex-token"));
        assert_eq!(requests[0].account_id.as_deref(), Some("account-1"));
        assert_eq!(requests[0].originator.as_deref(), Some(CODEX_ORIGINATOR));
        assert_eq!(requests[0].user_agent.as_deref(), Some("codex_cli_rs/1.2.3"));
        assert_eq!(requests[0].version.as_deref(), Some("1.2.3"));
        assert_eq!(catalog.models.len(), 2);
        assert!(catalog.is_authoritative());
        assert_eq!(catalog.models[0].context_window, Some(353400));
        assert!(catalog.models[0].supports_search_tool);
        assert_eq!(catalog.models[0].supported_in_api, false);
        assert!(client.cache_path().exists());
    }

    #[tokio::test]
    async fn unauthorized_refreshes_only_codex_credentials_once() {
        let (base_url, observed, server) =
            spawn_server([StatusCode::UNAUTHORIZED, StatusCode::OK], model_response(), None)
                .await;
        let temp = tempfile::tempdir().unwrap();
        let old = credentials("old-token", "account-1");
        let new = credentials("new-token", "account-1");
        let auth = Arc::new(TestAuthSource {
            current: Mutex::new(Some(old.clone())),
            fresh: Some(old),
            refreshed: Some(new),
            force_calls: AtomicUsize::new(0),
        });
        let client = test_client(&temp, base_url, auth.clone());

        assert!(client.fetch_and_cache().await.unwrap().is_some());
        server.abort();

        assert_eq!(auth.force_calls.load(Ordering::SeqCst), 1);
        let requests = observed.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].authorization.as_deref(), Some("Bearer old-token"));
        assert_eq!(requests[1].authorization.as_deref(), Some("Bearer new-token"));
    }

    #[tokio::test]
    async fn conditional_request_revalidates_with_etag_and_304_extends_ttl() {
        let (base_url, observed, server) = spawn_server_with_etag(
            [StatusCode::OK, StatusCode::NOT_MODIFIED],
            model_response(),
            Some("\"catalog-v1\""),
            None,
        )
        .await;
        let temp = tempfile::tempdir().unwrap();
        let account = credentials("codex-token", "account-1");
        let auth = Arc::new(TestAuthSource {
            current: Mutex::new(Some(account.clone())),
            fresh: Some(account.clone()),
            refreshed: None,
            force_calls: AtomicUsize::new(0),
        });
        let client = test_client(&temp, base_url, auth);

        let first = client.fetch_and_cache().await.unwrap().unwrap();
        assert_eq!(first.etag.as_deref(), Some("\"catalog-v1\""));

        // Age the cache past its TTL so only a successful 304 revalidation can
        // make it fresh again.
        client
            .persist(&first, &account, Utc::now() - ChronoDuration::hours(1))
            .unwrap();
        assert!(client.load_fresh_cache().is_none());

        let second = client.fetch_and_cache().await.unwrap().unwrap();
        server.abort();

        let requests = observed.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(
            requests[0].if_none_match, None,
            "the first request has no cache to revalidate"
        );
        assert_eq!(requests[1].if_none_match.as_deref(), Some("\"catalog-v1\""));
        assert_eq!(second.models, first.models);
        assert_eq!(second.etag.as_deref(), Some("\"catalog-v1\""));
        assert!(
            client.load_fresh_cache().is_some(),
            "a 304 must refresh the cache timestamp"
        );
    }

    #[tokio::test]
    async fn mismatched_account_cache_never_sends_if_none_match() {
        let (base_url, observed, server) = spawn_server_with_etag(
            [StatusCode::OK],
            model_response(),
            Some("\"catalog-v2\""),
            None,
        )
        .await;
        let temp = tempfile::tempdir().unwrap();
        let account_a = credentials("token-a", "account-a");
        let account_b = credentials("token-b", "account-b");
        let auth = Arc::new(TestAuthSource {
            current: Mutex::new(Some(account_b.clone())),
            fresh: Some(account_b.clone()),
            refreshed: None,
            force_calls: AtomicUsize::new(0),
        });
        let client = test_client(&temp, base_url, auth);
        let model = convert_model(serde_json::from_value(json!({
            "slug": "cached",
            "display_name": "Cached",
            "visibility": "list",
            "context_window": 100000
        })).unwrap()).unwrap();
        let stale_catalog = CodexModelsCatalog {
            models: vec![model],
            etag: Some("\"other-account-etag\"".to_owned()),
            account_fingerprint: account_fingerprint(&account_a).unwrap(),
        };
        client.persist(&stale_catalog, &account_a, Utc::now()).unwrap();

        let fetched = client.fetch_and_cache().await.unwrap().unwrap();
        server.abort();

        let requests = observed.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0].if_none_match, None,
            "another account's ETag must never be traded for a 304"
        );
        assert_eq!(
            fetched.account_fingerprint(),
            account_fingerprint(&account_b).unwrap()
        );
    }

    #[tokio::test]
    async fn unauthorized_retry_keeps_conditional_revalidation() {
        let (base_url, observed, server) = spawn_server_with_etag(
            [StatusCode::UNAUTHORIZED, StatusCode::NOT_MODIFIED],
            model_response(),
            Some("\"catalog-v1\""),
            None,
        )
        .await;
        let temp = tempfile::tempdir().unwrap();
        let old = credentials("old-token", "account-1");
        let new = credentials("new-token", "account-1");
        let auth = Arc::new(TestAuthSource {
            current: Mutex::new(Some(old.clone())),
            fresh: Some(old.clone()),
            refreshed: Some(new),
            force_calls: AtomicUsize::new(0),
        });
        let client = test_client(&temp, base_url, auth);
        let model = convert_model(serde_json::from_value(json!({
            "slug": "cached",
            "display_name": "Cached",
            "visibility": "list",
            "context_window": 100000
        })).unwrap()).unwrap();
        let cached_catalog = CodexModelsCatalog {
            models: vec![model],
            etag: Some("\"catalog-v1\"".to_owned()),
            account_fingerprint: account_fingerprint(&old).unwrap(),
        };
        client.persist(&cached_catalog, &old, Utc::now()).unwrap();

        let fetched = client.fetch_and_cache().await.unwrap().unwrap();
        server.abort();

        let requests = observed.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].if_none_match.as_deref(), Some("\"catalog-v1\""));
        assert_eq!(
            requests[1].if_none_match.as_deref(),
            Some("\"catalog-v1\""),
            "the post-refresh retry stays conditional for the same account"
        );
        assert_eq!(fetched.models, cached_catalog.models);
        assert_eq!(fetched.etag.as_deref(), Some("\"catalog-v1\""));
    }

    #[tokio::test]
    async fn account_switch_does_not_publish_in_flight_catalog() {
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let (base_url, _observed, server) = spawn_server(
            [StatusCode::OK],
            model_response(),
            Some((started.clone(), release.clone())),
        )
        .await;
        let temp = tempfile::tempdir().unwrap();
        let account_a = credentials("token-a", "account-a");
        let account_b = credentials("token-b", "account-b");
        let auth = Arc::new(TestAuthSource {
            current: Mutex::new(Some(account_a.clone())),
            fresh: Some(account_a),
            refreshed: None,
            force_calls: AtomicUsize::new(0),
        });
        let client = test_client(&temp, base_url, auth.clone());
        let fetching = {
            let client = client.clone();
            tokio::spawn(async move { client.fetch_and_cache().await })
        };
        started.notified().await;
        *auth.current.lock().unwrap() = Some(account_b);
        release.notify_one();

        let catalog = fetching.await.unwrap().unwrap().unwrap();
        server.abort();

        assert!(!client.catalog_matches_current_account(&catalog));
        assert!(!client.cache_path().exists());
    }

    #[test]
    fn cache_rejects_account_and_stale_entries() {
        let temp = tempfile::tempdir().unwrap();
        let account_a = credentials("token-a", "account-a");
        let auth = Arc::new(TestAuthSource {
            current: Mutex::new(Some(account_a.clone())),
            fresh: Some(account_a.clone()),
            refreshed: None,
            force_calls: AtomicUsize::new(0),
        });
        let client = test_client(&temp, "https://chatgpt.example/codex".to_owned(), auth.clone());
        let model = convert_model(serde_json::from_value(json!({
            "slug": "cached",
            "display_name": "Cached",
            "visibility": "list",
            "context_window": 100000
        })).unwrap()).unwrap();
        let catalog = CodexModelsCatalog {
            models: vec![model],
            etag: Some("etag".to_owned()),
            account_fingerprint: account_fingerprint(&account_a).unwrap(),
        };

        client.persist(&catalog, &account_a, Utc::now()).unwrap();
        assert!(client.load_fresh_cache().is_some());

        *auth.current.lock().unwrap() = Some(credentials("token-b", "account-b"));
        assert!(client.load_fresh_cache().is_none());

        *auth.current.lock().unwrap() = Some(account_a.clone());
        client
            .persist(&catalog, &account_a, Utc::now() - ChronoDuration::hours(1))
            .unwrap();
        assert!(client.load_fresh_cache().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn cache_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let account = credentials("token", "account");
        let auth = Arc::new(TestAuthSource {
            current: Mutex::new(Some(account.clone())),
            fresh: Some(account.clone()),
            refreshed: None,
            force_calls: AtomicUsize::new(0),
        });
        let client = test_client(&temp, "https://chatgpt.example/codex".to_owned(), auth);
        let model = convert_model(serde_json::from_value(json!({
            "slug": "cached",
            "display_name": "Cached",
            "visibility": "list"
        })).unwrap()).unwrap();
        let catalog = CodexModelsCatalog {
            models: vec![model],
            etag: None,
            account_fingerprint: account_fingerprint(&account).unwrap(),
        };
        client.persist(&catalog, &account, Utc::now()).unwrap();
        assert_eq!(
            std::fs::metadata(client.cache_path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn compatibility_version_normalizes() {
        assert_eq!(normalize_whole_semver(" v0.144.5-alpha+build"), Some("0.144.5".to_owned()));
        assert_eq!(normalize_whole_semver("nope"), None);
    }

    #[test]
    fn unknown_account_cannot_be_cached() {
        let credentials = CodexCredentials {
            access_token: "token".to_owned(),
            account_id: None,
            chatgpt_user_id: None,
            email: None,
            plan_type: None,
            is_workspace_account: false,
            account_is_fedramp: false,
        };
        assert!(account_fingerprint(&credentials).is_none());
    }
}
