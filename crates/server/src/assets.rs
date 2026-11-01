//! Where key images come from: local files, plain URLs, Home Assistant paths (`/api/...`), or
//! objects in an S3 bucket (Garage), plus the background fetcher. The renderer never blocks on any of this: it asks
//! [`Fetcher::spawn`] for a download and gets the decoded image back through a callback.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use image::RgbaImage;
use rusty_s3::{Bucket, Credentials, S3Action, UrlStyle};
use tokio::runtime::Handle;

const FETCH_TIMEOUT: Duration = Duration::from_secs(15);
/// Sanity cap on a downloaded image.
const MAX_IMAGE_BYTES: u64 = 16 * 1024 * 1024;
/// Presigned URLs are used immediately, so they only need to outlive the request.
const PRESIGN_TTL: Duration = Duration::from_secs(300);

/// Credentials and location of the bucket that holds icon/image objects (path-style addressing,
/// since Garage has no per-bucket DNS).
#[derive(Debug, Clone)]
pub struct S3Config {
    pub endpoint: String,
    pub bucket: String,
    pub region: String,
    pub access_key_id: String,
    pub secret_access_key: String,
}

impl S3Config {
    /// Reads `SDC_ASSETS_S3_*`. `None` if the assets bucket is not (fully) configured.
    pub fn from_env() -> Option<Self> {
        let var = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        let endpoint = var("SDC_ASSETS_S3_ENDPOINT");
        let bucket = var("SDC_ASSETS_S3_BUCKET");
        let key = var("SDC_ASSETS_S3_ACCESS_KEY_ID");
        let secret = var("SDC_ASSETS_S3_SECRET_ACCESS_KEY");
        match (endpoint, bucket, key, secret) {
            (Some(endpoint), Some(bucket), Some(access_key_id), Some(secret_access_key)) => {
                Some(Self {
                    endpoint,
                    bucket,
                    region: var("SDC_ASSETS_S3_REGION").unwrap_or_else(|| "garage".into()),
                    access_key_id,
                    secret_access_key,
                })
            }
            (None, None, None, None) => None,
            _ => {
                tracing::warn!(
                    "S3 assets disabled: set all of SDC_ASSETS_S3_ENDPOINT, _BUCKET, \
                     _ACCESS_KEY_ID and _SECRET_ACCESS_KEY"
                );
                None
            }
        }
    }

    /// A presigned, path-style GET URL for `key`, e.g. `http://host:3900/<bucket>/<key>?X-Amz-...`.
    pub fn presign_get(&self, key: &str) -> Result<String> {
        let endpoint = self
            .endpoint
            .parse()
            .with_context(|| format!("invalid SDC_ASSETS_S3_ENDPOINT {:?}", self.endpoint))?;
        let bucket = Bucket::new(
            endpoint,
            UrlStyle::Path,
            self.bucket.clone(),
            self.region.clone(),
        )
        .map_err(|e| anyhow!("invalid S3 bucket config: {e}"))?;
        let creds = Credentials::new(&self.access_key_id, &self.secret_access_key);
        let url = bucket
            .get_object(Some(&creds), key.trim_start_matches('/'))
            .sign(PRESIGN_TTL);
        Ok(url.to_string())
    }
}

/// Where to reach Home Assistant's HTTP API (for `/api/...` image paths such as media artwork).
#[derive(Debug, Clone)]
pub struct HaApi {
    /// `http://host:8123`, no trailing slash.
    pub base_url: String,
    pub token: String,
}

impl HaApi {
    pub fn new(url: &str, token: &str) -> Self {
        let u = url.trim().trim_end_matches('/');
        let u = u.strip_suffix("/api/websocket").unwrap_or(u);
        let u = if let Some(rest) = u.strip_prefix("wss://") {
            format!("https://{rest}")
        } else if let Some(rest) = u.strip_prefix("ws://") {
            format!("http://{rest}")
        } else {
            u.to_string()
        };
        Self {
            base_url: u,
            token: token.to_string(),
        }
    }

    /// Reads `HA_URL` / `HA_TOKEN`; `None` if either is unset.
    pub fn from_env() -> Option<Self> {
        let url = std::env::var("HA_URL").ok().filter(|s| !s.is_empty())?;
        let token = std::env::var("HA_TOKEN").ok().filter(|s| !s.is_empty())?;
        Some(Self::new(&url, &token))
    }
}

/// Prefix of the cache keys of Home Assistant sources.
const HA_KEY: &str = "ha:";

/// True for the cache key of a Home Assistant source (see [`Assets::remote_key`]).
pub fn is_ha_key(key: &str) -> bool {
    key.starts_with(HA_KEY)
}

/// A resolved image reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    File(PathBuf),
    Url(String),
    /// Object key in the assets bucket.
    S3(String),
    /// Path on the Home Assistant server, e.g. `/api/media_player_proxy/...`.
    Ha(String),
}

/// What to request to download a remote source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Download {
    pub url: String,
    /// Sent as `Authorization: Bearer <token>`.
    pub bearer: Option<String>,
}

/// Resolves the strings used for `icon` / `image` to a [`Source`].
pub struct Assets {
    base_dir: PathBuf,
    s3: Option<S3Config>,
    ha: Option<HaApi>,
}

impl Assets {
    /// `base_dir` is the directory local paths are relative to (the config's directory).
    pub fn new(base_dir: PathBuf, s3: Option<S3Config>) -> Self {
        Self {
            base_dir,
            s3,
            ha: None,
        }
    }

    /// Enables `/api/...` sources.
    pub fn with_ha(mut self, ha: Option<HaApi>) -> Self {
        self.ha = ha;
        self
    }

    /// `http(s)://` -> URL; `/api/...` -> Home Assistant (an error if it is not configured);
    /// else an existing file relative to the config dir; else, if S3 assets are configured, an
    /// object key in the bucket.
    pub fn resolve(&self, name: &str) -> Result<Source> {
        if name.starts_with("http://") || name.starts_with("https://") {
            return Ok(Source::Url(name.to_string()));
        }
        if name.starts_with("/api/") {
            if self.ha.is_none() {
                bail!("{} needs HA_URL and HA_TOKEN", redact(name));
            }
            return Ok(Source::Ha(name.to_string()));
        }
        let path = self.base_dir.join(name);
        if path.is_file() {
            return Ok(Source::File(path));
        }
        if self.s3.is_some() {
            return Ok(Source::S3(name.to_string()));
        }
        bail!(
            "{} not found and no S3 assets bucket is configured",
            path.display()
        )
    }

    /// Stable cache key for a remote source (`None` for local files).
    pub fn remote_key(&self, src: &Source) -> Option<String> {
        match src {
            Source::File(_) => None,
            Source::Url(u) => Some(u.clone()),
            Source::Ha(p) => Some(format!("{HA_KEY}{p}")),
            Source::S3(k) => Some(format!(
                "s3://{}/{}",
                self.s3.as_ref().map_or("", |s| s.bucket.as_str()),
                k.trim_start_matches('/')
            )),
        }
    }

    /// How to download a remote source (presigned URL for S3, bearer token for Home Assistant).
    pub fn download(&self, src: &Source) -> Result<Download> {
        let plain = |url| Download { url, bearer: None };
        match src {
            Source::Url(u) => Ok(plain(u.clone())),
            Source::S3(k) => self
                .s3
                .as_ref()
                .ok_or_else(|| anyhow!("S3 assets are not configured"))?
                .presign_get(k)
                .map(plain),
            Source::Ha(p) => {
                let ha = self
                    .ha
                    .as_ref()
                    .ok_or_else(|| anyhow!("Home Assistant is not configured"))?;
                Ok(Download {
                    url: format!("{}{p}", ha.base_url),
                    bearer: Some(ha.token.clone()),
                })
            }
            Source::File(p) => bail!("{} is a local file", p.display()),
        }
    }
}

/// What a finished background fetch reports: the cache key and the decoded image or an error.
pub type FetchResult = (String, Result<RgbaImage, String>);
type OnDone = dyn Fn(FetchResult) + Send + Sync;

/// Runs blocking downloads on the tokio blocking pool and reports each result via a callback.
#[derive(Clone)]
pub struct Fetcher {
    handle: Handle,
    on_done: Arc<OnDone>,
}

impl Fetcher {
    pub fn new(handle: Handle, on_done: impl Fn(FetchResult) + Send + Sync + 'static) -> Self {
        Self {
            handle,
            on_done: Arc::new(on_done),
        }
    }

    /// Starts downloading in the background; `key` identifies it in the result.
    pub fn spawn(&self, key: String, dl: Download) {
        let on_done = self.on_done.clone();
        self.handle.spawn_blocking(move || {
            let res = fetch_image(&dl).map_err(|e| format!("{e:#}"));
            on_done((key, res));
        });
    }
}

/// Downloads and decodes an image. Blocking.
pub fn fetch_image(dl: &Download) -> Result<RgbaImage> {
    let url = dl.url.as_str();
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(FETCH_TIMEOUT))
        .build()
        .new_agent();
    let mut req = agent.get(url);
    if let Some(token) = &dl.bearer {
        req = req.header("Authorization", format!("Bearer {token}"));
    }
    let mut resp = req.call().map_err(|e| match e {
        // Presigned URLs (and HA artwork) carry secrets in the query string: keep it out of logs.
        ureq::Error::StatusCode(code) => anyhow!("HTTP {code} from {}", redact(url)),
        other => anyhow!("{other}"),
    })?;
    let bytes = resp
        .body_mut()
        .with_config()
        .limit(MAX_IMAGE_BYTES)
        .read_to_vec()
        .context("reading response")?;
    Ok(image::load_from_memory(&bytes)
        .context("decoding image")?
        .to_rgba8())
}

/// `url` without its query string.
pub fn redact(url: &str) -> &str {
    url.split('?').next().unwrap_or(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s3() -> S3Config {
        S3Config {
            endpoint: "http://garage.garage.svc.cluster.local:3900".into(),
            bucket: "stream-deck-icons".into(),
            region: "garage".into(),
            access_key_id: "GKtest".into(),
            secret_access_key: "secret".into(),
        }
    }

    fn tmp(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("sdc-assets-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn presigned_url_is_path_style() {
        let url = s3().presign_get("/tv/popcorn.png").unwrap();
        assert!(
            url.starts_with(
                "http://garage.garage.svc.cluster.local:3900/stream-deck-icons/tv/popcorn.png?"
            ),
            "{url}"
        );
        assert!(url.contains("X-Amz-Algorithm=AWS4-HMAC-SHA256"));
        assert!(url.contains("X-Amz-Credential=GKtest%2F"));
        assert!(url.contains("%2Fgarage%2Fs3%2Faws4_request"));
        assert!(url.contains("X-Amz-Signature="));
        assert!(!url.contains("secret"));
    }

    #[test]
    fn resolution_order() {
        let dir = tmp("resolve");
        std::fs::write(dir.join("local.png"), b"x").unwrap();
        let with_s3 = Assets::new(dir.clone(), Some(s3()));
        // URL wins, then an existing local file, then the bucket
        assert_eq!(
            with_s3.resolve("https://example.com/a.png").unwrap(),
            Source::Url("https://example.com/a.png".into())
        );
        assert_eq!(
            with_s3.resolve("local.png").unwrap(),
            Source::File(dir.join("local.png"))
        );
        assert_eq!(
            with_s3.resolve("tv/popcorn.png").unwrap(),
            Source::S3("tv/popcorn.png".into())
        );
        // without a bucket a missing file is an error
        let no_s3 = Assets::new(dir.clone(), None);
        assert!(no_s3.resolve("tv/popcorn.png").is_err());
        assert!(no_s3.resolve("local.png").is_ok());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn remote_keys_and_download_urls() {
        let a = Assets::new(PathBuf::from("."), Some(s3()));
        let obj = Source::S3("tv/popcorn.png".into());
        assert_eq!(
            a.remote_key(&obj).as_deref(),
            Some("s3://stream-deck-icons/tv/popcorn.png")
        );
        assert!(a
            .download(&obj)
            .unwrap()
            .url
            .contains("/stream-deck-icons/tv/popcorn.png?"));
        let u = Source::Url("https://x/y.png".into());
        assert_eq!(a.remote_key(&u).as_deref(), Some("https://x/y.png"));
        assert_eq!(
            a.download(&u).unwrap(),
            Download {
                url: "https://x/y.png".into(),
                bearer: None
            }
        );
        assert_eq!(a.remote_key(&Source::File("f".into())), None);
    }

    #[test]
    fn ha_paths_resolve_to_the_ha_server_with_the_token() {
        let ha = HaApi::new("http://ha.local:8123/", "tok");
        let a = Assets::new(PathBuf::from("."), None).with_ha(Some(ha));
        let path = "/api/media_player_proxy/media_player.tv?token=abc&cache=1";
        let src = a.resolve(path).unwrap();
        assert_eq!(src, Source::Ha(path.into()));
        assert!(is_ha_key(&a.remote_key(&src).unwrap()));
        assert_eq!(
            a.download(&src).unwrap(),
            Download {
                url: format!("http://ha.local:8123{path}"),
                bearer: Some("tok".into())
            }
        );
        // not configured: an error source; other absolute paths are not HA's
        let none = Assets::new(PathBuf::from("."), None);
        assert!(none.resolve(path).is_err());
        assert!(a.resolve("/apix/y.png").is_err());
        // websocket-style URLs are accepted too
        assert_eq!(
            HaApi::new("wss://ha.example/api/websocket", "t").base_url,
            "https://ha.example"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn ha_fetch_sends_the_bearer_token() {
        use axum::{http::HeaderMap, routing::get, Router};
        let app = Router::new().route(
            "/api/media_player_proxy/x",
            get(|h: HeaderMap| async move {
                if h.get("authorization").and_then(|v| v.to_str().ok()) != Some("Bearer secret") {
                    return (axum::http::StatusCode::UNAUTHORIZED, Vec::new());
                }
                let mut png = std::io::Cursor::new(Vec::new());
                RgbaImage::from_pixel(2, 2, image::Rgba([1, 2, 3, 255]))
                    .write_to(&mut png, image::ImageFormat::Png)
                    .unwrap();
                (axum::http::StatusCode::OK, png.into_inner())
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let a = |token: &str| {
            Assets::new(PathBuf::from("."), None).with_ha(Some(HaApi::new(&base, token)))
        };
        let path = "/api/media_player_proxy/x?token=t";
        let dl = |a: &Assets| a.download(&a.resolve(path).unwrap()).unwrap();
        let (good, bad) = (dl(&a("secret")), dl(&a("wrong")));
        let img = tokio::task::spawn_blocking(move || fetch_image(&good))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(img.get_pixel(0, 0).0, [1, 2, 3, 255]);
        let err = tokio::task::spawn_blocking(move || fetch_image(&bad))
            .await
            .unwrap()
            .unwrap_err();
        assert!(format!("{err}").contains("401"), "{err}");
        assert!(!format!("{err}").contains("token=t"), "query is redacted");
    }

    #[test]
    fn redacts_query() {
        assert_eq!(
            redact("http://h/p.png?X-Amz-Signature=abc"),
            "http://h/p.png"
        );
        assert_eq!(redact("http://h/p.png"), "http://h/p.png");
    }
}
