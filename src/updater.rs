use crate::args::{DownloadType, UpdateArgs};

pub fn update(args: UpdateArgs) {
    let web_json = get_json_from_web();
    if web_json.is_null() {
        return;
    }

    let cache_json = get_json_from_cache(&args.cache_path);
    let web_download_url = get_download_url_from_json(&web_json, &args.download_type).unwrap();
    let cache_download_url =
        get_download_url_from_json(&cache_json, &args.download_type).unwrap_or("0.0.0".to_owned());

    println!("Current version in cache: {}", cache_download_url);
    println!("Version available on the web: {}", web_download_url);

    if !args.force && web_download_url == cache_download_url {
        println!(
            "You are already on the latest version: {}",
            cache_download_url
        );
        return;
    }

    println!("New version available: {}", web_download_url);
    let Some(zip_path) = fetch_update_zip(&web_download_url) else {
        return;
    };

    apply_update(args.server_path, &zip_path, args.exclude);
    std::fs::remove_file(zip_path).unwrap();

    if let Err(e) = update_cache(web_json, &args.cache_path) {
        eprintln!("Failed to update cache: {}", e);
    }
    println!("Update applied successfully.");
}

fn get_json_from_web() -> serde_json::Value {
    println!("Fetching links from the web...");
    let links_url = "https://net-secondary.web.minecraft-services.net/api/v1.0/download/links";

    reqwest::blocking::get(links_url)
        .and_then(|resp| resp.json::<serde_json::Value>())
        .unwrap_or_else(|e| {
            eprintln!("Failed to fetch or parse JSON from {}: {}", links_url, e);
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

fn apply_update(server_path: String, zip_path: &std::path::Path, exclude: Vec<String>) {
    println!("Applying update from: {}", zip_path.display());
    println!("Excluded files: {:?}", exclude);

    let mut archive = zip::ZipArchive::new(std::fs::File::open(zip_path).unwrap()).unwrap();
    let server_path = std::path::PathBuf::from(shellexpand::tilde(&server_path).to_string());
    let exclude_set: std::collections::HashSet<_> = exclude.into_iter().collect();

    for i in 0..archive.len() {
        let mut file = archive.by_index(i).unwrap();
        let out_path = match file.enclosed_name() {
            Some(path) => path,
            None => continue,
        };

        if exclude_set.contains(out_path.to_str().unwrap_or("")) {
            println!("Skipping excluded file: {}", out_path.display());
            continue;
        }

        let out_path = server_path.join(out_path);

        if file.is_dir() {
            std::fs::create_dir_all(&out_path).unwrap();
        } else {
            if let Some(parent) = out_path.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            let mut outfile = std::fs::File::create(&out_path).unwrap();
            std::io::copy(&mut file, &mut outfile).unwrap();
        }
    }
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
