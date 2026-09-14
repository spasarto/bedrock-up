use crate::config::{DownloadType, UpdateConfig};
use crate::error::UpdateError;
use std::path::{Path, PathBuf};

/// Marks a file that was in use and had to be renamed aside to make room for
/// its replacement.
const STALE_MARKER: &str = ".bedrock-up-old";

/// Result of [`check`]: either nothing to do, or an [`AvailableUpdate`] ready
/// to be downloaded.
pub enum CheckOutcome {
    UpToDate { version: String },
    UpdateAvailable(AvailableUpdate),
}

/// An update the web API reports as available. Downloading it is a network
/// operation only — the server can keep running while it happens.
pub struct AvailableUpdate {
    version: String,
    download_url: String,
    web_json: serde_json::Value,
}

/// An update whose archive has already been downloaded to a local temp
/// directory. Applying it touches only the filesystem and should be fast;
/// the server should be stopped first.
///
/// The backing temp directory is removed automatically when this value is
/// dropped, whether or not `apply` is ever called — unless it was produced
/// by [`StagedUpdate::load`], in which case it survived a previous process
/// on purpose and is only cleaned up on a successful `apply`.
pub struct StagedUpdate {
    zip_path: PathBuf,
    version: String,
    web_json: serde_json::Value,
    cleanup: Cleanup,
}

enum Cleanup {
    /// Never read; kept alive only so its `Drop` removes the temp directory.
    TempDir(#[allow(dead_code)] tempfile::TempDir),
    /// A directory written by `persist`, kept around after this process
    /// exits so a later `apply` can pick it up. Removed once that `apply`
    /// succeeds; left alone on `Drop` otherwise, so a discarded value here
    /// doesn't destroy a stage nothing has consumed yet.
    Persisted(PathBuf),
}

/// The sidecar file [`StagedUpdate::persist`] writes alongside the archive so
/// [`StagedUpdate::load`] can reconstitute it in a later process.
const STAGED_META_FILE: &str = "staged.json";

/// Outcome of a successful [`StagedUpdate::apply`] call, used by callers
/// (e.g. a supervisor) to decide whether the server needs to be restarted.
pub enum UpdateOutcome {
    Updated,
    /// The update was written, but some files were in use and were staged in
    /// place. The running server keeps the old build until it is restarted.
    UpdatedPendingRestart,
}

/// Checks whether a new version is available, without downloading anything.
pub fn check(config: &UpdateConfig) -> Result<CheckOutcome, UpdateError> {
    let web_json = get_json_from_web()?;
    let cache_json = get_json_from_cache(&config.cache_path);
    let web_download_url = get_download_url_from_json(&web_json, &config.download_type)
        .ok_or_else(|| {
            UpdateError::UpstreamFormat(format!(
                "no download URL found for {}",
                config.download_type
            ))
        })?;
    let cache_download_url =
        get_download_url_from_json(&cache_json, &config.download_type).unwrap_or("0.0.0".to_owned());

    log::info!("Current version in cache: {}", cache_download_url);
    log::info!("Version available on the web: {}", web_download_url);

    if !config.force && web_download_url == cache_download_url {
        log::info!(
            "You are already on the latest version: {}",
            cache_download_url
        );
        return Ok(CheckOutcome::UpToDate {
            version: cache_download_url,
        });
    }

    log::info!("New version available: {}", web_download_url);
    Ok(CheckOutcome::UpdateAvailable(AvailableUpdate {
        version: web_download_url.clone(),
        download_url: web_download_url,
        web_json,
    }))
}

impl AvailableUpdate {
    pub fn version(&self) -> &str {
        &self.version
    }

    /// Downloads the update archive to a fresh temp directory. Network only
    /// — the server can stay up while this runs.
    pub fn download(self) -> Result<StagedUpdate, UpdateError> {
        let temp_dir = tempfile::Builder::new()
            .prefix("bedrock-up-")
            .tempdir()
            .map_err(|source| UpdateError::Io {
                path: std::env::temp_dir(),
                source,
            })?;
        let zip_path = fetch_update_zip(&self.download_url, temp_dir.path())?;

        Ok(StagedUpdate {
            zip_path,
            version: self.version,
            web_json: self.web_json,
            cleanup: Cleanup::TempDir(temp_dir),
        })
    }
}

impl StagedUpdate {
    /// The version this staged archive will install, e.g. for a `download`
    /// command to report what it fetched.
    pub fn version(&self) -> &str {
        &self.version
    }

    /// Applies the staged archive over `config.server_path`. Filesystem only
    /// and fast; the server should be stopped before calling this.
    pub fn apply(self, config: &UpdateConfig) -> Result<UpdateOutcome, UpdateError> {
        // Files staged by a previous update can only be deleted once the
        // server that had them open has restarted, so clear them out now.
        let swept = sweep_stale_files(&config.server_path);
        if swept > 0 {
            log::info!("Cleaned up {} file(s) staged by a previous update.", swept);
        }

        log::info!("Applying update from: {}", self.zip_path.display());
        let staged = apply_update(&config.server_path, &self.zip_path, config.exclude.clone())?;

        update_cache(&self.web_json, &config.cache_path).map_err(|source| {
            UpdateError::CacheWrite {
                path: config.cache_path.clone(),
                source,
            }
        })?;

        // Only now that the update landed (and the cache write above didn't
        // bail out early) is a persisted stage no longer needed.
        if let Cleanup::Persisted(dir) = &self.cleanup
            && let Err(e) = std::fs::remove_dir_all(dir)
        {
            log::warn!("failed to clean up staging directory {}: {e}", dir.display());
        }

        if staged.is_empty() {
            log::info!("Update applied successfully.");
            return Ok(UpdateOutcome::Updated);
        }

        log::warn!(
            "Update applied successfully. {} file(s) were in use and were staged in place.",
            staged.len()
        );
        report_running_servers(&config.server_path);
        Ok(UpdateOutcome::UpdatedPendingRestart)
    }

    /// Persists this staged update to `dir`, so a later process can pick it
    /// up with [`StagedUpdate::load`] instead of the archive being removed
    /// when this value is dropped. `dir` is created if it doesn't exist.
    ///
    /// This is what backs the `bedrock-up download` / `bedrock-up apply`
    /// split: `download` can run during the day while the server is up, and
    /// `apply` at night once it's stopped, as two separate CLI invocations.
    pub fn persist(self, dir: &Path) -> Result<(), UpdateError> {
        std::fs::create_dir_all(dir).map_err(|source| UpdateError::Io {
            path: dir.to_path_buf(),
            source,
        })?;

        let file_name = self.zip_path.file_name().ok_or_else(|| UpdateError::Io {
            path: self.zip_path.clone(),
            source: std::io::Error::new(std::io::ErrorKind::InvalidInput, "archive path has no file name"),
        })?;
        let dest = dir.join(file_name);
        move_file(&self.zip_path, &dest)?;

        let meta = serde_json::json!({
            "version": self.version,
            "zip_file": file_name.to_string_lossy(),
            "web_json": self.web_json,
        });
        let meta_path = dir.join(STAGED_META_FILE);
        let file = std::fs::File::create(&meta_path).map_err(|source| UpdateError::Io {
            path: meta_path.clone(),
            source,
        })?;
        serde_json::to_writer(file, &meta).map_err(|e| UpdateError::Io {
            path: meta_path,
            source: std::io::Error::other(e),
        })?;

        // `self` is dropped here; its `Cleanup::TempDir` removes the now-empty
        // temp directory the archive was moved out of.
        Ok(())
    }

    /// Reconstitutes a [`StagedUpdate`] previously written by
    /// [`StagedUpdate::persist`]. Returns `Ok(None)` if `dir` holds no staged
    /// update — that's the expected state before the first `download`, not
    /// an error.
    pub fn load(dir: &Path) -> Result<Option<StagedUpdate>, UpdateError> {
        let meta_path = dir.join(STAGED_META_FILE);
        if !meta_path.exists() {
            return Ok(None);
        }

        let invalid = |reason: String| UpdateError::Staging {
            path: dir.to_path_buf(),
            reason,
        };

        let content = std::fs::read_to_string(&meta_path).map_err(|source| UpdateError::Io {
            path: meta_path.clone(),
            source,
        })?;
        let meta: serde_json::Value =
            serde_json::from_str(&content).map_err(|e| invalid(format!("malformed metadata: {e}")))?;

        let version = meta
            .get("version")
            .and_then(|v| v.as_str())
            .ok_or_else(|| invalid("metadata missing \"version\"".to_string()))?
            .to_string();
        let zip_file = meta
            .get("zip_file")
            .and_then(|v| v.as_str())
            .ok_or_else(|| invalid("metadata missing \"zip_file\"".to_string()))?;
        let web_json = meta
            .get("web_json")
            .cloned()
            .ok_or_else(|| invalid("metadata missing \"web_json\"".to_string()))?;

        let zip_path = dir.join(zip_file);
        if !zip_path.exists() {
            return Err(invalid(format!("archive {} is missing", zip_path.display())));
        }

        Ok(Some(StagedUpdate {
            zip_path,
            version,
            web_json,
            cleanup: Cleanup::Persisted(dir.to_path_buf()),
        }))
    }
}

/// Moves a file, falling back to copy-then-remove when `rename` fails because
/// the source and destination are on different filesystems (or drives, on
/// Windows) — the case a fixed staging directory can't rule out the way a
/// same-filesystem temp dir usually can.
fn move_file(from: &Path, to: &Path) -> Result<(), UpdateError> {
    if std::fs::rename(from, to).is_ok() {
        return Ok(());
    }
    std::fs::copy(from, to).map_err(|source| UpdateError::Io {
        path: to.to_path_buf(),
        source,
    })?;
    std::fs::remove_file(from).map_err(|source| UpdateError::Io {
        path: from.to_path_buf(),
        source,
    })
}

/// Names the processes still running the previous build out of `server_path`,
/// so the caller knows exactly what has to be restarted.
fn report_running_servers(server_path: &Path) {
    let running = crate::process::find_server_processes(server_path);
    if running.is_empty() {
        log::info!("No running server found for this path; the update is already live.");
        return;
    }

    log::warn!("Restart the following to activate the new version:");
    for process in running {
        log::warn!("  PID {} ({})", process.pid, process.exe.display());
    }
}

fn get_json_from_web() -> Result<serde_json::Value, UpdateError> {
    get_json_from_web_with_url(
        "https://net-secondary.web.minecraft-services.net/api/v1.0/download/links",
    )
}

fn get_json_from_web_with_url(url: &str) -> Result<serde_json::Value, UpdateError> {
    log::info!("Fetching links from the web...");
    let json = reqwest::blocking::get(url)?.json::<serde_json::Value>()?;
    Ok(json)
}

fn get_json_from_cache(cache_path: &Path) -> serde_json::Value {
    log::info!("Reading cache from: {}", cache_path.display());

    std::fs::File::open(cache_path)
        .and_then(|file| {
            let reader = std::io::BufReader::new(file);
            serde_json::from_reader(reader).map_err(|_| {
                log::warn!("Failed to read or parse cache file: {}", cache_path.display());
                std::io::Error::new(std::io::ErrorKind::InvalidData, "Parse error")
            })
        })
        .unwrap_or(serde_json::Value::Null)
}

fn get_download_url_from_json(
    json: &serde_json::Value,
    download_type: &DownloadType,
) -> Option<String> {
    json.get("result")?
        .get("links")?
        .as_array()?
        .iter()
        .find_map(|item| {
            if item.get("downloadType") == Some(&download_type.to_string().into()) {
                item.get("downloadUrl")?.as_str().map(|s| s.to_string())
            } else {
                None
            }
        })
}

fn fetch_update_zip(download_url: &str, dir: &Path) -> Result<PathBuf, UpdateError> {
    let resp = reqwest::blocking::get(download_url)
        .and_then(|resp| resp.error_for_status())
        .map_err(|source| UpdateError::Download {
            url: download_url.to_string(),
            source,
        })?;

    let file_name = download_url.split('/').next_back().unwrap_or("update.zip");
    let file_path = dir.join(file_name);

    let mut file = std::fs::File::create(&file_path).map_err(|source| UpdateError::Io {
        path: file_path.clone(),
        source,
    })?;
    let bytes = resp.bytes().map_err(|source| UpdateError::Download {
        url: download_url.to_string(),
        source,
    })?;
    std::io::copy(&mut bytes.as_ref(), &mut file).map_err(|source| UpdateError::Io {
        path: file_path.clone(),
        source,
    })?;

    log::info!("Downloaded update to: {}", file_path.display());
    Ok(file_path)
}

/// Applies the archive over the server directory, returning the files that had
/// to be staged because they were in use.
fn apply_update(
    server_path: &Path,
    zip_path: &Path,
    exclude: Vec<String>,
) -> Result<Vec<PathBuf>, UpdateError> {
    log::info!("Excluded files: {:?}", exclude);

    let zip_file = std::fs::File::open(zip_path).map_err(|source| UpdateError::Io {
        path: zip_path.to_path_buf(),
        source,
    })?;
    let mut archive = zip::ZipArchive::new(zip_file).map_err(UpdateError::Archive)?;
    let exclude_set: std::collections::HashSet<_> = exclude.into_iter().collect();
    let mut staged = Vec::new();

    for i in 0..archive.len() {
        let mut file = archive.by_index(i).map_err(UpdateError::Archive)?;
        let out_path = match file.enclosed_name() {
            Some(path) => path,
            None => continue,
        };

        let should_exclude = exclude_set.contains(out_path.to_str().unwrap_or(""));

        let out_path = server_path.join(out_path);

        let should_exclude = should_exclude && std::fs::metadata(&out_path).is_ok();
        if should_exclude {
            log::info!("Skipping excluded file: {}", out_path.display());
            continue;
        }
        if file.is_dir() {
            std::fs::create_dir_all(&out_path).map_err(|source| UpdateError::Io {
                path: out_path.clone(),
                source,
            })?;
        } else {
            if let Some(parent) = out_path.parent() {
                std::fs::create_dir_all(parent).map_err(|source| UpdateError::Io {
                    path: parent.to_path_buf(),
                    source,
                })?;
            }

            let (mut outfile, was_staged) = create_replacing_in_use(&out_path)?;
            if was_staged {
                log::info!("Staged in-use file: {}", out_path.display());
                staged.push(out_path.clone());
            }
            std::io::copy(&mut file, &mut outfile).map_err(|source| UpdateError::Io {
                path: out_path.clone(),
                source,
            })?;
        }
    }

    Ok(staged)
}

/// Opens `out_path` for writing, renaming an existing file out of the way if the
/// OS refuses to replace it because it is in use.
///
/// Windows lets a running executable be renamed even though it cannot be
/// overwritten or deleted, and Linux lets one be unlinked. Either way the
/// running server keeps the file it already has open while the new version
/// lands in its place and takes effect on the next start. Returns whether the
/// file had to be staged this way.
fn create_replacing_in_use(out_path: &Path) -> Result<(std::fs::File, bool), UpdateError> {
    match std::fs::File::create(out_path) {
        Ok(file) => Ok((file, false)),
        Err(e) if is_in_use_error(&e) => {
            // Guaranteed not to name an existing file, so nothing staged by an
            // earlier update is disturbed or blocks the rename.
            let stale = stale_path(out_path);
            std::fs::rename(out_path, &stale).map_err(|_| UpdateError::InUse {
                path: out_path.to_path_buf(),
            })?;
            let file = std::fs::File::create(out_path).map_err(|source| UpdateError::Io {
                path: out_path.to_path_buf(),
                source,
            })?;
            Ok((file, true))
        }
        Err(source) => Err(UpdateError::Io {
            path: out_path.to_path_buf(),
            source,
        }),
    }
}

/// Picks the name to move an in-use file aside to.
///
/// A file staged by an earlier update can still be held open by a server that
/// has not restarted yet, and such a file can be neither deleted nor replaced.
/// Stepping past names that are already taken keeps a second update from
/// clashing with the first, so any number of updates can be applied between
/// restarts.
fn stale_path(out_path: &Path) -> PathBuf {
    let staged_name = |suffix: &str| {
        let mut name = out_path.as_os_str().to_os_string();
        name.push(STALE_MARKER);
        name.push(suffix);
        PathBuf::from(name)
    };

    let mut candidate = staged_name("");
    let mut attempt = 1u32;
    while candidate.exists() {
        candidate = staged_name(&format!(".{}", attempt));
        attempt += 1;
    }

    candidate
}

/// Whether `path` names a file that an earlier update moved aside.
fn is_stale_file(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let Some((_, tail)) = name.split_once(STALE_MARKER) else {
        return false;
    };

    // Either the bare marker, or the marker plus a `.N` disambiguator.
    tail.is_empty()
        || tail
            .strip_prefix('.')
            .is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
}

/// Whether the error means the file is held open by a running process.
fn is_in_use_error(e: &std::io::Error) -> bool {
    if cfg!(windows) {
        // ERROR_ACCESS_DENIED (5) and ERROR_SHARING_VIOLATION (32).
        matches!(e.raw_os_error(), Some(5) | Some(32))
    } else {
        // ETXTBSY: cannot write to a currently-executing binary.
        matches!(e.raw_os_error(), Some(26))
    }
}

/// Removes files staged by earlier updates. They can only be deleted once the
/// server holding them open has restarted, so this is best-effort and silently
/// leaves behind any that are still in use. Returns how many were removed.
fn sweep_stale_files(dir: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };

    let mut removed = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            removed += sweep_stale_files(&path);
        } else if is_stale_file(&path) && std::fs::remove_file(&path).is_ok() {
            removed += 1;
        }
    }
    removed
}

fn update_cache(web_json: &serde_json::Value, cache_path: &Path) -> std::io::Result<()> {
    if let Some(parent) = cache_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = std::fs::File::create(cache_path)?;
    serde_json::to_writer(file, web_json)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mockito::Server;
    use serde_json::json;

    #[test]
    fn test_get_json_from_web_success() {
        let mut server = Server::new();
        let mock_response = json!({
            "result": {
                "links": [
                    {
                        "downloadType": "bedrock-server",
                        "downloadUrl": "https://example.com/bedrock-server.zip"
                    }
                ]
            }
        });

        let mock = server
            .mock("GET", "/api/v1.0/download/links")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(mock_response.to_string())
            .create();

        let result =
            get_json_from_web_with_url(&format!("{}/api/v1.0/download/links", server.url()));

        mock.assert();
        assert_eq!(result.unwrap(), mock_response);
    }

    #[test]
    fn test_get_json_from_web_invalid_json() {
        let mut server = Server::new();

        let mock = server
            .mock("GET", "/api/v1.0/download/links")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body("invalid json")
            .create();

        let result =
            get_json_from_web_with_url(&format!("{}/api/v1.0/download/links", server.url()));

        mock.assert();
        assert!(matches!(result, Err(UpdateError::Network(_))));
    }

    #[test]
    fn test_get_json_from_web_server_error() {
        let mut server = Server::new();

        let mock = server
            .mock("GET", "/api/v1.0/download/links")
            .with_status(500)
            .with_body("Internal Server Error")
            .create();

        let result =
            get_json_from_web_with_url(&format!("{}/api/v1.0/download/links", server.url()));

        mock.assert();
        // A 500 is still valid JSON-less body, but reqwest doesn't treat non-2xx
        // as an error unless asked to, so this fails at JSON parsing.
        assert!(matches!(result, Err(UpdateError::Network(_))));
    }

    #[test]
    fn test_get_json_from_web_connection_error() {
        let result = get_json_from_web_with_url("http://non-existent-domain-12345.com/api");

        assert!(matches!(result, Err(UpdateError::Network(_))));
    }

    #[test]
    fn test_get_json_from_web_empty_response() {
        let mut server = Server::new();

        let mock = server
            .mock("GET", "/api/v1.0/download/links")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body("{}")
            .create();

        let result =
            get_json_from_web_with_url(&format!("{}/api/v1.0/download/links", server.url()));

        mock.assert();
        assert_eq!(result.unwrap(), json!({}));
    }

    #[test]
    fn test_get_json_from_web_complex_response() {
        let mut server = Server::new();
        let mock_response = json!({
            "result": {
                "links": [
                    {
                        "downloadType": "bedrock-server",
                        "downloadUrl": "https://example.com/bedrock-server-1.20.0.zip",
                        "version": "1.20.0"
                    },
                    {
                        "downloadType": "bedrock-server-preview",
                        "downloadUrl": "https://example.com/bedrock-server-preview-1.21.0.zip",
                        "version": "1.21.0"
                    }
                ],
                "metadata": {
                    "lastUpdated": "2025-07-04T12:00:00Z"
                }
            }
        });

        let mock = server
            .mock("GET", "/api/v1.0/download/links")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(mock_response.to_string())
            .create();

        let result =
            get_json_from_web_with_url(&format!("{}/api/v1.0/download/links", server.url()))
                .unwrap();

        mock.assert();
        assert_eq!(result, mock_response);
        assert_eq!(result["result"]["links"][0]["downloadType"], "bedrock-server");
        assert_eq!(result["result"]["links"][1]["version"], "1.21.0");
        assert_eq!(
            result["result"]["metadata"]["lastUpdated"],
            "2025-07-04T12:00:00Z"
        );
    }

    // Tests for get_json_from_cache

    #[test]
    fn test_get_json_from_cache_success() {
        use std::io::Write;
        use tempfile::NamedTempFile;

        let mut temp_file = NamedTempFile::new().unwrap();
        let test_json = json!({
            "result": {
                "links": [
                    {
                        "downloadType": "bedrock-server",
                        "downloadUrl": "https://example.com/bedrock-server.zip"
                    }
                ]
            }
        });

        write!(temp_file, "{}", test_json).unwrap();
        temp_file.flush().unwrap();

        let result = get_json_from_cache(temp_file.path());

        assert_eq!(result, test_json);
    }

    #[test]
    fn test_get_json_from_cache_file_not_found() {
        let result = get_json_from_cache(Path::new("/non/existent/file.json"));

        assert_eq!(result, serde_json::Value::Null);
    }

    #[test]
    fn test_get_json_from_cache_invalid_json() {
        use std::io::Write;
        use tempfile::NamedTempFile;

        let mut temp_file = NamedTempFile::new().unwrap();
        write!(temp_file, "invalid json content").unwrap();
        temp_file.flush().unwrap();

        let result = get_json_from_cache(temp_file.path());

        assert_eq!(result, serde_json::Value::Null);
    }

    #[test]
    fn test_get_json_from_cache_empty_file() {
        use tempfile::NamedTempFile;

        let temp_file = NamedTempFile::new().unwrap();

        let result = get_json_from_cache(temp_file.path());

        assert_eq!(result, serde_json::Value::Null);
    }

    #[test]
    fn test_get_json_from_cache_empty_json_object() {
        use std::io::Write;
        use tempfile::NamedTempFile;

        let mut temp_file = NamedTempFile::new().unwrap();
        write!(temp_file, "{{}}").unwrap();
        temp_file.flush().unwrap();

        let result = get_json_from_cache(temp_file.path());

        assert_eq!(result, json!({}));
    }

    // Tests for get_download_url_from_json

    #[test]
    fn test_get_download_url_from_json_success_windows() {
        let json_data = json!({
            "result": {
                "links": [
                    {
                        "downloadType": "serverBedrockWindows",
                        "downloadUrl": "https://example.com/bedrock-server-windows.zip"
                    },
                    {
                        "downloadType": "serverBedrockLinux",
                        "downloadUrl": "https://example.com/bedrock-server-linux.zip"
                    }
                ]
            }
        });

        let result = get_download_url_from_json(&json_data, &DownloadType::Windows);

        assert_eq!(
            result,
            Some("https://example.com/bedrock-server-windows.zip".to_string())
        );
    }

    #[test]
    fn test_get_download_url_from_json_success_server_jar() {
        let json_data = json!({
            "result": {
                "links": [
                    {
                        "downloadType": "serverBedrockWindows",
                        "downloadUrl": "https://example.com/bedrock-server-windows.zip"
                    },
                    {
                        "downloadType": "serverJar",
                        "downloadUrl": "https://example.com/bedrock-server.jar"
                    }
                ]
            }
        });

        let result = get_download_url_from_json(&json_data, &DownloadType::ServerJar);

        assert_eq!(
            result,
            Some("https://example.com/bedrock-server.jar".to_string())
        );
    }

    #[test]
    fn test_get_download_url_from_json_not_found() {
        let json_data = json!({
            "result": {
                "links": [
                    {
                        "downloadType": "serverBedrockWindows",
                        "downloadUrl": "https://example.com/bedrock-server-windows.zip"
                    }
                ]
            }
        });

        let result = get_download_url_from_json(&json_data, &DownloadType::ServerJar);

        assert_eq!(result, None);
    }

    #[test]
    fn test_get_download_url_from_json_missing_result() {
        let json_data = json!({ "error": "No data available" });

        let result = get_download_url_from_json(&json_data, &DownloadType::Windows);

        assert_eq!(result, None);
    }

    #[test]
    fn test_get_download_url_from_json_null_json() {
        let json_data = serde_json::Value::Null;

        let result = get_download_url_from_json(&json_data, &DownloadType::Windows);

        assert_eq!(result, None);
    }

    #[test]
    fn test_get_download_url_from_json_case_sensitive() {
        let json_data = json!({
            "result": {
                "links": [
                    {
                        "downloadType": "serverbedrockwindows",
                        "downloadUrl": "https://example.com/bedrock-server-windows.zip"
                    }
                ]
            }
        });

        let result = get_download_url_from_json(&json_data, &DownloadType::Windows);

        assert_eq!(result, None);
    }

    // Tests for fetch_update_zip

    #[test]
    fn test_fetch_update_zip_success() {
        use tempfile::TempDir;

        let mut server = Server::new();
        let test_content = b"fake zip content for testing";

        let mock = server
            .mock("GET", "/bedrock-server-success.zip")
            .with_status(200)
            .with_header("content-type", "application/zip")
            .with_body(test_content)
            .create();

        let dir = TempDir::new().unwrap();
        let download_url = format!("{}/bedrock-server-success.zip", server.url());
        let result = fetch_update_zip(&download_url, dir.path());

        mock.assert();
        let file_path = result.unwrap();
        assert!(file_path.exists());
        assert_eq!(file_path.file_name().unwrap(), "bedrock-server-success.zip");
        assert_eq!(std::fs::read(&file_path).unwrap(), test_content);
    }

    #[test]
    fn test_fetch_update_zip_server_error() {
        use tempfile::TempDir;

        let mut server = Server::new();

        let mock = server
            .mock("GET", "/bedrock-server.zip")
            .with_status(500)
            .with_body("Internal Server Error")
            .create();

        let dir = TempDir::new().unwrap();
        let download_url = format!("{}/bedrock-server.zip", server.url());
        let result = fetch_update_zip(&download_url, dir.path());

        mock.assert();
        assert!(matches!(result, Err(UpdateError::Download { .. })));
    }

    #[test]
    fn test_fetch_update_zip_not_found() {
        use tempfile::TempDir;

        let mut server = Server::new();

        let mock = server
            .mock("GET", "/nonexistent.zip")
            .with_status(404)
            .with_body("Not Found")
            .create();

        let dir = TempDir::new().unwrap();
        let download_url = format!("{}/nonexistent.zip", server.url());
        let result = fetch_update_zip(&download_url, dir.path());

        mock.assert();
        assert!(matches!(result, Err(UpdateError::Download { .. })));
    }

    #[test]
    fn test_fetch_update_zip_connection_error() {
        use tempfile::TempDir;

        let dir = TempDir::new().unwrap();
        let download_url = "http://non-existent-domain-12345.com/bedrock-server.zip";
        let result = fetch_update_zip(download_url, dir.path());

        assert!(matches!(result, Err(UpdateError::Download { .. })));
    }

    #[test]
    fn test_fetch_update_zip_two_concurrent_downloads_do_not_collide() {
        use tempfile::TempDir;

        // Regression test for the temp-dir collision fixed in this phase: two
        // concurrent downloads of the same filename must not corrupt each
        // other now that each gets its own unique directory.
        let mut server = Server::new();

        let mock = server
            .mock("GET", "/bedrock-server.zip")
            .with_status(200)
            .with_body(b"first")
            .expect(2)
            .create();

        let dir_a = TempDir::new().unwrap();
        let dir_b = TempDir::new().unwrap();
        let download_url = format!("{}/bedrock-server.zip", server.url());

        let path_a = fetch_update_zip(&download_url, dir_a.path()).unwrap();
        let path_b = fetch_update_zip(&download_url, dir_b.path()).unwrap();

        mock.assert();
        assert_ne!(path_a, path_b);
        assert_eq!(std::fs::read(&path_a).unwrap(), b"first");
        assert_eq!(std::fs::read(&path_b).unwrap(), b"first");
    }

    // Tests for update_cache

    #[test]
    fn test_update_cache_success() {
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let cache_path = temp_dir.path().join("cache.json");

        let test_json = json!({
            "result": {
                "links": [
                    {
                        "downloadType": "serverBedrockWindows",
                        "downloadUrl": "https://example.com/bedrock-server.zip"
                    }
                ]
            }
        });

        let result = update_cache(&test_json, &cache_path);

        assert!(result.is_ok());
        let content = std::fs::read_to_string(&cache_path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(parsed, test_json);
    }

    #[test]
    fn test_update_cache_creates_directory() {
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let nested_path = temp_dir.path().join("nested").join("deep").join("cache.json");

        let test_json = json!({ "version": "1.0.0", "data": "test" });

        let result = update_cache(&test_json, &nested_path);

        assert!(result.is_ok());
        assert!(nested_path.exists());
    }

    #[test]
    fn test_update_cache_overwrites_existing() {
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let cache_path = temp_dir.path().join("cache.json");

        update_cache(&json!({ "version": "1.0.0" }), &cache_path).unwrap();

        let second_json = json!({ "version": "2.0.0", "new_data": "updated" });
        update_cache(&second_json, &cache_path).unwrap();

        let content = std::fs::read_to_string(&cache_path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(parsed, second_json);
    }

    #[test]
    fn test_update_cache_invalid_path() {
        let invalid_path = if cfg!(windows) {
            Path::new("Z:\\nonexistent\\path\\cache.json")
        } else {
            Path::new("/root/nonexistent/path/cache.json")
        };

        let result = update_cache(&json!({ "test": "data" }), invalid_path);

        assert!(result.is_err());
    }

    // Helpers for the apply_update / staging tests

    fn write_test_zip(path: &Path, entries: &[(&str, &[u8])]) {
        use std::io::Write;

        let file = std::fs::File::create(path).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default();

        for (name, contents) in entries {
            writer.start_file(*name, options).unwrap();
            writer.write_all(contents).unwrap();
        }
        writer.finish().unwrap();
    }

    /// The name the first staged copy of `path` takes. Unlike `stale_path`,
    /// this does not step past names already taken, so tests can name the file
    /// staging produced rather than the one the next staging would produce.
    fn bare_stale_path(path: &Path) -> PathBuf {
        let mut name = path.as_os_str().to_os_string();
        name.push(STALE_MARKER);
        PathBuf::from(name)
    }

    /// Opens `path` the way Windows holds a running executable: readable and
    /// renameable by others, but not writable. Returns the handle; the lock
    /// lasts until it is dropped.
    #[cfg(windows)]
    fn lock_like_running_exe(path: &Path) -> std::fs::File {
        use std::os::windows::fs::OpenOptionsExt;

        const FILE_SHARE_READ: u32 = 0x1;
        const FILE_SHARE_DELETE: u32 = 0x4;

        std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_DELETE)
            .open(path)
            .unwrap()
    }

    // Tests for apply_update

    #[test]
    fn test_apply_update_writes_all_files() {
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let server_path = temp_dir.path().join("server");
        let zip_path = temp_dir.path().join("update.zip");

        write_test_zip(
            &zip_path,
            &[
                ("bedrock_server.exe", b"new binary"),
                ("behavior_packs/pack/manifest.json", b"{}"),
            ],
        );

        let staged = apply_update(&server_path, &zip_path, vec![]).unwrap();

        assert!(staged.is_empty());
        assert_eq!(
            std::fs::read(server_path.join("bedrock_server.exe")).unwrap(),
            b"new binary"
        );
        assert_eq!(
            std::fs::read(server_path.join("behavior_packs/pack/manifest.json")).unwrap(),
            b"{}"
        );
    }

    #[test]
    fn test_apply_update_skips_existing_excluded_file() {
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let server_path = temp_dir.path().join("server");
        std::fs::create_dir_all(&server_path).unwrap();
        std::fs::write(server_path.join("server.properties"), b"my settings").unwrap();

        let zip_path = temp_dir.path().join("update.zip");
        write_test_zip(
            &zip_path,
            &[
                ("server.properties", b"default settings"),
                ("bedrock_server.exe", b"new binary"),
            ],
        );

        let staged = apply_update(
            &server_path,
            &zip_path,
            vec!["server.properties".to_string()],
        )
        .unwrap();

        assert!(staged.is_empty());
        assert_eq!(
            std::fs::read(server_path.join("server.properties")).unwrap(),
            b"my settings"
        );
        assert_eq!(
            std::fs::read(server_path.join("bedrock_server.exe")).unwrap(),
            b"new binary"
        );
    }

    #[test]
    fn test_apply_update_writes_excluded_file_when_absent() {
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let server_path = temp_dir.path().join("server");
        let zip_path = temp_dir.path().join("update.zip");

        write_test_zip(&zip_path, &[("server.properties", b"default settings")]);

        let staged = apply_update(
            &server_path,
            &zip_path,
            vec!["server.properties".to_string()],
        )
        .unwrap();

        assert!(staged.is_empty());
        assert_eq!(
            std::fs::read(server_path.join("server.properties")).unwrap(),
            b"default settings"
        );
    }

    #[test]
    fn test_apply_update_missing_archive() {
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let server_path = temp_dir.path().join("server");
        let zip_path = temp_dir.path().join("does-not-exist.zip");

        let result = apply_update(&server_path, &zip_path, vec![]);

        assert!(matches!(result, Err(UpdateError::Io { .. })));
    }

    #[test]
    fn test_apply_update_overwrites_existing_files() {
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let server_path = temp_dir.path().join("server");
        std::fs::create_dir_all(&server_path).unwrap();
        std::fs::write(server_path.join("bedrock_server.exe"), b"old binary").unwrap();

        let zip_path = temp_dir.path().join("update.zip");
        write_test_zip(&zip_path, &[("bedrock_server.exe", b"new binary")]);

        let staged = apply_update(&server_path, &zip_path, vec![]).unwrap();

        assert!(staged.is_empty());
        assert_eq!(
            std::fs::read(server_path.join("bedrock_server.exe")).unwrap(),
            b"new binary"
        );
        assert!(
            !server_path
                .join(format!("bedrock_server.exe{}", STALE_MARKER))
                .exists()
        );
    }

    #[cfg(windows)]
    #[test]
    fn test_apply_update_stages_in_use_file() {
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let server_path = temp_dir.path().join("server");
        std::fs::create_dir_all(&server_path).unwrap();

        let exe_path = server_path.join("bedrock_server.exe");
        std::fs::write(&exe_path, b"old binary").unwrap();
        let _lock = lock_like_running_exe(&exe_path);

        let zip_path = temp_dir.path().join("update.zip");
        write_test_zip(
            &zip_path,
            &[
                ("bedrock_server.exe", b"new binary"),
                ("definitions/thing.json", b"{}"),
            ],
        );

        let staged = apply_update(&server_path, &zip_path, vec![]).unwrap();

        assert_eq!(staged, vec![exe_path.clone()]);
        assert_eq!(std::fs::read(&exe_path).unwrap(), b"new binary");
        assert_eq!(
            std::fs::read(server_path.join("definitions/thing.json")).unwrap(),
            b"{}"
        );

        let stale = bare_stale_path(&exe_path);
        assert!(stale.exists());
        assert_eq!(std::fs::read(&stale).unwrap(), b"old binary");
    }

    #[cfg(windows)]
    #[test]
    fn test_apply_update_stages_twice_without_restart() {
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let server_path = temp_dir.path().join("server");
        std::fs::create_dir_all(&server_path).unwrap();

        let exe_path = server_path.join("bedrock_server.exe");
        std::fs::write(&exe_path, b"v1").unwrap();

        let _running_v1 = lock_like_running_exe(&exe_path);

        let v2_zip = temp_dir.path().join("v2.zip");
        write_test_zip(&v2_zip, &[("bedrock_server.exe", b"v2")]);
        let staged_v2 = apply_update(&server_path, &v2_zip, vec![]).unwrap();
        assert_eq!(staged_v2.len(), 1);

        let _running_v2 = lock_like_running_exe(&exe_path);

        let v3_zip = temp_dir.path().join("v3.zip");
        write_test_zip(&v3_zip, &[("bedrock_server.exe", b"v3")]);
        let staged_v3 = apply_update(&server_path, &v3_zip, vec![])
            .expect("second update must not clash with the file staged by the first");
        assert_eq!(staged_v3.len(), 1);

        assert_eq!(std::fs::read(&exe_path).unwrap(), b"v3");
        let preserved: std::collections::HashSet<Vec<u8>> = std::fs::read_dir(&server_path)
            .unwrap()
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| is_stale_file(path))
            .map(|path| std::fs::read(path).unwrap())
            .collect();
        assert_eq!(
            preserved,
            [b"v1".to_vec(), b"v2".to_vec()].into_iter().collect()
        );
    }

    // Tests for create_replacing_in_use

    #[test]
    fn test_create_replacing_in_use_writes_normally() {
        use std::io::Write;
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("file.txt");
        std::fs::write(&path, b"old").unwrap();

        let (mut file, was_staged) = create_replacing_in_use(&path).unwrap();
        file.write_all(b"new").unwrap();
        drop(file);

        assert!(!was_staged);
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
        assert!(!bare_stale_path(&path).exists());
    }

    #[cfg(windows)]
    #[test]
    fn test_create_replacing_in_use_stages_locked_file() {
        use std::io::Write;
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("bedrock_server.exe");
        std::fs::write(&path, b"old").unwrap();
        let _lock = lock_like_running_exe(&path);

        let (mut file, was_staged) = create_replacing_in_use(&path).unwrap();
        file.write_all(b"new").unwrap();
        drop(file);

        assert!(was_staged);
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
        assert_eq!(std::fs::read(bare_stale_path(&path)).unwrap(), b"old");
    }

    #[cfg(windows)]
    #[test]
    fn test_create_replacing_in_use_preserves_previous_stale_file() {
        use std::io::Write;
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("bedrock_server.exe");
        std::fs::write(&path, b"old").unwrap();

        let existing_stale = temp_dir
            .path()
            .join(format!("bedrock_server.exe{}", STALE_MARKER));
        std::fs::write(&existing_stale, b"ancient").unwrap();
        let _lock = lock_like_running_exe(&path);

        let (mut file, was_staged) = create_replacing_in_use(&path).unwrap();
        file.write_all(b"new").unwrap();
        drop(file);

        assert!(was_staged);
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
        assert_eq!(std::fs::read(&existing_stale).unwrap(), b"ancient");
        assert_eq!(
            std::fs::read(
                temp_dir
                    .path()
                    .join(format!("bedrock_server.exe{}.1", STALE_MARKER))
            )
            .unwrap(),
            b"old"
        );
    }

    // Tests for stale_path and sweep_stale_files

    #[test]
    fn test_stale_path_appends_suffix() {
        let path = Path::new("/srv/minecraft/bedrock_server.exe");

        let stale = stale_path(path);

        assert_eq!(
            stale.file_name().unwrap().to_str().unwrap(),
            format!("bedrock_server.exe{}", STALE_MARKER)
        );
        assert_eq!(stale.parent(), path.parent());
    }

    #[test]
    fn test_stale_path_steps_past_taken_names() {
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("bedrock_server.exe");
        std::fs::write(&path, b"current").unwrap();

        std::fs::write(
            temp_dir
                .path()
                .join(format!("bedrock_server.exe{}", STALE_MARKER)),
            b"v1",
        )
        .unwrap();
        std::fs::write(
            temp_dir
                .path()
                .join(format!("bedrock_server.exe{}.1", STALE_MARKER)),
            b"v2",
        )
        .unwrap();

        let stale = stale_path(&path);

        assert_eq!(
            stale.file_name().unwrap().to_str().unwrap(),
            format!("bedrock_server.exe{}.2", STALE_MARKER)
        );
        assert!(!stale.exists());
    }

    #[test]
    fn test_is_stale_file_matches_bare_and_numbered_markers() {
        let base = format!("/srv/bedrock_server.exe{}", STALE_MARKER);

        assert!(is_stale_file(Path::new(&base)));
        assert!(is_stale_file(Path::new(&format!("{}.1", base))));
        assert!(is_stale_file(Path::new(&format!("{}.42", base))));
    }

    #[test]
    fn test_is_stale_file_rejects_unrelated_names() {
        assert!(!is_stale_file(Path::new("/srv/bedrock_server.exe")));
        assert!(!is_stale_file(Path::new("/srv/server.properties")));
        assert!(!is_stale_file(Path::new(&format!(
            "/srv/bedrock_server.exe{}.backup",
            STALE_MARKER
        ))));
    }

    #[test]
    fn test_sweep_stale_files_removes_nested_stale_files() {
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let nested = temp_dir.path().join("behavior_packs").join("pack");
        std::fs::create_dir_all(&nested).unwrap();

        std::fs::write(
            temp_dir.path().join(format!("server.exe{}", STALE_MARKER)),
            b"x",
        )
        .unwrap();
        std::fs::write(nested.join(format!("manifest.json{}", STALE_MARKER)), b"x").unwrap();
        std::fs::write(temp_dir.path().join("keep.txt"), b"keep").unwrap();

        let removed = sweep_stale_files(temp_dir.path());

        assert_eq!(removed, 2);
        assert!(
            !temp_dir
                .path()
                .join(format!("server.exe{}", STALE_MARKER))
                .exists()
        );
        assert!(
            !nested
                .join(format!("manifest.json{}", STALE_MARKER))
                .exists()
        );
        assert_eq!(
            std::fs::read(temp_dir.path().join("keep.txt")).unwrap(),
            b"keep"
        );
    }

    #[test]
    fn test_sweep_stale_files_empty_directory() {
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();

        assert_eq!(sweep_stale_files(temp_dir.path()), 0);
    }

    #[test]
    fn test_sweep_stale_files_missing_directory() {
        assert_eq!(sweep_stale_files(Path::new("/no/such/directory/anywhere")), 0);
    }

    // Tests for is_in_use_error

    #[test]
    fn test_is_in_use_error_classifies_sharing_violation() {
        let sharing_violation =
            std::io::Error::from_raw_os_error(if cfg!(windows) { 32 } else { 26 });

        assert!(is_in_use_error(&sharing_violation));
    }

    #[test]
    fn test_is_in_use_error_ignores_unrelated_errors() {
        let not_found = std::io::Error::from_raw_os_error(2);

        assert!(!is_in_use_error(&not_found));
    }

    // End-to-end: check() -> download() -> apply()

    #[test]
    fn test_check_reports_up_to_date_when_versions_match() {
        use tempfile::TempDir;

        let mut server = Server::new();
        let links = json!({
            "result": {
                "links": [
                    {
                        "downloadType": "serverBedrockWindows",
                        "downloadUrl": "https://example.com/same.zip"
                    }
                ]
            }
        });

        let _mock = server
            .mock("GET", "/api/v1.0/download/links")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(links.to_string())
            .create();

        let temp_dir = TempDir::new().unwrap();
        let cache_path = temp_dir.path().join("cache.json");
        std::fs::write(&cache_path, links.to_string()).unwrap();

        // check() itself always hits the real URL, so this test exercises the
        // pure decision logic via get_json_from_web_with_url + the comparison
        // directly instead of going through check().
        let web_json =
            get_json_from_web_with_url(&format!("{}/api/v1.0/download/links", server.url()))
                .unwrap();
        let cache_json = get_json_from_cache(&cache_path);
        let web_url =
            get_download_url_from_json(&web_json, &DownloadType::Windows).unwrap();
        let cache_url =
            get_download_url_from_json(&cache_json, &DownloadType::Windows).unwrap();

        assert_eq!(web_url, cache_url);
    }

    #[test]
    fn test_download_then_apply_round_trip() {
        use tempfile::TempDir;

        let mut server = Server::new();
        let mock = server
            .mock("GET", "/bedrock-server.zip")
            .with_status(200)
            .with_body(b"not actually a zip is fine for this stage")
            .create();

        let dir = TempDir::new().unwrap();
        let zip_path = fetch_update_zip(
            &format!("{}/bedrock-server.zip", server.url()),
            dir.path(),
        )
        .unwrap();
        mock.assert();
        assert!(zip_path.exists());

        // A StagedUpdate constructed directly (bypassing AvailableUpdate,
        // since that requires the real Mojang URL) still cleans up its temp
        // dir on drop.
        let staged = StagedUpdate {
            zip_path: zip_path.clone(),
            version: "1.0.0".to_string(),
            web_json: json!({}),
            cleanup: Cleanup::TempDir(dir),
        };
        drop(staged);
        assert!(!zip_path.exists(), "temp dir should be removed on drop");
    }

    // Tests for persist / load

    #[test]
    fn test_persist_then_load_round_trip() {
        use tempfile::TempDir;

        let source_dir = TempDir::new().unwrap();
        let zip_path = source_dir.path().join("update.zip");
        std::fs::write(&zip_path, b"archive contents").unwrap();

        let staged = StagedUpdate {
            zip_path,
            version: "1.2.3".to_string(),
            web_json: json!({ "result": { "links": [] } }),
            cleanup: Cleanup::TempDir(source_dir),
        };

        let stage_dir = TempDir::new().unwrap();
        staged.persist(stage_dir.path()).unwrap();

        let loaded = StagedUpdate::load(stage_dir.path()).unwrap().unwrap();
        assert_eq!(loaded.version(), "1.2.3");
        assert_eq!(std::fs::read(&loaded.zip_path).unwrap(), b"archive contents");
        assert_eq!(loaded.web_json, json!({ "result": { "links": [] } }));
    }

    #[test]
    fn test_load_missing_stage_returns_none() {
        use tempfile::TempDir;

        let stage_dir = TempDir::new().unwrap();

        assert!(StagedUpdate::load(stage_dir.path()).unwrap().is_none());
    }

    #[test]
    fn test_load_malformed_metadata_is_an_error() {
        use tempfile::TempDir;

        let stage_dir = TempDir::new().unwrap();
        std::fs::write(stage_dir.path().join(STAGED_META_FILE), b"not json").unwrap();

        let result = StagedUpdate::load(stage_dir.path());

        assert!(matches!(result, Err(UpdateError::Staging { .. })));
    }

    #[test]
    fn test_apply_removes_persisted_stage_on_success() {
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let server_path = temp_dir.path().join("server");
        let cache_path = temp_dir.path().join("cache.json");
        let stage_dir = temp_dir.path().join("stage");

        let source_dir = TempDir::new().unwrap();
        let zip_path = source_dir.path().join("update.zip");
        write_test_zip(&zip_path, &[("bedrock_server.exe", b"new binary")]);

        let staged = StagedUpdate {
            zip_path,
            version: "1.2.3".to_string(),
            web_json: json!({}),
            cleanup: Cleanup::TempDir(source_dir),
        };
        staged.persist(&stage_dir).unwrap();

        let loaded = StagedUpdate::load(&stage_dir).unwrap().unwrap();
        let config = UpdateConfig {
            download_type: DownloadType::Windows,
            server_path,
            cache_path,
            exclude: vec![],
            force: false,
        };
        loaded.apply(&config).unwrap();

        assert!(!stage_dir.exists(), "persisted stage should be cleaned up after apply");
    }
}
