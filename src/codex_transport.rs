use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use codex_api::{
    ApiError, Compression, ImageBackground, ImageEditRequest, ImageGenerationRequest, ImageQuality,
    ImageUrl, ImagesClient, ReqwestTransport, ResponseStream, ResponsesClient,
};
use codex_backend_client::Client as BackendClient;
use codex_http_client::{ClientRouteClass, HttpClientFactory, OutboundProxyPolicy, TransportError};
use codex_login::{
    AuthCredentialsStoreMode, AuthKeyringBackendKind, AuthManager, AuthRouteConfig, default_client,
};
use codex_model_provider::{SharedModelProvider, create_model_provider};
use codex_model_provider_info::ModelProviderInfo;
use codex_models_manager::manager::{RefreshStrategy, SharedModelsManager};
use codex_protocol::auth::AuthMode;
use http::{HeaderMap, StatusCode};
use serde_json::Value;
use tokio::sync::Mutex;

use crate::catalog::{Catalog, save_cached_catalog};
use crate::error::{BridgeError, Result};
use crate::paths::AppPaths;
use crate::usage::{UsageCache, UsageSnapshot};

const CHATGPT_BASE_URL: &str = "https://chatgpt.com/backend-api/";
const IMAGE_MODEL: &str = "gpt-image-2.5-sunburst";
const USAGE_CACHE_TTL: Duration = Duration::from_secs(60);

pub struct CodexTransport {
    auth_manager: Arc<AuthManager>,
    provider: SharedModelProvider,
    models_manager: SharedModelsManager,
    http_factory: HttpClientFactory,
    catalog_cache: PathBuf,
    auth_refresh_lock: PathBuf,
    usage_cache: Mutex<UsageCache>,
}

impl CodexTransport {
    pub async fn connect(paths: &AppPaths) -> Result<Self> {
        std::fs::create_dir_all(&paths.state_dir).map_err(|source| BridgeError::Write {
            path: paths.state_dir.clone(),
            source,
        })?;
        let http_factory = HttpClientFactory::new(OutboundProxyPolicy::RespectSystemProxy);
        let auth_manager = Arc::new(
            AuthManager::new(
                paths.codex_home.clone(),
                false,
                AuthCredentialsStoreMode::Auto,
                None,
                None,
                AuthKeyringBackendKind::default(),
                AuthRouteConfig::from_http_client_factory(http_factory.clone()),
            )
            .await,
        );
        let auth = auth_manager.auth().await.ok_or_else(|| {
            BridgeError::Authentication(
                "ChatGPT login required; run the installed Codex login flow first".to_string(),
            )
        })?;
        ensure_subscription_auth_mode(auth.api_auth_mode())?;

        let provider = create_model_provider(
            ModelProviderInfo::create_openai_provider(None),
            Some(auth_manager.clone()),
        );
        let models_manager = provider.models_manager(paths.codex_home.clone(), None);

        Ok(Self {
            auth_manager,
            provider,
            models_manager,
            http_factory,
            catalog_cache: paths.catalog_cache.clone(),
            auth_refresh_lock: paths.state_dir.join("auth-refresh.lock"),
            usage_cache: Mutex::new(UsageCache::new(USAGE_CACHE_TTL)),
        })
    }

    pub async fn refresh_catalog(&self) -> Result<Catalog> {
        let response = self
            .models_manager
            .raw_model_catalog(RefreshStrategy::Online, self.http_factory.clone())
            .await;
        let catalog = Catalog::from_codex(response.models)?;
        save_cached_catalog(&self.catalog_cache, &catalog)?;
        Ok(catalog)
    }

    pub async fn stream(&self, body: Value) -> Result<ResponseStream> {
        let mut retry = UnauthorizedRetryPolicy::default();
        loop {
            match self.open_stream(body.clone()).await {
                Ok(stream) => return Ok(stream),
                Err(error) => {
                    let status = api_error_status(&error);
                    let usage_error = if status == Some(StatusCode::TOO_MANY_REQUESTS) {
                        self.refresh_rate_limits(true).await.err()
                    } else {
                        None
                    };
                    if !status.is_some_and(|status| retry.should_retry(status.as_u16(), false)) {
                        let message = usage_error.map_or_else(
                            || error.to_string(),
                            |usage_error| {
                                format!(
                                    "{error}; additionally failed to refresh usage after 429: {usage_error}"
                                )
                            },
                        );
                        return Err(BridgeError::CodexTransport(message));
                    }
                    self.refresh_auth_once().await?;
                }
            }
        }
    }

    pub async fn refresh_auth_once(&self) -> Result<()> {
        let lock = acquire_auth_lock(self.auth_refresh_lock.clone()).await?;
        let mut recovery = self.auth_manager.unauthorized_recovery();
        let mut changed = false;
        while recovery.has_next() {
            let result = recovery
                .next()
                .await
                .map_err(|error| BridgeError::Authentication(error.to_string()))?;
            if result.auth_state_changed() == Some(true) {
                changed = true;
                break;
            }
        }
        drop(lock);
        if changed {
            Ok(())
        } else {
            Err(BridgeError::Authentication(
                "Codex could not refresh the ChatGPT session after an unauthorized response"
                    .to_string(),
            ))
        }
    }

    pub async fn rate_limits(&self) -> Result<UsageSnapshot> {
        self.refresh_rate_limits(false).await
    }

    pub async fn generate_image(
        &self,
        prompt: String,
        referenced_image_paths: Vec<PathBuf>,
    ) -> Result<Vec<u8>> {
        let request = image_request(prompt, referenced_image_paths)?;
        let mut retry = UnauthorizedRetryPolicy::default();
        loop {
            match self.open_image(request.clone()).await {
                Ok(bytes) => return Ok(bytes),
                Err(error)
                    if api_error_status(&error)
                        .is_some_and(|status| retry.should_retry(status.as_u16(), false)) =>
                {
                    self.refresh_auth_once().await?
                }
                Err(error) => return Err(BridgeError::CodexTransport(error.to_string())),
            }
        }
    }

    async fn open_stream(&self, body: Value) -> std::result::Result<ResponseStream, ApiError> {
        let api_provider = self
            .provider
            .api_provider()
            .await
            .map_err(|error| ApiError::Stream(error.to_string()))?;
        let api_auth = self
            .provider
            .api_auth()
            .await
            .map_err(|error| ApiError::Stream(error.to_string()))?;
        let request_url = api_provider.url_for_path("/responses");
        let http = default_client::create_client_for_route_async(
            self.http_factory.clone(),
            request_url,
            ClientRouteClass::Api,
        )
        .await
        .map_err(|error| ApiError::Stream(error.to_string()))?;
        ResponsesClient::new(
            ReqwestTransport::from_http_client(http),
            api_provider,
            api_auth,
        )
        .stream(body, HeaderMap::new(), Compression::None, None)
        .await
    }

    async fn open_image(&self, request: ImageRequest) -> std::result::Result<Vec<u8>, ApiError> {
        let api_provider = self
            .provider
            .api_provider()
            .await
            .map_err(|error| ApiError::Stream(error.to_string()))?;
        let api_auth = self
            .provider
            .api_auth()
            .await
            .map_err(|error| ApiError::Stream(error.to_string()))?;
        let request_url = api_provider.url_for_path("images/generations");
        let http = default_client::create_client_for_route_async(
            self.http_factory.clone(),
            request_url,
            ClientRouteClass::Api,
        )
        .await
        .map_err(|error| ApiError::Stream(error.to_string()))?;
        let client = ImagesClient::new(
            ReqwestTransport::from_http_client(http),
            api_provider,
            api_auth,
        );
        let (response, _) = match request {
            ImageRequest::Generate(request) => client.generate(&request, HeaderMap::new()).await?,
            ImageRequest::Edit(request) => client.edit(&request, HeaderMap::new()).await?,
        };
        let data = response.data.into_iter().next().ok_or_else(|| {
            ApiError::Stream("image generation returned no image data".to_string())
        })?;
        STANDARD.decode(data.b64_json).map_err(|error| {
            ApiError::Stream(format!("image generation returned invalid base64: {error}"))
        })
    }

    async fn refresh_rate_limits(&self, force: bool) -> Result<UsageSnapshot> {
        let mut cache = self.usage_cache.lock().await;
        let now = Instant::now();
        if !force && !cache.should_refresh(now) {
            return cache.snapshot().cloned().ok_or_else(|| {
                BridgeError::CodexTransport("usage cache is unexpectedly empty".to_string())
            });
        }

        let auth = self
            .auth_manager
            .auth()
            .await
            .ok_or_else(|| BridgeError::Authentication("ChatGPT login required".to_string()))?;
        ensure_subscription_auth_mode(auth.api_auth_mode())?;
        let api_auth = self
            .provider
            .api_auth()
            .await
            .map_err(|error| BridgeError::CodexTransport(error.to_string()))?;
        let mut backend = BackendClient::new(CHATGPT_BASE_URL, self.http_factory.clone())
            .with_user_agent(format!("claude-gpt/{}", env!("CARGO_PKG_VERSION")))
            .with_auth_provider(api_auth);
        if let Some(account_id) = auth.get_account_id() {
            backend = backend.with_chatgpt_account_id(account_id);
        }
        if auth.is_fedramp_account() {
            backend = backend.with_fedramp_routing_header();
        }
        let limits = backend
            .get_rate_limits_many()
            .await
            .map_err(|error| BridgeError::CodexTransport(error.to_string()))?;
        let snapshot = UsageSnapshot::new(limits);
        cache.record(snapshot.clone(), now);
        Ok(snapshot)
    }
}

#[derive(Clone)]
enum ImageRequest {
    Generate(ImageGenerationRequest),
    Edit(ImageEditRequest),
}

fn image_request(prompt: String, referenced_image_paths: Vec<PathBuf>) -> Result<ImageRequest> {
    if referenced_image_paths.is_empty() {
        return Ok(ImageRequest::Generate(ImageGenerationRequest {
            prompt,
            background: Some(ImageBackground::Auto),
            model: IMAGE_MODEL.to_string(),
            n: None,
            quality: Some(ImageQuality::Auto),
            size: Some("auto".to_string()),
        }));
    }
    if referenced_image_paths.len() > 5 {
        return Err(BridgeError::InvalidRequest {
            path: "referenced_image_paths".to_string(),
            message: "at most five reference images are supported".to_string(),
        });
    }
    let images = referenced_image_paths
        .iter()
        .map(|path| image_url_from_path(path))
        .collect::<Result<Vec<_>>>()?;
    Ok(ImageRequest::Edit(ImageEditRequest {
        images,
        prompt,
        background: Some(ImageBackground::Auto),
        model: IMAGE_MODEL.to_string(),
        n: None,
        quality: Some(ImageQuality::Auto),
        size: Some("auto".to_string()),
    }))
}

fn image_url_from_path(path: &Path) -> Result<ImageUrl> {
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| extension.to_ascii_lowercase());
    let mime_type = match extension.as_deref() {
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        Some("gif") => "image/gif",
        _ => {
            return Err(BridgeError::InvalidRequest {
                path: path.display().to_string(),
                message: "reference images must be PNG, JPEG, WebP, or GIF files".to_string(),
            });
        }
    };
    let bytes = std::fs::read(path).map_err(|source| BridgeError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(ImageUrl {
        image_url: format!("data:{mime_type};base64,{}", STANDARD.encode(bytes)),
    })
}

pub fn ensure_subscription_auth_mode(mode: AuthMode) -> Result<()> {
    if mode == AuthMode::Chatgpt {
        Ok(())
    } else {
        Err(BridgeError::Authentication(format!(
            "ChatGPT login required; API keys and non-subscription auth are disabled (found {mode})"
        )))
    }
}

#[derive(Debug, Default)]
pub struct UnauthorizedRetryPolicy {
    attempted: bool,
}

impl UnauthorizedRetryPolicy {
    pub fn should_retry(&mut self, status: u16, stream_started: bool) -> bool {
        if status != StatusCode::UNAUTHORIZED.as_u16() || stream_started || self.attempted {
            return false;
        }
        self.attempted = true;
        true
    }
}

fn api_error_status(error: &ApiError) -> Option<StatusCode> {
    match error {
        ApiError::Api { status, .. } | ApiError::Transport(TransportError::Http { status, .. }) => {
            Some(*status)
        }
        _ => None,
    }
}

async fn acquire_auth_lock(path: PathBuf) -> Result<File> {
    tokio::task::spawn_blocking(move || lock_file(&path))
        .await
        .map_err(|error| BridgeError::Authentication(format!("auth lock task failed: {error}")))?
}

fn lock_file(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)
        .map_err(|source| BridgeError::Write {
            path: path.to_path_buf(),
            source,
        })?;
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
    if result == 0 {
        Ok(file)
    } else {
        Err(BridgeError::Authentication(format!(
            "failed to lock {}: {}",
            path.display(),
            std::io::Error::last_os_error()
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::{ImageRequest, image_request};

    #[test]
    fn image_requests_use_sunburst_for_generation_and_editing() {
        let generated = image_request("a paper airplane".to_string(), vec![]).unwrap();
        let temporary_directory = tempfile::tempdir().unwrap();
        let reference = temporary_directory.path().join("reference.png");
        std::fs::write(&reference, b"png").unwrap();
        let edited = image_request("make it red".to_string(), vec![reference]).unwrap();

        match generated {
            ImageRequest::Generate(request) => {
                assert_eq!(request.model, "gpt-image-2.5-sunburst");
            }
            ImageRequest::Edit(_) => panic!("expected a generation request"),
        }
        match edited {
            ImageRequest::Edit(request) => {
                assert_eq!(request.model, "gpt-image-2.5-sunburst");
            }
            ImageRequest::Generate(_) => panic!("expected an edit request"),
        }
    }
}
