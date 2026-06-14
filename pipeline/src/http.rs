use std::path::Path;
use std::time::Duration;
use anyhow::{anyhow, Context, Result};
use tracing::{debug, info, warn};

/// Exponential-backoff retry policy. All fields are CLI-configurable; see `cli::Cli`.
#[derive(Debug, Clone)]
pub struct RetryConfig {
    pub max_attempts: u32,
    pub base_delay:   Duration,
    pub max_delay:    Duration,
    pub factor:       f64,
}

impl RetryConfig {
    pub fn new(max_attempts: u32, base_ms: u64, max_ms: u64, factor: f64) -> Self {
        Self {
            max_attempts,
            base_delay: Duration::from_millis(base_ms),
            max_delay:  Duration::from_millis(max_ms),
            factor,
        }
    }
}

/// Shared HTTP client with configurable retry logic for transient errors.
#[derive(Clone)]
pub struct Client {
    inner:  reqwest::Client,
    retry:  RetryConfig,
}

impl Client {
    pub fn new(retry: RetryConfig) -> Self {
        Self {
            inner: reqwest::Client::builder()
                .timeout(Duration::from_secs(60))
                .build()
                .expect("failed to build HTTP client"),
            retry,
        }
    }

    /// GET a URL as raw bytes, retrying on transient failures.
    pub async fn get_bytes(&self, url: &str) -> Result<Vec<u8>> {
        self.retry(url, |c, u| async move {
            Ok(c.inner.get(u).send().await?.error_for_status()?.bytes().await?.to_vec())
        })
        .await
    }

    /// GET the last `suffix_bytes` bytes of a URL via an HTTP Range request.
    /// Uses `bytes=-N` syntax (RFC 7233 suffix range), which is well-supported by S3.
    pub async fn get_range_bytes_suffix(&self, url: &str, suffix_bytes: u64) -> Result<Vec<u8>> {
        let range_value = format!("bytes=-{suffix_bytes}");
        self.retry(url, move |c, u| {
            let rv = range_value.clone();
            async move {
                Ok(c.inner
                    .get(&u)
                    .header(reqwest::header::RANGE, rv)
                    .send()
                    .await?
                    .error_for_status()?
                    .bytes()
                    .await?
                    .to_vec())
            }
        })
        .await
    }

    /// GET a URL as a UTF-8 string, retrying on transient failures.
    pub async fn get_text(&self, url: &str) -> Result<String> {
        self.retry(url, |c, u| async move {
            Ok(c.inner.get(u).send().await?.error_for_status()?.text().await?)
        })
        .await
    }

    /// Stream-download `url` to `dest`, logging progress every 50 MB.
    /// Uses a no-body-read-timeout client so large PBF files (hundreds of MB) don't time out.
    pub async fn download_to_file(&self, url: &str, dest: &Path) -> Result<u64> {
        use tokio::io::AsyncWriteExt;

        // No response-body timeout — large files can take many minutes on a slow link.
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| anyhow!("failed to build download client: {e}"))?;

        let mut response = client.get(url).send().await?.error_for_status()?;
        let total = response.content_length();
        let total_mb = total.map(|b| b / 1_048_576);

        info!(url, dest = %dest.display(), total_mb, "downloading");

        let mut file = tokio::fs::File::create(dest).await
            .with_context(|| format!("failed to create {}", dest.display()))?;

        let mut downloaded: u64 = 0;
        let mut last_logged: u64 = 0;

        while let Some(chunk) = response.chunk().await? {
            file.write_all(&chunk).await?;
            downloaded += chunk.len() as u64;

            if downloaded.saturating_sub(last_logged) >= 50 * 1_048_576 {
                let pct = total.map(|t| format!(" ({:.0}%)", downloaded as f64 / t as f64 * 100.0))
                               .unwrap_or_default();
                info!(
                    downloaded_mb = downloaded / 1_048_576,
                    total_mb,
                    progress = %format!("{} MB{pct}", downloaded / 1_048_576),
                    "downloading"
                );
                last_logged = downloaded;
            }
        }

        file.flush().await?;
        info!(total_mb = downloaded / 1_048_576, dest = %dest.display(), "download complete");
        Ok(downloaded)
    }

    async fn retry<F, Fut, T>(&self, url: &str, f: F) -> Result<T>
    where
        F: Fn(Self, String) -> Fut,
        Fut: std::future::Future<Output = Result<T>>,
    {
        let mut delay = self.retry.base_delay;
        let max       = self.retry.max_attempts;

        for attempt in 1..=max {
            debug!(attempt, url, "HTTP GET");
            match f(self.clone(), url.to_string()).await {
                Ok(v) => return Ok(v),
                Err(e) if attempt < max && is_transient(&e) => {
                    warn!(
                        attempt,
                        max_attempts = max,
                        url,
                        error = %e,
                        delay_ms = delay.as_millis(),
                        "transient HTTP error, retrying"
                    );
                    tokio::time::sleep(delay).await;
                    delay = (delay.mul_f64(self.retry.factor)).min(self.retry.max_delay);
                }
                Err(e) => return Err(e),
            }
        }
        Err(anyhow!("all {max} attempts failed for {url}"))
    }
}

fn is_transient(err: &anyhow::Error) -> bool {
    if let Some(re) = err.downcast_ref::<reqwest::Error>() {
        if re.is_timeout() || re.is_connect() { return true; }
        if let Some(status) = re.status() {
            return status.as_u16() == 429 || status.is_server_error();
        }
    }
    false
}
