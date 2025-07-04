use crate::args::{DownloadType, UpdateArgs};

pub fn update(args: UpdateArgs) {
    let web_json = get_json_from_web();

    if !web_json.is_null() {
        let cache_json = get_json_from_cache(&args.cache_path);

        let web_download_url = get_download_url_from_json(&web_json, &args.download_type).unwrap();
        let cache_download_url = get_download_url_from_json(&cache_json, &args.download_type)
            .unwrap_or("0.0.0".to_owned());

        println!("Current version in cache: {}", cache_download_url);
        println!("Version available on the web: {}", web_download_url);
        if args.force || web_download_url != cache_download_url {
            println!("New version available: {}", web_download_url);
            if let Some(zip_path) = fetch_update_zip(&web_download_url) {
                apply_update(args.server_path.clone(), &zip_path, args.exclude.clone());
                std::fs::remove_file(zip_path).unwrap();
                let cache_result = update_cache(web_json, &args.cache_path);
                if let Err(e) = cache_result {
                    eprintln!("Failed to update cache: {}", e);
                }
                println!("Update applied successfully.");
            }
        } else {
            println!(
                "You are already on the latest version: {}",
                cache_download_url
            );
        }
    }
}

fn get_json_from_web() -> serde_json::Value {
    println!("Fetching links from the web...");
    let links_url = "https://net-secondary.web.minecraft-services.net/api/v1.0/download/links";
    let response = reqwest::blocking::get(links_url);

    match response {
        Ok(resp) => {
            if let Ok(json) = resp.json::<serde_json::Value>() {
                json
            } else {
                eprintln!("Failed to parse JSON response.");
                serde_json::Value::Null
            }
        }
        Err(e) => {
            eprintln!("Failed to fetch links from {}: {}", links_url, e);
            serde_json::Value::Null
        }
    }
}

fn get_json_from_cache(cache_path: &str) -> serde_json::Value {
    println!("Reading cache from: {}", cache_path);
    let cache_path = shellexpand::tilde(cache_path).to_string();
    if let Ok(file) = std::fs::File::open(&cache_path) {
        let reader = std::io::BufReader::new(file);
        let json: serde_json::Value = serde_json::from_reader(reader).unwrap_or_else(|_| {
            eprintln!("Failed to read or parse cache file: {}", cache_path);
            serde_json::Value::Null
        });
        json
    } else {
        serde_json::Value::Null
    }
}

fn get_download_url_from_json(
    json: &serde_json::Value,
    download_type: &DownloadType,
) -> Option<String> {
    if json.is_null() {
        return None;
    }
    json.get("result")
        .and_then(|r| r.get("links"))
        .and_then(|l| {
            l.as_array().and_then(|arr| {
                arr.iter().find_map(|item| {
                    if item.get("downloadType") == Some(&download_type.to_string().into()) {
                        item.get("downloadUrl")
                            .and_then(|url| url.as_str())
                            .map(|s| s.to_string())
                    } else {
                        None
                    }
                })
            })
        })
}

fn fetch_update_zip(download_url: &str) -> Option<std::path::PathBuf> {
    let response = reqwest::blocking::get(download_url);
    match response {
        Ok(resp) => {
            if resp.status().is_success() {
                let file_name = download_url.split('/').last().unwrap_or("update.zip");
                let temp_dir = std::env::temp_dir();
                let file_path = temp_dir.join(file_name);
                let mut file = std::fs::File::create(&file_path).unwrap();
                std::io::copy(&mut resp.bytes().unwrap().as_ref(), &mut file).unwrap();

                println!("Downloaded update to: {}", file_path.display());
                Some(file_path)
            } else {
                eprintln!("Failed to download update: {}", resp.status());
                None
            }
        }
        Err(e) => {
            eprintln!("Failed to fetch update zip: {}", e);
            None
        }
    }
}

fn apply_update(server_path: String, zip_path: &std::path::Path, exclude: Vec<String>) {
    println!("Applying update from: {}", zip_path.display());
    println!("Excluded files: {:?}", exclude);

    let mut archive = zip::ZipArchive::new(std::fs::File::open(zip_path).unwrap()).unwrap();
    let server_path = shellexpand::tilde(&server_path).to_string();
    let server_path = std::path::PathBuf::from(server_path);
    let server_path = server_path.canonicalize().unwrap();
    let exclude_set: std::collections::HashSet<_> = exclude.iter().map(|s| s.to_string()).collect();
    for i in 0..archive.len() {
        let mut file = archive.by_index(i).unwrap();
        let out_path = file.enclosed_name().unwrap();

        if exclude_set.contains(out_path.to_str().unwrap()) {
            println!("Skipping excluded file: {}", out_path.display());
            continue;
        }

        let out_path = server_path.join(out_path);
        if (*file.name()).ends_with('/') {
            std::fs::create_dir_all(&out_path).unwrap();
            continue;
        }

        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }

        let mut outfile = std::fs::File::create(&out_path).unwrap();
        std::io::copy(&mut file, &mut outfile).unwrap();
    }
}

fn update_cache(web_json: serde_json::Value, cache_path: &str) -> std::io::Result<()> {
    let cache_path = shellexpand::tilde(cache_path).to_string();
    std::fs::create_dir_all(std::path::Path::new(&cache_path).parent().unwrap())?;
    let file = std::fs::File::create(cache_path)?;
    serde_json::to_writer(file, &web_json)?;
    Ok(())
}
