//! Finding a release asset and downloading it, bounded and hashed.
//!
//! # Where requests go
//!
//! GitHub's API for a `release` source ([`GITHUB_API`]), and wherever the
//! release's asset URL leads; a `url` source's own address otherwise. Every
//! address — the first and each redirect — must be https, or plain http to
//! this machine only ([`crate::agent::catalogue::key_may_travel_to`]), so an
//! asset can never be fetched in clear from elsewhere. Redirects are followed
//! here, one at a time and at most [`MAX_REDIRECTS`], over the crate's shared
//! client that follows none by itself
//! ([`crate::runtime::http::no_redirect_client`]).
//!
//! Nothing about the operator is sent: no credential, no cookie, and a
//! User-Agent that names only the program ([`USER_AGENT`]), which GitHub's
//! API requires.
//!
//! # Bounds
//!
//! The release lookup: [`API_BOUND`] and [`MAX_API_BYTES`]. The download:
//! [`DOWNLOAD_BOUND`] for the whole transfer and [`MAX_DOWNLOAD_BYTES`] — or
//! the size the release declares, when it declares one — counted as the bytes
//! arrive, not taken from a header.

use std::io::Write as _;
use std::path::Path;
use std::time::Duration;

use serde_json::Value;
use sha2::{Digest as _, Sha256};

use super::{Target, ToolEntry};

/// GitHub's REST API root.
pub(crate) const GITHUB_API: &str = "https://api.github.com";
/// What every request says it is: the program, nothing about who runs it.
const USER_AGENT: &str = "incode";
/// How long the release lookup may take.
const API_BOUND: Duration = Duration::from_secs(30);
/// The most of a release description read.
const MAX_API_BYTES: usize = 4 * 1024 * 1024;
/// How long one download may take, start to finish.
const DOWNLOAD_BOUND: Duration = Duration::from_secs(15 * 60);
/// The largest asset downloaded.
pub(crate) const MAX_DOWNLOAD_BYTES: u64 = 512 * 1024 * 1024;
/// The most redirects followed from one address.
const MAX_REDIRECTS: usize = 5;

/// What to fetch, and what the release says about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Resolved {
    /// The release tag (or the version a `url` source named).
    pub(crate) version: String,
    /// The asset's file name.
    pub(crate) asset: String,
    /// Where to download it.
    pub(crate) url: String,
    /// Where it came from, for the manifest.
    pub(crate) source: String,
    /// The sha256 the release publishes for it, lowercase hex, if any.
    pub(crate) digest: Option<String>,
    /// The size the release declares, if any.
    pub(crate) size: Option<u64>,
}

/// The asset `entry` names for `target`, in `release`'s latest release or the
/// one tagged with its version.
pub(crate) fn resolve_github(
    api: &str,
    release: &str,
    entry: &ToolEntry,
    target: Target,
) -> Result<Resolved, String> {
    let version = entry.version();
    let path = if version == "latest" {
        "releases/latest".to_string()
    } else {
        format!("releases/tags/{version}")
    };
    let url = format!("{}/repos/{release}/{path}", api.trim_end_matches('/'));
    let body = get_bounded(
        &url,
        "application/vnd.github+json",
        API_BOUND,
        MAX_API_BYTES,
    )
    .map_err(|why| format!("the release of {release} ({version}) could not be read: {why}"))?;
    let json: Value = serde_json::from_slice(&body)
        .map_err(|e| format!("the release of {release} did not parse: {e}"))?;
    let tag = json
        .get("tag_name")
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty())
        .ok_or_else(|| format!("the release of {release} ({version}) names no tag"))?
        .to_string();
    let wanted = entry.asset_for(target, &tag)?;
    let assets = json
        .get("assets")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let Some(asset) = assets
        .iter()
        .find(|a| a.get("name").and_then(Value::as_str) == Some(wanted.as_str()))
    else {
        let names: Vec<&str> = assets
            .iter()
            .filter_map(|a| a.get("name").and_then(Value::as_str))
            .take(40)
            .collect();
        return Err(format!(
            "release {tag} of {release} has no asset `{wanted}` (it has: {})",
            if names.is_empty() {
                "none".to_string()
            } else {
                names.join(", ")
            }
        ));
    };
    let download = asset
        .get("browser_download_url")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("asset `{wanted}` of {release} {tag} has no download address"))?
        .to_string();
    let digest = asset
        .get("digest")
        .and_then(Value::as_str)
        .and_then(|d| d.strip_prefix("sha256:"))
        .map(str::to_ascii_lowercase);
    Ok(Resolved {
        version: tag,
        asset: wanted,
        url: download,
        source: release.to_string(),
        digest,
        size: asset.get("size").and_then(Value::as_u64),
    })
}

/// One GET, following redirects under the address rule, bounded in time and
/// size. `Err` names what went wrong, never a header.
fn get_bounded(url: &str, accept: &str, bound: Duration, cap: usize) -> Result<Vec<u8>, String> {
    let url = url.to_string();
    let accept = accept.to_string();
    crate::runtime::http::run_async(async move {
        let fetched = tokio::time::timeout(bound, async {
            let response = follow(&url, &accept).await?;
            let mut body = Vec::new();
            let mut response = response;
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|e| format!("reading the answer failed: {}", chain(&e)))?
            {
                if body.len() + chunk.len() > cap {
                    return Err(format!("the answer is larger than {cap} bytes"));
                }
                body.extend_from_slice(&chunk);
            }
            Ok(body)
        })
        .await;
        fetched.unwrap_or_else(|_| Err(format!("no answer within {}s", bound.as_secs())))
    })
}

/// The response at the end of `url`'s redirects, each address checked first.
async fn follow(url: &str, accept: &str) -> Result<reqwest::Response, String> {
    let client = crate::runtime::http::no_redirect_client()?;
    let mut at = url.to_string();
    for _ in 0..=MAX_REDIRECTS {
        crate::agent::catalogue::key_may_travel_to(&at)
            .map_err(|why| format!("refused to fetch it: {why}"))?;
        let response = client
            .get(&at)
            .header(reqwest::header::USER_AGENT, USER_AGENT)
            .header(reqwest::header::ACCEPT, accept)
            .send()
            .await
            .map_err(|e| format!("the request failed: {}", chain(&e)))?;
        let status = response.status();
        if status.is_redirection() {
            let next = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| format!("a redirect ({status}) with no address"))?;
            at = response
                .url()
                .join(next)
                .map_err(|e| format!("a redirect to an address that is not valid: {e}"))?
                .to_string();
            continue;
        }
        if !status.is_success() {
            return Err(format!("the server answered {status}"));
        }
        return Ok(response);
    }
    Err(format!("more than {MAX_REDIRECTS} redirects"))
}

/// An error and every cause under it, on one line.
fn chain(error: &dyn std::error::Error) -> String {
    crate::runtime::http::error_chain(error)
}

/// Download `url` into `to`, returning the sha256 of what arrived, lowercase
/// hex. At most `declared` bytes when the release declares a size (and never
/// more than [`MAX_DOWNLOAD_BYTES`]); the whole transfer within
/// [`DOWNLOAD_BOUND`].
pub(crate) fn download(url: &str, declared: Option<u64>, to: &Path) -> Result<String, String> {
    let cap = declared.map_or(MAX_DOWNLOAD_BYTES, |d| d.min(MAX_DOWNLOAD_BYTES));
    if declared.is_some_and(|d| d > MAX_DOWNLOAD_BYTES) {
        return Err(format!(
            "the asset is {} bytes, over the {MAX_DOWNLOAD_BYTES} a download may be",
            declared.unwrap_or_default()
        ));
    }
    let mut file =
        std::fs::File::create(to).map_err(|e| format!("could not write {}: {e}", to.display()))?;
    let url = url.to_string();
    let hashed = crate::runtime::http::run_async(async move {
        let fetched = tokio::time::timeout(DOWNLOAD_BOUND, async {
            let mut response = follow(&url, "application/octet-stream").await?;
            let mut hasher = Sha256::new();
            let mut total: u64 = 0;
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|e| format!("the download broke off: {}", chain(&e)))?
            {
                total += chunk.len() as u64;
                if total > cap {
                    return Err(format!(
                        "the download ran past {cap} bytes, more than it may be; it was stopped"
                    ));
                }
                hasher.update(&chunk);
                file.write_all(&chunk)
                    .map_err(|e| format!("the download could not be written: {e}"))?;
            }
            file.flush()
                .map_err(|e| format!("the download could not be written: {e}"))?;
            let digest = hasher.finalize();
            Ok(digest
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>())
        })
        .await;
        fetched.unwrap_or_else(|_| {
            Err(format!(
                "the download did not finish within {}s; it was stopped",
                DOWNLOAD_BOUND.as_secs()
            ))
        })
    });
    if hashed.is_err() {
        let _ = std::fs::remove_file(to);
    }
    hashed
}
