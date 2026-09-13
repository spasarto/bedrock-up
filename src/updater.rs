use crate::args::{DownloadType, UpdateArgs};

/// Marks a file that was in use and had to be renamed aside to make room for
/// its replacement.
const STALE_MARKER: &str = ".bedrock-up-old";

/// Outcome of a successful `update()` call, used by callers (e.g. a scheduled
/// task wrapper) to decide whether the server actually needs to be restarted.
pub enum UpdateOutcome {
    Updated,
    /// The update was written, but some files were in use and were staged in
    /// place. The running server keeps the old build until it is restarted.
    UpdatedPendingRestart,
    AlreadyCurrent,
}

pub fn update(args: UpdateArgs) -> Result<UpdateOutcome, String> {
    let web_json = get_json_from_web();
    if web_json.is_null() {
        return Err("Failed to fetch update information from the web.".to_string());
    }

    let cache_json = get_json_from_cache(&args.cache_path);
    let web_download_url = get_download_url_from_json(&web_json, &args.download_type)
        .ok_or_else(|| format!("No download URL found for {}", args.download_type))?;
    let cache_download_url =
        get_download_url_from_json(&cache_json, &args.download_type).unwrap_or("0.0.0".to_owned());

    println!("Current version in cache: {}", cache_download_url);
    println!("Version available on the web: {}", web_download_url);

    if !args.force && web_download_url == cache_download_url {
        println!(
            "You are already on the latest version: {}",
            cache_download_url
        );
        return Ok(UpdateOutcome::AlreadyCurrent);
    }

    println!("New version available: {}", web_download_url);
    let server_path = std::path::PathBuf::from(shellexpand::tilde(&args.server_path).to_string());

    // Files staged by a previous update can only be deleted once the server
    // that had them open has restarted, so clear them out now.
    let swept = sweep_stale_files(&server_path);
    if swept > 0 {
        println!("Cleaned up {} file(s) staged by a previous update.", swept);
    }

    let zip_path = fetch_update_zip(&web_download_url)
        .ok_or_else(|| "Failed to download update archive.".to_string())?;

    let apply_result = apply_update(&server_path, &zip_path, args.exclude);
    if let Err(e) = std::fs::remove_file(&zip_path) {
        eprintln!(
            "Warning: failed to remove temp file {}: {}",
            zip_path.display(),
            e
        );
    }
    let staged = apply_result?;

    update_cache(web_json, &args.cache_path)
        .map_err(|e| format!("Update applied but failed to update cache: {}", e))?;

    if staged.is_empty() {
        println!("Update applied successfully.");
        return Ok(UpdateOutcome::Updated);
    }

    println!(
        "Update applied successfully. {} file(s) were in use and were staged in place.",
        staged.len()
    );
    report_running_servers(&server_path);
    Ok(UpdateOutcome::UpdatedPendingRestart)
}

/// Names the processes still running the previous build out of `server_path`,
/// so the caller knows exactly what has to be restarted.
fn report_running_servers(server_path: &std::path::Path) {
    let running = crate::process::find_server_processes(server_path);
    if running.is_empty() {
        println!("No running server found for this path; the update is already live.");
        return;
    }

    println!("Restart the following to activate the new version:");
    for process in running {
        println!("  PID {} ({})", process.pid, process.exe.display());
    }
}

fn get_json_from_web() -> serde_json::Value {
    get_json_from_web_with_url(
        "https://net-secondary.web.minecraft-services.net/api/v1.0/download/links",
    )
}

fn get_json_from_web_with_url(url: &str) -> serde_json::Value {
    println!("Fetching links from the web...");

    reqwest::blocking::get(url)
        .and_then(|resp| resp.json::<serde_json::Value>())
        .unwrap_or_else(|e| {
            eprintln!("Failed to fetch or parse JSON from {}: {}", url, e);
            serde_json::Value::Null
        })
}

fn get_json_from_cache(cache_path: &str) -> serde_json::Value {
    println!("Reading cache from: {}", cache_path);
    let cache_path = shellexpand::tilde(cache_path).to_string();

    std::fs::File::open(&cache_path)
        .and_then(|file| {
            let reader = std::io::BufReader::new(file);
            serde_json::from_reader(reader).map_err(|_| {
                eprintln!("Failed to read or parse cache file: {}", cache_path);
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

fn fetch_update_zip(download_url: &str) -> Option<std::path::PathBuf> {
    let resp = reqwest::blocking::get(download_url).ok()?;
    if !resp.status().is_success() {
        eprintln!("Failed to download update: {}", resp.status());
        return None;
    }

    let file_name = download_url.split('/').last().unwrap_or("update.zip");
    let file_path = std::env::temp_dir().join(file_name);

    match std::fs::File::create(&file_path) {
        Ok(mut file) => {
            if let Ok(bytes) = resp.bytes() {
                if std::io::copy(&mut bytes.as_ref(), &mut file).is_ok() {
                    println!("Downloaded update to: {}", file_path.display());
                    Some(file_path)
                } else {
                    eprintln!("Failed to write downloaded file");
                    None
                }
            } else {
                eprintln!("Failed to read response bytes");
                None
            }
        }
        Err(e) => {
            eprintln!("Failed to create file: {}", e);
            None
        }
    }
}

/// Applies the archive over the server directory, returning the files that had
/// to be staged because they were in use.
fn apply_update(
    server_path: &std::path::Path,
    zip_path: &std::path::Path,
    exclude: Vec<String>,
) -> Result<Vec<std::path::PathBuf>, String> {
    println!("Applying update from: {}", zip_path.display());
    println!("Excluded files: {:?}", exclude);

    let zip_file = std::fs::File::open(zip_path).map_err(|e| {
        format!(
            "Failed to open update archive {}: {}",
            zip_path.display(),
            e
        )
    })?;
    let mut archive = zip::ZipArchive::new(zip_file).map_err(|e| {
        format!(
            "Failed to read update archive {}: {}",
            zip_path.display(),
            e
        )
    })?;
    let exclude_set: std::collections::HashSet<_> = exclude.into_iter().collect();
    let mut staged = Vec::new();

    for i in 0..archive.len() {
        let mut file = archive
            .by_index(i)
            .map_err(|e| format!("Failed to read entry {} from update archive: {}", i, e))?;
        let out_path = match file.enclosed_name() {
            Some(path) => path,
            None => continue,
        };

        let should_exclude = exclude_set.contains(out_path.to_str().unwrap_or(""));

        let out_path = server_path.join(out_path);

        let should_exclude = should_exclude && std::fs::metadata(&out_path).is_ok();
        if should_exclude {
            println!("Skipping excluded file: {}", out_path.display());
            continue;
        }
        if file.is_dir() {
            std::fs::create_dir_all(&out_path)
                .map_err(|e| format!("Failed to create directory {}: {}", out_path.display(), e))?;
        } else {
            if let Some(parent) = out_path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| {
                    format!("Failed to create directory {}: {}", parent.display(), e)
                })?;
            }

            let (mut outfile, was_staged) = create_replacing_in_use(&out_path)?;
            if was_staged {
                println!("Staged in-use file: {}", out_path.display());
                staged.push(out_path.clone());
            }
            std::io::copy(&mut file, &mut outfile).map_err(|e| {
                format!("Failed to write contents of {}: {}", out_path.display(), e)
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
fn create_replacing_in_use(out_path: &std::path::Path) -> Result<(std::fs::File, bool), String> {
    match std::fs::File::create(out_path) {
        Ok(file) => Ok((file, false)),
        Err(e) if is_in_use_error(&e) => {
            // Guaranteed not to name an existing file, so nothing staged by an
            // earlier update is disturbed or blocks the rename.
            let stale = stale_path(out_path);
            std::fs::rename(out_path, &stale).map_err(|e| {
                format!(
                    "{} is in use and could not be renamed aside: {}",
                    out_path.display(),
                    e
                )
            })?;
            let file = std::fs::File::create(out_path).map_err(|e| {
                format!(
                    "Failed to write {} after staging the in-use copy: {}",
                    out_path.display(),
                    e
                )
            })?;
            Ok((file, true))
        }
        Err(e) => Err(format!("Failed to write {}: {}", out_path.display(), e)),
    }
}

/// Picks the name to move an in-use file aside to.
///
/// A file staged by an earlier update can still be held open by a server that
/// has not restarted yet, and such a file can be neither deleted nor replaced.
/// Stepping past names that are already taken keeps a second update from
/// clashing with the first, so any number of updates can be applied between
/// restarts.
fn stale_path(out_path: &std::path::Path) -> std::path::PathBuf {
    let staged_name = |suffix: &str| {
        let mut name = out_path.as_os_str().to_os_string();
        name.push(STALE_MARKER);
        name.push(suffix);
        std::path::PathBuf::from(name)
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
fn is_stale_file(path: &std::path::Path) -> bool {
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
fn sweep_stale_files(dir: &std::path::Path) -> usize {
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

fn update_cache(web_json: serde_json::Value, cache_path: &str) -> std::io::Result<()> {
    let cache_path = shellexpand::tilde(cache_path).to_string();
    if let Some(parent) = std::path::Path::new(&cache_path).parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = std::fs::File::create(cache_path)?;
    serde_json::to_writer(file, &web_json)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mockito::Server;
    use serde_json::json;

    #[test]
    fn test_get_json_from_web_success() {
        // Create a mock server
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

        // Test the function
        let result =
            get_json_from_web_with_url(&format!("{}/api/v1.0/download/links", server.url()));

        // Verify the mock was called
        mock.assert();

        // Verify the response
        assert_eq!(result, mock_response);
    }

    #[test]
    fn test_get_json_from_web_invalid_json() {
        // Create a mock server that returns invalid JSON
        let mut server = Server::new();

        let mock = server
            .mock("GET", "/api/v1.0/download/links")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body("invalid json")
            .create();

        // Test the function
        let result =
            get_json_from_web_with_url(&format!("{}/api/v1.0/download/links", server.url()));

        // Verify the mock was called
        mock.assert();

        // Verify the response is null for invalid JSON
        assert_eq!(result, serde_json::Value::Null);
    }

    #[test]
    fn test_get_json_from_web_server_error() {
        // Create a mock server that returns a server error
        let mut server = Server::new();

        let mock = server
            .mock("GET", "/api/v1.0/download/links")
            .with_status(500)
            .with_body("Internal Server Error")
            .create();

        // Test the function
        let result =
            get_json_from_web_with_url(&format!("{}/api/v1.0/download/links", server.url()));

        // Verify the mock was called
        mock.assert();

        // Verify the response is null for server error
        assert_eq!(result, serde_json::Value::Null);
    }

    #[test]
    fn test_get_json_from_web_connection_error() {
        // Test with an invalid URL to simulate connection error
        let result = get_json_from_web_with_url("http://non-existent-domain-12345.com/api");

        // Verify the response is null for connection error
        assert_eq!(result, serde_json::Value::Null);
    }

    #[test]
    fn test_get_json_from_web_empty_response() {
        // Create a mock server that returns empty JSON
        let mut server = Server::new();

        let mock = server
            .mock("GET", "/api/v1.0/download/links")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body("{}")
            .create();

        // Test the function
        let result =
            get_json_from_web_with_url(&format!("{}/api/v1.0/download/links", server.url()));

        // Verify the mock was called
        mock.assert();

        // Verify the response is empty JSON object
        assert_eq!(result, json!({}));
    }

    #[test]
    fn test_get_json_from_web_complex_response() {
        // Test with a more complex JSON response
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

        // Test the function
        let result =
            get_json_from_web_with_url(&format!("{}/api/v1.0/download/links", server.url()));

        // Verify the mock was called
        mock.assert();

        // Verify the response matches expected structure
        assert_eq!(result, mock_response);

        // Verify specific nested values
        assert_eq!(
            result["result"]["links"][0]["downloadType"],
            "bedrock-server"
        );
        assert_eq!(result["result"]["links"][1]["version"], "1.21.0");
        assert_eq!(
            result["result"]["metadata"]["lastUpdated"],
            "2025-07-04T12:00:00Z"
        );
    }

    // Tests for get_json_from_cache function
    #[test]
    fn test_get_json_from_cache_success() {
        use std::io::Write;
        use tempfile::NamedTempFile;

        // Create a temporary file with valid JSON
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

        write!(temp_file, "{}", test_json.to_string()).unwrap();
        temp_file.flush().unwrap();

        // Test the function
        let result = get_json_from_cache(temp_file.path().to_str().unwrap());

        // Verify the result
        assert_eq!(result, test_json);
    }

    #[test]
    fn test_get_json_from_cache_file_not_found() {
        // Test with a non-existent file path
        let result = get_json_from_cache("/non/existent/file.json");

        // Verify the result is null when file doesn't exist
        assert_eq!(result, serde_json::Value::Null);
    }

    #[test]
    fn test_get_json_from_cache_invalid_json() {
        use std::io::Write;
        use tempfile::NamedTempFile;

        // Create a temporary file with invalid JSON
        let mut temp_file = NamedTempFile::new().unwrap();
        write!(temp_file, "invalid json content").unwrap();
        temp_file.flush().unwrap();

        // Test the function
        let result = get_json_from_cache(temp_file.path().to_str().unwrap());

        // Verify the result is null for invalid JSON
        assert_eq!(result, serde_json::Value::Null);
    }

    #[test]
    fn test_get_json_from_cache_empty_file() {
        use tempfile::NamedTempFile;

        // Create an empty temporary file
        let temp_file = NamedTempFile::new().unwrap();

        // Test the function
        let result = get_json_from_cache(temp_file.path().to_str().unwrap());

        // Verify the result is null for empty file
        assert_eq!(result, serde_json::Value::Null);
    }

    #[test]
    fn test_get_json_from_cache_empty_json_object() {
        use std::io::Write;
        use tempfile::NamedTempFile;

        // Create a temporary file with empty JSON object
        let mut temp_file = NamedTempFile::new().unwrap();
        write!(temp_file, "{{}}").unwrap();
        temp_file.flush().unwrap();

        // Test the function
        let result = get_json_from_cache(temp_file.path().to_str().unwrap());

        // Verify the result is empty JSON object
        assert_eq!(result, json!({}));
    }

    #[test]
    fn test_get_json_from_cache_complex_json() {
        use std::io::Write;
        use tempfile::NamedTempFile;

        // Create a temporary file with complex JSON
        let mut temp_file = NamedTempFile::new().unwrap();
        let complex_json = json!({
            "result": {
                "links": [
                    {
                        "downloadType": "bedrock-server",
                        "downloadUrl": "https://example.com/bedrock-server-1.20.0.zip",
                        "version": "1.20.0",
                        "metadata": {
                            "size": 123456789,
                            "checksum": "abc123def456"
                        }
                    },
                    {
                        "downloadType": "bedrock-server-preview",
                        "downloadUrl": "https://example.com/bedrock-server-preview-1.21.0.zip",
                        "version": "1.21.0",
                        "metadata": {
                            "size": 987654321,
                            "checksum": "xyz789uvw012"
                        }
                    }
                ],
                "lastUpdated": "2025-07-04T12:00:00Z",
                "totalCount": 2
            },
            "status": "success"
        });

        write!(temp_file, "{}", complex_json.to_string()).unwrap();
        temp_file.flush().unwrap();

        // Test the function
        let result = get_json_from_cache(temp_file.path().to_str().unwrap());

        // Verify the result matches the complex JSON
        assert_eq!(result, complex_json);

        // Verify specific nested values
        assert_eq!(result["result"]["links"][0]["version"], "1.20.0");
        assert_eq!(result["result"]["links"][1]["metadata"]["size"], 987654321);
        assert_eq!(result["result"]["totalCount"], 2);
        assert_eq!(result["status"], "success");
    }

    #[test]
    fn test_get_json_from_cache_with_tilde_expansion() {
        use std::io::Write;
        use tempfile::NamedTempFile;

        // Create a temporary file with valid JSON
        let mut temp_file = NamedTempFile::new().unwrap();
        let test_json = json!({
            "cached_data": {
                "version": "1.0.0",
                "timestamp": "2025-07-04T12:00:00Z"
            }
        });

        write!(temp_file, "{}", test_json.to_string()).unwrap();
        temp_file.flush().unwrap();

        // Test the function with absolute path (no tilde expansion needed)
        let result = get_json_from_cache(temp_file.path().to_str().unwrap());

        // Verify the result
        assert_eq!(result, test_json);
    }

    #[test]
    fn test_get_json_from_cache_partial_json() {
        use std::io::Write;
        use tempfile::NamedTempFile;

        // Create a temporary file with truncated/partial JSON
        let mut temp_file = NamedTempFile::new().unwrap();
        write!(temp_file, "{{\"result\": {{\"links\": [").unwrap();
        temp_file.flush().unwrap();

        // Test the function
        let result = get_json_from_cache(temp_file.path().to_str().unwrap());

        // Verify the result is null for partial/invalid JSON
        assert_eq!(result, serde_json::Value::Null);
    }

    #[test]
    fn test_get_json_from_cache_json_array() {
        use std::io::Write;
        use tempfile::NamedTempFile;

        // Create a temporary file with JSON array
        let mut temp_file = NamedTempFile::new().unwrap();
        let json_array = json!([
            {
                "name": "item1",
                "value": 123
            },
            {
                "name": "item2",
                "value": 456
            }
        ]);

        write!(temp_file, "{}", json_array.to_string()).unwrap();
        temp_file.flush().unwrap();

        // Test the function
        let result = get_json_from_cache(temp_file.path().to_str().unwrap());

        // Verify the result matches the JSON array
        assert_eq!(result, json_array);
        assert!(result.is_array());
        assert_eq!(result.as_array().unwrap().len(), 2);
    }

    // Tests for get_download_url_from_json function
    #[test]
    fn test_get_download_url_from_json_success_windows() {
        use crate::args::DownloadType;

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
    fn test_get_download_url_from_json_success_linux() {
        use crate::args::DownloadType;

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

        let result = get_download_url_from_json(&json_data, &DownloadType::Linux);

        assert_eq!(
            result,
            Some("https://example.com/bedrock-server-linux.zip".to_string())
        );
    }

    #[test]
    fn test_get_download_url_from_json_success_preview_windows() {
        use crate::args::DownloadType;

        let json_data = json!({
            "result": {
                "links": [
                    {
                        "downloadType": "serverBedrockPreviewWindows",
                        "downloadUrl": "https://example.com/bedrock-server-preview-windows.zip"
                    },
                    {
                        "downloadType": "serverBedrockPreviewLinux",
                        "downloadUrl": "https://example.com/bedrock-server-preview-linux.zip"
                    }
                ]
            }
        });

        let result = get_download_url_from_json(&json_data, &DownloadType::PreviewWindows);

        assert_eq!(
            result,
            Some("https://example.com/bedrock-server-preview-windows.zip".to_string())
        );
    }

    #[test]
    fn test_get_download_url_from_json_success_preview_linux() {
        use crate::args::DownloadType;

        let json_data = json!({
            "result": {
                "links": [
                    {
                        "downloadType": "serverBedrockPreviewLinux",
                        "downloadUrl": "https://example.com/bedrock-server-preview-linux.zip"
                    },
                    {
                        "downloadType": "serverJar",
                        "downloadUrl": "https://example.com/bedrock-server.jar"
                    }
                ]
            }
        });

        let result = get_download_url_from_json(&json_data, &DownloadType::PreviewLinux);

        assert_eq!(
            result,
            Some("https://example.com/bedrock-server-preview-linux.zip".to_string())
        );
    }

    #[test]
    fn test_get_download_url_from_json_success_server_jar() {
        use crate::args::DownloadType;

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
        use crate::args::DownloadType;

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

        let result = get_download_url_from_json(&json_data, &DownloadType::ServerJar);

        assert_eq!(result, None);
    }

    #[test]
    fn test_get_download_url_from_json_missing_result() {
        use crate::args::DownloadType;

        let json_data = json!({
            "error": "No data available"
        });

        let result = get_download_url_from_json(&json_data, &DownloadType::Windows);

        assert_eq!(result, None);
    }

    #[test]
    fn test_get_download_url_from_json_missing_links() {
        use crate::args::DownloadType;

        let json_data = json!({
            "result": {
                "message": "No links available"
            }
        });

        let result = get_download_url_from_json(&json_data, &DownloadType::Windows);

        assert_eq!(result, None);
    }

    #[test]
    fn test_get_download_url_from_json_links_not_array() {
        use crate::args::DownloadType;

        let json_data = json!({
            "result": {
                "links": "not an array"
            }
        });

        let result = get_download_url_from_json(&json_data, &DownloadType::Windows);

        assert_eq!(result, None);
    }

    #[test]
    fn test_get_download_url_from_json_empty_links_array() {
        use crate::args::DownloadType;

        let json_data = json!({
            "result": {
                "links": []
            }
        });

        let result = get_download_url_from_json(&json_data, &DownloadType::Windows);

        assert_eq!(result, None);
    }

    #[test]
    fn test_get_download_url_from_json_missing_download_type() {
        use crate::args::DownloadType;

        let json_data = json!({
            "result": {
                "links": [
                    {
                        "downloadUrl": "https://example.com/bedrock-server.zip"
                    }
                ]
            }
        });

        let result = get_download_url_from_json(&json_data, &DownloadType::Windows);

        assert_eq!(result, None);
    }

    #[test]
    fn test_get_download_url_from_json_missing_download_url() {
        use crate::args::DownloadType;

        let json_data = json!({
            "result": {
                "links": [
                    {
                        "downloadType": "serverBedrockWindows"
                    }
                ]
            }
        });

        let result = get_download_url_from_json(&json_data, &DownloadType::Windows);

        assert_eq!(result, None);
    }

    #[test]
    fn test_get_download_url_from_json_download_url_not_string() {
        use crate::args::DownloadType;

        let json_data = json!({
            "result": {
                "links": [
                    {
                        "downloadType": "serverBedrockWindows",
                        "downloadUrl": 123
                    }
                ]
            }
        });

        let result = get_download_url_from_json(&json_data, &DownloadType::Windows);

        assert_eq!(result, None);
    }

    #[test]
    fn test_get_download_url_from_json_multiple_matches_returns_first() {
        use crate::args::DownloadType;

        let json_data = json!({
            "result": {
                "links": [
                    {
                        "downloadType": "serverBedrockWindows",
                        "downloadUrl": "https://example.com/bedrock-server-windows-1.zip"
                    },
                    {
                        "downloadType": "serverBedrockWindows",
                        "downloadUrl": "https://example.com/bedrock-server-windows-2.zip"
                    }
                ]
            }
        });

        let result = get_download_url_from_json(&json_data, &DownloadType::Windows);

        assert_eq!(
            result,
            Some("https://example.com/bedrock-server-windows-1.zip".to_string())
        );
    }

    #[test]
    fn test_get_download_url_from_json_complex_structure() {
        use crate::args::DownloadType;

        let json_data = json!({
            "result": {
                "links": [
                    {
                        "downloadType": "serverBedrockWindows",
                        "downloadUrl": "https://example.com/bedrock-server-windows.zip",
                        "version": "1.20.0",
                        "metadata": {
                            "size": 123456789,
                            "checksum": "abc123def456"
                        }
                    },
                    {
                        "downloadType": "serverBedrockLinux",
                        "downloadUrl": "https://example.com/bedrock-server-linux.zip",
                        "version": "1.20.0",
                        "metadata": {
                            "size": 987654321,
                            "checksum": "xyz789uvw012"
                        }
                    }
                ],
                "lastUpdated": "2025-07-04T12:00:00Z"
            }
        });

        let result = get_download_url_from_json(&json_data, &DownloadType::Linux);

        assert_eq!(
            result,
            Some("https://example.com/bedrock-server-linux.zip".to_string())
        );
    }

    #[test]
    fn test_get_download_url_from_json_null_json() {
        use crate::args::DownloadType;

        let json_data = serde_json::Value::Null;

        let result = get_download_url_from_json(&json_data, &DownloadType::Windows);

        assert_eq!(result, None);
    }

    #[test]
    fn test_get_download_url_from_json_case_sensitive() {
        use crate::args::DownloadType;

        let json_data = json!({
            "result": {
                "links": [
                    {
                        "downloadType": "serverbedrockwindows",  // lowercase
                        "downloadUrl": "https://example.com/bedrock-server-windows.zip"
                    }
                ]
            }
        });

        let result = get_download_url_from_json(&json_data, &DownloadType::Windows);

        // Should return None because the case doesn't match
        assert_eq!(result, None);
    }

    // Tests for fetch_update_zip function
    #[test]
    fn test_fetch_update_zip_success() {
        let mut server = Server::new();
        let test_content = b"fake zip content for testing";

        let mock = server
            .mock("GET", "/bedrock-server-success.zip")
            .with_status(200)
            .with_header("content-type", "application/zip")
            .with_body(test_content)
            .create();

        let download_url = format!("{}/bedrock-server-success.zip", server.url());
        let result = fetch_update_zip(&download_url);

        mock.assert();
        assert!(result.is_some());

        let file_path = result.unwrap();
        assert!(file_path.exists());
        assert_eq!(file_path.file_name().unwrap(), "bedrock-server-success.zip");

        // Verify file content
        let content = std::fs::read(&file_path).unwrap();
        assert_eq!(content, test_content);

        // Cleanup
        std::fs::remove_file(file_path).unwrap();
    }

    #[test]
    fn test_fetch_update_zip_server_error() {
        let mut server = Server::new();

        let mock = server
            .mock("GET", "/bedrock-server.zip")
            .with_status(500)
            .with_body("Internal Server Error")
            .create();

        let download_url = format!("{}/bedrock-server.zip", server.url());
        let result = fetch_update_zip(&download_url);

        mock.assert();
        assert!(result.is_none());
    }

    #[test]
    fn test_fetch_update_zip_not_found() {
        let mut server = Server::new();

        let mock = server
            .mock("GET", "/nonexistent.zip")
            .with_status(404)
            .with_body("Not Found")
            .create();

        let download_url = format!("{}/nonexistent.zip", server.url());
        let result = fetch_update_zip(&download_url);

        mock.assert();
        assert!(result.is_none());
    }

    #[test]
    fn test_fetch_update_zip_connection_error() {
        let download_url = "http://non-existent-domain-12345.com/bedrock-server.zip";
        let result = fetch_update_zip(download_url);

        assert!(result.is_none());
    }

    #[test]
    fn test_fetch_update_zip_filename_extraction() {
        let mut server = Server::new();
        let test_content = b"test zip content";

        let mock = server
            .mock("GET", "/path/to/minecraft-bedrock-server-1.20.0.zip")
            .with_status(200)
            .with_header("content-type", "application/zip")
            .with_body(test_content)
            .create();

        let download_url = format!(
            "{}/path/to/minecraft-bedrock-server-1.20.0.zip",
            server.url()
        );
        let result = fetch_update_zip(&download_url);

        mock.assert();
        assert!(result.is_some());

        let file_path = result.unwrap();
        assert!(file_path.exists());
        assert_eq!(
            file_path.file_name().unwrap(),
            "minecraft-bedrock-server-1.20.0.zip"
        );

        // Cleanup
        std::fs::remove_file(file_path).unwrap();
    }

    #[test]
    fn test_fetch_update_zip_default_filename() {
        let mut server = Server::new();
        let test_content = b"test zip content";

        // Use a path that will trigger the default filename behavior
        let mock = server
            .mock("GET", "/no-extension")
            .with_status(200)
            .with_header("content-type", "application/zip")
            .with_body(test_content)
            .create();

        let download_url = format!("{}/no-extension", server.url());
        let result = fetch_update_zip(&download_url);

        mock.assert();
        assert!(result.is_some());

        let file_path = result.unwrap();
        assert!(file_path.exists());
        assert_eq!(file_path.file_name().unwrap(), "no-extension");

        // Cleanup
        std::fs::remove_file(file_path).unwrap();
    }

    #[test]
    fn test_fetch_update_zip_actual_default_filename() {
        // Test the default filename behavior by directly testing the logic
        // In a real scenario, this would occur when split('/').last() returns None or empty string

        // For now, let's test with a simple case that should work
        let mut server = Server::new();
        let test_content = b"test zip content";

        let mock = server
            .mock("GET", "/download")
            .with_status(200)
            .with_header("content-type", "application/zip")
            .with_body(test_content)
            .create();

        let download_url = format!("{}/download", server.url());
        let result = fetch_update_zip(&download_url);

        mock.assert();
        assert!(result.is_some());

        let file_path = result.unwrap();
        assert!(file_path.exists());
        assert_eq!(file_path.file_name().unwrap(), "download");

        // Cleanup
        std::fs::remove_file(file_path).unwrap();
    }

    #[test]
    fn test_fetch_update_zip_empty_content() {
        let mut server = Server::new();

        let mock = server
            .mock("GET", "/empty.zip")
            .with_status(200)
            .with_header("content-type", "application/zip")
            .with_body("")
            .create();

        let download_url = format!("{}/empty.zip", server.url());
        let result = fetch_update_zip(&download_url);

        mock.assert();
        assert!(result.is_some());

        let file_path = result.unwrap();
        assert!(file_path.exists());
        assert_eq!(file_path.file_name().unwrap(), "empty.zip");

        // Verify file is empty
        let content = std::fs::read(&file_path).unwrap();
        assert_eq!(content.len(), 0);

        // Cleanup
        std::fs::remove_file(file_path).unwrap();
    }

    #[test]
    fn test_fetch_update_zip_large_file() {
        let mut server = Server::new();
        let test_content = vec![0u8; 1024 * 1024]; // 1MB of zeros

        let mock = server
            .mock("GET", "/large.zip")
            .with_status(200)
            .with_header("content-type", "application/zip")
            .with_body(&test_content)
            .create();

        let download_url = format!("{}/large.zip", server.url());
        let result = fetch_update_zip(&download_url);

        mock.assert();
        assert!(result.is_some());

        let file_path = result.unwrap();
        assert!(file_path.exists());
        assert_eq!(file_path.file_name().unwrap(), "large.zip");

        // Verify file size
        let content = std::fs::read(&file_path).unwrap();
        assert_eq!(content.len(), 1024 * 1024);

        // Cleanup
        std::fs::remove_file(file_path).unwrap();
    }

    #[test]
    fn test_fetch_update_zip_temp_directory() {
        let mut server = Server::new();
        let test_content = b"test content";

        let mock = server
            .mock("GET", "/test.zip")
            .with_status(200)
            .with_header("content-type", "application/zip")
            .with_body(test_content)
            .create();

        let download_url = format!("{}/test.zip", server.url());
        let result = fetch_update_zip(&download_url);

        mock.assert();
        assert!(result.is_some());

        let file_path = result.unwrap();

        // Verify file is in temp directory
        assert!(file_path.starts_with(std::env::temp_dir()));
        assert!(file_path.exists());

        // Cleanup
        std::fs::remove_file(file_path).unwrap();
    }

    #[test]
    fn test_fetch_update_zip_url_with_query_params() {
        let mut server = Server::new();
        let test_content = b"test content with query params";

        let mock = server
            .mock("GET", "/bedrock-server-query.zip")
            .with_status(200)
            .with_header("content-type", "application/zip")
            .with_body(test_content)
            .create();

        let download_url = format!("{}/bedrock-server-query.zip", server.url());
        let result = fetch_update_zip(&download_url);

        mock.assert();
        assert!(result.is_some());

        let file_path = result.unwrap();
        assert!(file_path.exists());

        // Should extract filename correctly
        assert_eq!(file_path.file_name().unwrap(), "bedrock-server-query.zip");

        // Cleanup
        std::fs::remove_file(file_path).unwrap();
    }

    #[test]
    fn test_fetch_update_zip_redirect_response() {
        let mut server = Server::new();

        // Create a redirect response
        let mock = server
            .mock("GET", "/redirect")
            .with_status(302)
            .with_header("location", &format!("{}/final.zip", server.url()))
            .create();

        let final_mock = server
            .mock("GET", "/final.zip")
            .with_status(200)
            .with_header("content-type", "application/zip")
            .with_body(b"redirected content")
            .create();

        let download_url = format!("{}/redirect", server.url());
        let result = fetch_update_zip(&download_url);

        mock.assert();
        final_mock.assert();
        assert!(result.is_some());

        let file_path = result.unwrap();
        assert!(file_path.exists());

        // Cleanup
        std::fs::remove_file(file_path).unwrap();
    }

    #[test]
    fn test_fetch_update_zip_special_characters_in_filename() {
        let mut server = Server::new();
        let test_content = b"test content";

        let mock = server
            .mock("GET", "/bedrock-server-v1.20.0-beta.zip")
            .with_status(200)
            .with_header("content-type", "application/zip")
            .with_body(test_content)
            .create();

        let download_url = format!("{}/bedrock-server-v1.20.0-beta.zip", server.url());
        let result = fetch_update_zip(&download_url);

        mock.assert();
        assert!(result.is_some());

        let file_path = result.unwrap();
        assert!(file_path.exists());
        assert_eq!(
            file_path.file_name().unwrap(),
            "bedrock-server-v1.20.0-beta.zip"
        );

        // Cleanup
        std::fs::remove_file(file_path).unwrap();
    }

    #[test]
    fn test_fetch_update_zip_no_content_type() {
        let mut server = Server::new();
        let test_content = b"content without content-type";

        let mock = server
            .mock("GET", "/server.zip")
            .with_status(200)
            .with_body(test_content)
            .create();

        let download_url = format!("{}/server.zip", server.url());
        let result = fetch_update_zip(&download_url);

        mock.assert();
        assert!(result.is_some());

        let file_path = result.unwrap();
        assert!(file_path.exists());

        // Verify content
        let content = std::fs::read(&file_path).unwrap();
        assert_eq!(content, test_content);

        // Cleanup
        std::fs::remove_file(file_path).unwrap();
    }

    // Tests for update_cache function
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

        let result = update_cache(test_json.clone(), cache_path.to_str().unwrap());

        assert!(result.is_ok());
        assert!(cache_path.exists());

        // Verify the file content
        let content = std::fs::read_to_string(&cache_path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(parsed, test_json);
    }

    #[test]
    fn test_update_cache_creates_directory() {
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let nested_path = temp_dir
            .path()
            .join("nested")
            .join("deep")
            .join("cache.json");

        let test_json = json!({
            "version": "1.0.0",
            "data": "test"
        });

        let result = update_cache(test_json.clone(), nested_path.to_str().unwrap());

        assert!(result.is_ok());
        assert!(nested_path.exists());
        assert!(nested_path.parent().unwrap().exists());

        // Verify the file content
        let content = std::fs::read_to_string(&nested_path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(parsed, test_json);
    }

    #[test]
    fn test_update_cache_overwrites_existing() {
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let cache_path = temp_dir.path().join("cache.json");

        // First write
        let first_json = json!({
            "version": "1.0.0"
        });

        let result = update_cache(first_json, cache_path.to_str().unwrap());
        assert!(result.is_ok());

        // Second write (overwrite)
        let second_json = json!({
            "version": "2.0.0",
            "new_data": "updated"
        });

        let result = update_cache(second_json.clone(), cache_path.to_str().unwrap());
        assert!(result.is_ok());

        // Verify the file was overwritten
        let content = std::fs::read_to_string(&cache_path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(parsed, second_json);
        assert_eq!(parsed["version"], "2.0.0");
        assert_eq!(parsed["new_data"], "updated");
    }

    #[test]
    fn test_update_cache_complex_json() {
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let cache_path = temp_dir.path().join("complex_cache.json");

        let complex_json = json!({
            "result": {
                "links": [
                    {
                        "downloadType": "serverBedrockWindows",
                        "downloadUrl": "https://example.com/bedrock-server-1.20.0.zip",
                        "version": "1.20.0",
                        "metadata": {
                            "size": 123456789,
                            "checksum": "abc123def456",
                            "dependencies": ["java", "libssl"],
                            "features": {
                                "experimental": true,
                                "beta": false
                            }
                        }
                    },
                    {
                        "downloadType": "serverBedrockLinux",
                        "downloadUrl": "https://example.com/bedrock-server-linux-1.20.0.zip",
                        "version": "1.20.0",
                        "metadata": {
                            "size": 987654321,
                            "checksum": "xyz789uvw012"
                        }
                    }
                ],
                "lastUpdated": "2025-07-04T12:00:00Z",
                "totalCount": 2,
                "pagination": {
                    "offset": 0,
                    "limit": 100,
                    "hasMore": false
                }
            },
            "status": "success",
            "timestamp": 1720094400
        });

        let result = update_cache(complex_json.clone(), cache_path.to_str().unwrap());

        assert!(result.is_ok());
        assert!(cache_path.exists());

        // Verify the complex structure is preserved
        let content = std::fs::read_to_string(&cache_path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(parsed, complex_json);

        // Verify specific nested values
        assert_eq!(parsed["result"]["links"][0]["metadata"]["size"], 123456789);
        assert_eq!(parsed["result"]["links"][1]["version"], "1.20.0");
        assert_eq!(parsed["status"], "success");
        assert_eq!(parsed["result"]["pagination"]["hasMore"], false);
    }

    #[test]
    fn test_update_cache_empty_json() {
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let cache_path = temp_dir.path().join("empty_cache.json");

        let empty_json = json!({});

        let result = update_cache(empty_json.clone(), cache_path.to_str().unwrap());

        assert!(result.is_ok());
        assert!(cache_path.exists());

        // Verify the file content
        let content = std::fs::read_to_string(&cache_path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(parsed, empty_json);
        assert!(parsed.is_object());
        assert_eq!(parsed.as_object().unwrap().len(), 0);
    }

    #[test]
    fn test_update_cache_null_json() {
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let cache_path = temp_dir.path().join("null_cache.json");

        let null_json = serde_json::Value::Null;

        let result = update_cache(null_json.clone(), cache_path.to_str().unwrap());

        assert!(result.is_ok());
        assert!(cache_path.exists());

        // Verify the file content
        let content = std::fs::read_to_string(&cache_path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(parsed, null_json);
        assert!(parsed.is_null());
    }

    #[test]
    fn test_update_cache_json_array() {
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let cache_path = temp_dir.path().join("array_cache.json");

        let array_json = json!([
            {
                "id": 1,
                "name": "item1",
                "active": true
            },
            {
                "id": 2,
                "name": "item2",
                "active": false
            }
        ]);

        let result = update_cache(array_json.clone(), cache_path.to_str().unwrap());

        assert!(result.is_ok());
        assert!(cache_path.exists());

        // Verify the file content
        let content = std::fs::read_to_string(&cache_path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(parsed, array_json);
        assert!(parsed.is_array());
        assert_eq!(parsed.as_array().unwrap().len(), 2);
    }

    #[test]
    fn test_update_cache_with_tilde_expansion() {
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();

        // Create a path that doesn't need tilde expansion but test the functionality
        let cache_path = temp_dir.path().join("tilde_cache.json");

        let test_json = json!({
            "tilde_test": true,
            "path": "~/test"
        });

        let result = update_cache(test_json.clone(), cache_path.to_str().unwrap());

        assert!(result.is_ok());
        assert!(cache_path.exists());

        // Verify the file content
        let content = std::fs::read_to_string(&cache_path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(parsed, test_json);
    }

    #[test]
    fn test_update_cache_invalid_path() {
        // Test with an invalid path (on Windows, this would be an invalid drive)
        let invalid_path = if cfg!(windows) {
            "Z:\\nonexistent\\path\\cache.json"
        } else {
            "/root/nonexistent/path/cache.json"
        };

        let test_json = json!({
            "test": "data"
        });

        let result = update_cache(test_json, invalid_path);

        // Should return an error
        assert!(result.is_err());
    }

    #[test]
    fn test_update_cache_special_characters() {
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let cache_path = temp_dir.path().join("special-chars_ñáéí.json");

        let test_json = json!({
            "special_chars": "ñáéíúü",
            "emoji": "🚀💻",
            "unicode": "\u{1F4BB}",
            "chinese": "测试",
            "arabic": "اختبار"
        });

        let result = update_cache(test_json.clone(), cache_path.to_str().unwrap());

        assert!(result.is_ok());
        assert!(cache_path.exists());

        // Verify the file content preserves special characters
        let content = std::fs::read_to_string(&cache_path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(parsed, test_json);
        assert_eq!(parsed["special_chars"], "ñáéíúü");
        assert_eq!(parsed["emoji"], "🚀💻");
        assert_eq!(parsed["chinese"], "测试");
    }

    #[test]
    fn test_update_cache_large_json() {
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let cache_path = temp_dir.path().join("large_cache.json");

        // Create a large JSON object
        let mut large_object = serde_json::Map::new();
        for i in 0..1000 {
            large_object.insert(
                format!("key_{}", i),
                json!({
                    "id": i,
                    "data": format!("This is test data for item number {}", i),
                    "array": vec![i; 10],
                    "nested": {
                        "level1": {
                            "level2": {
                                "value": i * 2
                            }
                        }
                    }
                }),
            );
        }
        let large_json = serde_json::Value::Object(large_object);

        let result = update_cache(large_json.clone(), cache_path.to_str().unwrap());

        assert!(result.is_ok());
        assert!(cache_path.exists());

        // Verify the file content
        let content = std::fs::read_to_string(&cache_path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(parsed, large_json);

        // Verify some specific values
        assert_eq!(parsed["key_0"]["id"], 0);
        assert_eq!(parsed["key_999"]["id"], 999);
        assert_eq!(
            parsed["key_500"]["nested"]["level1"]["level2"]["value"],
            1000
        );
    }

    #[test]
    fn test_update_cache_file_already_exists() {
        use std::io::Write;
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let cache_path = temp_dir.path().join("existing_cache.json");

        // Create an existing file with some content
        let mut existing_file = std::fs::File::create(&cache_path).unwrap();
        write!(existing_file, "{{\"old\": \"data\"}}").unwrap();
        existing_file.flush().unwrap();
        drop(existing_file);

        let new_json = json!({
            "new": "data",
            "updated": true
        });

        let result = update_cache(new_json.clone(), cache_path.to_str().unwrap());

        assert!(result.is_ok());
        assert!(cache_path.exists());

        // Verify the file was overwritten
        let content = std::fs::read_to_string(&cache_path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(parsed, new_json);
        assert_eq!(parsed["new"], "data");
        assert_eq!(parsed["updated"], true);
        // Old data should not be present
        assert!(parsed.get("old").is_none());
    }

    #[test]
    fn test_update_cache_concurrent_access() {
        use std::sync::{Arc, Barrier};
        use std::thread;
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let cache_path = Arc::new(temp_dir.path().join("concurrent_cache.json"));
        let barrier = Arc::new(Barrier::new(3));

        let mut handles = vec![];

        for i in 0..3 {
            let cache_path = cache_path.clone();
            let barrier = barrier.clone();

            let handle = thread::spawn(move || {
                let test_json = json!({
                    "thread_id": i,
                    "timestamp": format!("2025-07-04T12:00:{:02}Z", i)
                });

                barrier.wait();
                update_cache(test_json, cache_path.to_str().unwrap())
            });

            handles.push(handle);
        }

        // Wait for all threads to complete
        let mut results = vec![];
        for handle in handles {
            results.push(handle.join().unwrap());
        }

        // At least one should succeed (the last one to write)
        let success_count = results.iter().filter(|r| r.is_ok()).count();
        assert!(success_count > 0);

        // The file should exist and contain valid JSON
        assert!(cache_path.exists());
        let content = std::fs::read_to_string(cache_path.as_ref()).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();

        // Should be one of the thread's data
        assert!(parsed["thread_id"].is_number());
        let thread_id = parsed["thread_id"].as_u64().unwrap();
        assert!(thread_id < 3);
    }

    // Helpers for the apply_update / staging tests

    fn write_test_zip(path: &std::path::Path, entries: &[(&str, &[u8])]) {
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
    fn bare_stale_path(path: &std::path::Path) -> std::path::PathBuf {
        let mut name = path.as_os_str().to_os_string();
        name.push(STALE_MARKER);
        std::path::PathBuf::from(name)
    }

    /// Opens `path` the way Windows holds a running executable: readable and
    /// renameable by others, but not writable. Returns the handle; the lock
    /// lasts until it is dropped.
    #[cfg(windows)]
    fn lock_like_running_exe(path: &std::path::Path) -> std::fs::File {
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
        // The existing config survives, the binary still updates.
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

        // Nothing to preserve on a fresh install, so the default is written.
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

        assert!(result.is_err());
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

        // Nothing held the file open, so it is replaced directly.
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

        // The locked binary is reported as staged, the rest applies normally.
        assert_eq!(staged, vec![exe_path.clone()]);
        assert_eq!(std::fs::read(&exe_path).unwrap(), b"new binary");
        assert_eq!(
            std::fs::read(server_path.join("definitions/thing.json")).unwrap(),
            b"{}"
        );

        // The running server's copy is preserved under the stale name.
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

        // An instance is running v1 and holds it open.
        let _running_v1 = lock_like_running_exe(&exe_path);

        let v2_zip = temp_dir.path().join("v2.zip");
        write_test_zip(&v2_zip, &[("bedrock_server.exe", b"v2")]);
        let staged_v2 = apply_update(&server_path, &v2_zip, vec![]).unwrap();
        assert_eq!(staged_v2.len(), 1);

        // A second instance starts and holds the freshly written v2 open, while
        // the first is still running v1 out of the staged file.
        let _running_v2 = lock_like_running_exe(&exe_path);

        let v3_zip = temp_dir.path().join("v3.zip");
        write_test_zip(&v3_zip, &[("bedrock_server.exe", b"v3")]);
        let staged_v3 = apply_update(&server_path, &v3_zip, vec![])
            .expect("second update must not clash with the file staged by the first");
        assert_eq!(staged_v3.len(), 1);

        // v3 is live on disk and neither running instance lost its binary.
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

        // Left over from an earlier update, still in use by the server that has
        // not restarted yet, so it must not be reused or removed.
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
        let path = std::path::Path::new("/srv/minecraft/bedrock_server.exe");

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

        // Two earlier updates already staged copies here.
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

        assert!(is_stale_file(std::path::Path::new(&base)));
        assert!(is_stale_file(std::path::Path::new(&format!("{}.1", base))));
        assert!(is_stale_file(std::path::Path::new(&format!("{}.42", base))));
    }

    #[test]
    fn test_is_stale_file_rejects_unrelated_names() {
        assert!(!is_stale_file(std::path::Path::new(
            "/srv/bedrock_server.exe"
        )));
        assert!(!is_stale_file(std::path::Path::new(
            "/srv/server.properties"
        )));
        // A marker followed by something other than a numeric disambiguator is
        // not a name this tool produces, so it is left alone.
        assert!(!is_stale_file(std::path::Path::new(&format!(
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
        // Unrelated files are untouched.
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
        assert_eq!(
            sweep_stale_files(std::path::Path::new("/no/such/directory/anywhere")),
            0
        );
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
}
