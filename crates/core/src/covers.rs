//! Local-first game covers. Only image bytes cross IPC; arbitrary file URLs and CSP
//! file scopes are not required. Requests are bounded by the visible-image queue.
use anyhow::{bail, Result};
use reqwest::blocking::Client;
use serde::Serialize;
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
    sync::OnceLock,
    time::{Duration, Instant},
};

const MAX_IMAGE_BYTES: usize = 2 * 1024 * 1024;
static IMAGE_CLIENT: OnceLock<std::result::Result<Client, String>> = OnceLock::new();
fn image_client() -> Result<Client> {
    IMAGE_CLIENT
        .get_or_init(|| {
            Client::builder()
                .connect_timeout(Duration::from_secs(2))
                .timeout(Duration::from_secs(3))
                .pool_max_idle_per_host(8)
                .pool_idle_timeout(Duration::from_secs(60))
                .user_agent("Mozilla/5.0 SteamToolsNext/0.2.4")
                .build()
                .map_err(|error| error.to_string())
        })
        .as_ref()
        .cloned()
        .map_err(|error| anyhow::anyhow!(error.clone()))
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GameCover {
    pub app_id: u32,
    pub data_url: String,
    pub source: &'static str,
}

pub fn load_game_cover(
    steam_root: Option<&Path>,
    cache_dir: &Path,
    app_id: u32,
    language: &str,
    preferred: &[String],
) -> Result<Option<GameCover>> {
    if let Some(cover) = load_cached_game_cover(steam_root, cache_dir, app_id, language)? {
        return Ok(Some(cover));
    }
    let path = cache_dir.join(format!("{app_id}.img"));
    let client = image_client()?;
    let mut urls = cover_candidates(app_id, preferred);
    // Real URLs supplied by Store/catalog are first. Template URLs are a fallback,
    // not an assertion that the game still uses Steam's legacy asset layout.
    for url in urls.iter().take(2) {
        if let Some(bytes) = fetch_image(&client, url) {
            return save_network_cover(cache_dir, &path, app_id, bytes);
        }
    }
    if let Some(url) = resolve_store_header(&client, app_id, language) {
        if !urls.contains(&url) {
            urls.insert(2.min(urls.len()), url);
        }
    }
    for url in urls.iter().skip(2) {
        if let Some(bytes) = fetch_image(&client, url) {
            return save_network_cover(cache_dir, &path, app_id, bytes);
        }
    }
    Ok(None)
}

/// Separate disk-only stage: stalled CDN requests never occupy this lane.
pub fn load_cached_game_cover(
    steam_root: Option<&Path>,
    cache_dir: &Path,
    app_id: u32,
    language: &str,
) -> Result<Option<GameCover>> {
    if app_id == 0 {
        bail!("AppID must be positive");
    }
    if let Some(root) = steam_root {
        for path in local_cover_candidates(root, app_id, language) {
            if let Some(cover) = read_cover(&path, app_id, "localSteam") {
                return Ok(Some(cover));
            }
        }
    }
    let path = cache_dir.join(format!("{app_id}.img"));
    if let Some(cover) = read_cover(&path, app_id, "diskCache") {
        return Ok(Some(cover));
    }
    Ok(None)
}

/// Achievement color/gray artwork has an independent content-addressed cache.
/// Steam's newer CommunityImages base is attempted before its retired layout.
pub fn load_achievement_image(
    cache_dir: &Path,
    app_id: u32,
    image: &str,
    fallback_image: Option<&str>,
) -> Result<Option<GameCover>> {
    let primary = achievement_image_name(app_id, image)?;
    let fallback = fallback_image
        .and_then(|value| achievement_image_name(app_id, value).ok())
        .filter(|name| name != &primary);
    if let Some(cover) = read_cover(
        &cache_dir.join(format!("{app_id}-{primary}.img")),
        app_id,
        "diskCache",
    ) {
        return Ok(Some(cover));
    }
    let client = image_client()?;
    let deadline = Instant::now() + Duration::from_secs(6);
    // Missing gray artwork tries the modern color URL next, before secondary
    // CDNs. All attempts share one six-second deadline rather than 3s per URL.
    for (name, is_fallback, url) in
        achievement_image_attempts(app_id, &primary, fallback.as_deref())
    {
        let path = cache_dir.join(format!("{app_id}-{name}.img"));
        if let Some(mut cover) = read_cover(&path, app_id, "diskCache") {
            if is_fallback {
                cover.source = "diskColorFallback";
            }
            return Ok(Some(cover));
        }
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            break;
        };
        if let Some(bytes) =
            fetch_image_with_timeout(&client, &url, remaining.min(Duration::from_secs(3)))
        {
            let mut cover = save_network_cover(cache_dir, &path, app_id, bytes)?;
            if is_fallback {
                if let Some(cover) = &mut cover {
                    cover.source = "networkColorFallback";
                }
            }
            return Ok(cover);
        }
    }
    Ok(None)
}

pub fn load_cached_achievement_image(
    cache_dir: &Path,
    app_id: u32,
    image: &str,
) -> Result<Option<GameCover>> {
    let name = achievement_image_name(app_id, image)?;
    Ok(read_cover(
        &cache_dir.join(format!("{app_id}-{name}.img")),
        app_id,
        "diskCache",
    ))
}

fn achievement_image_name(app_id: u32, image: &str) -> Result<String> {
    if app_id == 0 {
        bail!("AppID must be positive");
    }
    let value = image.trim();
    let name = if value.contains("://") {
        let url = reqwest::Url::parse(value)?;
        if !allowed_cover_url(value)
            || !url.host_str().is_some_and(|host| {
                host == "steamstatic.com"
                    || host.ends_with(".steamstatic.com")
                    || host == "steamcdn-a.akamaihd.net"
            })
        {
            bail!("Achievement image source is invalid");
        }
        let prefix = format!("/community_assets/images/apps/{app_id}/");
        let legacy = format!("/steamcommunity/public/images/apps/{app_id}/");
        url.path()
            .strip_prefix(&prefix)
            .or_else(|| url.path().strip_prefix(&legacy))
            .ok_or_else(|| anyhow::anyhow!("Achievement image AppID/path mismatch"))?
            .to_string()
    } else {
        value.to_string()
    };
    if name.is_empty()
        || name.len() > 128
        || name.starts_with('.')
        || name.contains("..")
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        bail!("Achievement image name is invalid");
    }
    Ok(name)
}

fn achievement_image_candidates(app_id: u32, name: &str) -> Vec<String> {
    vec![
        format!(
            "https://shared.akamai.steamstatic.com/community_assets/images/apps/{app_id}/{name}"
        ),
        format!(
            "https://shared.fastly.steamstatic.com/community_assets/images/apps/{app_id}/{name}"
        ),
        format!(
            "https://cdn.cloudflare.steamstatic.com/community_assets/images/apps/{app_id}/{name}"
        ),
        format!(
            "https://cdn.akamai.steamstatic.com/steamcommunity/public/images/apps/{app_id}/{name}"
        ),
        format!("https://cdn.steamstatic.com/steamcommunity/public/images/apps/{app_id}/{name}"),
    ]
}
fn achievement_image_attempts(
    app_id: u32,
    primary: &str,
    fallback: Option<&str>,
) -> Vec<(String, bool, String)> {
    let primary_urls = achievement_image_candidates(app_id, primary);
    let fallback_urls = fallback.map(|name| achievement_image_candidates(app_id, name));
    let mut attempts = Vec::new();
    for (index, url) in primary_urls.into_iter().enumerate() {
        attempts.push((primary.to_string(), false, url));
        if let (Some(name), Some(urls)) = (fallback, &fallback_urls) {
            attempts.push((name.to_string(), true, urls[index].clone()));
        }
    }
    attempts
}

fn read_cover(path: &Path, app_id: u32, source: &'static str) -> Option<GameCover> {
    let metadata = fs::metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > MAX_IMAGE_BYTES as u64 {
        return None;
    }
    let bytes = fs::read(path).ok()?;
    Some(GameCover {
        app_id,
        data_url: image_data_url(&bytes)?,
        source,
    })
}

fn save_network_cover(
    cache_dir: &Path,
    path: &Path,
    app_id: u32,
    bytes: Vec<u8>,
) -> Result<Option<GameCover>> {
    let Some(data_url) = image_data_url(&bytes) else {
        return Ok(None);
    };
    // Cache failures must not hide a successfully fetched image.
    if fs::create_dir_all(cache_dir).is_ok() {
        let temp = path.with_extension(format!("tmp-{}", std::process::id()));
        if fs::write(&temp, &bytes).is_ok() {
            if path.exists() {
                let _ = fs::remove_file(path);
            }
            if fs::rename(&temp, path).is_err() {
                let _ = fs::remove_file(&temp);
            }
        }
    }
    Ok(Some(GameCover {
        app_id,
        data_url,
        source: "network",
    }))
}

fn fetch_image(client: &Client, url: &str) -> Option<Vec<u8>> {
    fetch_image_with_timeout(client, url, Duration::from_secs(3))
}
fn fetch_image_with_timeout(client: &Client, url: &str, timeout: Duration) -> Option<Vec<u8>> {
    if !allowed_cover_url(url) {
        return None;
    }
    let response = client
        .get(url)
        .timeout(timeout)
        .send()
        .ok()?
        .error_for_status()
        .ok()?;
    // Reject redirect destinations outside the narrow image-source allowlist.
    if !allowed_cover_url(response.url().as_str())
        || response
            .content_length()
            .is_some_and(|n| n > MAX_IMAGE_BYTES as u64)
    {
        return None;
    }
    let mut bytes = Vec::new();
    response
        .take((MAX_IMAGE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .ok()?;
    image_data_url(&bytes)?;
    Some(bytes)
}

fn resolve_store_header(client: &Client, app_id: u32, language: &str) -> Option<String> {
    let json: serde_json::Value = client
        .get("https://store.steampowered.com/api/appdetails")
        .query(&[
            ("appids", app_id.to_string()),
            ("l", steam_language(language).to_string()),
        ])
        .send()
        .ok()?
        .error_for_status()
        .ok()?
        .json()
        .ok()?;
    let url = json
        .get(app_id.to_string())?
        .get("data")?
        .get("header_image")?
        .as_str()?;
    allowed_cover_url(url).then(|| url.to_string())
}

pub fn allowed_cover_url(url: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(url) else {
        return false;
    };
    url.scheme() == "https"
        && url.username().is_empty()
        && url.password().is_none()
        && url.port().is_none_or(|p| p == 443)
        && url.host_str().is_some_and(|host| {
            host == "steamstatic.com"
                || host.ends_with(".steamstatic.com")
                || host == "steamcdn-a.akamaihd.net"
                || host == "shared.fastly.steamstatic.com"
                || host == "cdn.cloudflare.steamstatic.com"
                || host.ends_with(".heyboximg.com")
                || host == "heyboximg.com"
                || [
                    "cdn.max-c.com",
                    "img.max-c.com",
                    "img1.max-c.com",
                    "img2.max-c.com",
                    "img3.max-c.com",
                ]
                .contains(&host)
        })
}

pub fn cover_candidates(app_id: u32, preferred: &[String]) -> Vec<String> {
    let mut urls = Vec::new();
    for url in preferred.iter().take(12).cloned().chain([
        format!("https://shared.akamai.steamstatic.com/store_item_assets/steam/apps/{app_id}/header.jpg"),
        format!("https://cdn.akamai.steamstatic.com/steam/apps/{app_id}/header.jpg"),
        format!("https://cdn.cloudflare.steamstatic.com/steam/apps/{app_id}/header.jpg"),
        format!("https://cdn.steamstatic.com/steam/apps/{app_id}/header.jpg"),
    ]) {
        if allowed_cover_url(&url) && !urls.contains(&url) { urls.push(url); }
    }
    urls
}

fn steam_language(language: &str) -> &'static str {
    match language {
        "zh-CN" | "zh-Hans" | "schinese" => "schinese",
        "zh-TW" | "zh-Hant" | "tchinese" => "tchinese",
        "ja" | "japanese" => "japanese",
        "ko" | "koreana" => "koreana",
        "de" | "german" => "german",
        "fr" | "french" => "french",
        "es" | "spanish" => "spanish",
        "pt-BR" | "brazilian" => "brazilian",
        "ru" | "russian" => "russian",
        _ => "english",
    }
}

fn local_cover_candidates(steam_root: &Path, app_id: u32, language: &str) -> Vec<PathBuf> {
    let root = steam_root.join("appcache").join("librarycache");
    let mut files = Vec::new();
    for name in [
        format!("{app_id}_header.jpg"),
        format!("{app_id}_library_header.jpg"),
        format!("{app_id}_library_hero.jpg"),
    ] {
        let path = root.join(name);
        if path.is_file() {
            files.push(path);
        }
    }
    let app_root = root.join(app_id.to_string());
    collect_images(&app_root, 0, &mut files);
    let language = steam_language(language);
    files.sort_by_key(|path| {
        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_ascii_lowercase();
        let kind = if name.contains("header") {
            0
        } else if name.contains("hero") && !name.contains("blur") {
            1
        } else {
            9
        };
        let locale = if name.contains(language) {
            0
        } else if name.contains("english") {
            1
        } else {
            2
        };
        (kind, locale, name)
    });
    files.retain(|path| {
        path.file_name().is_some_and(|name| {
            let name = name.to_string_lossy();
            name.contains("header") || (name.contains("hero") && !name.contains("blur"))
        })
    });
    files
}

fn collect_images(root: &Path, depth: usize, files: &mut Vec<PathBuf>) {
    if depth > 2 || files.len() >= 128 {
        return;
    }
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.take(256).flatten() {
        let path = entry.path();
        // No symlink traversal outside the selected Steam cache subtree.
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            collect_images(&path, depth + 1, files);
        } else if kind.is_file() && files.len() < 128 {
            files.push(path);
        }
    }
}

fn image_data_url(bytes: &[u8]) -> Option<String> {
    if bytes.len() < 12 || bytes.len() > MAX_IMAGE_BYTES {
        return None;
    }
    let mime = if bytes.starts_with(&[0xff, 0xd8, 0xff]) && bytes.ends_with(&[0xff, 0xd9]) {
        "image/jpeg"
    } else if bytes.starts_with(b"\x89PNG\r\n\x1a\n") && bytes.windows(4).any(|w| w == b"IEND") {
        "image/png"
    } else if bytes.starts_with(b"RIFF")
        && &bytes[8..12] == b"WEBP"
        && u32::from_le_bytes(bytes[4..8].try_into().ok()?) as usize + 8 == bytes.len()
    {
        "image/webp"
    } else {
        return None;
    };
    Some(format!("data:{mime};base64,{}", base64(bytes)))
}

fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let value = ((chunk[0] as u32) << 16)
            | ((chunk.get(1).copied().unwrap_or(0) as u32) << 8)
            | chunk.get(2).copied().unwrap_or(0) as u32;
        out.push(TABLE[(value >> 18) as usize] as char);
        out.push(TABLE[((value >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            TABLE[((value >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[(value & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    const JPEG: &[u8] = &[0xff, 0xd8, 0xff, 0xe0, 1, 2, 3, 4, 5, 6, 0xff, 0xd9];
    #[test]
    fn local_hashed_language_header_precedes_network_and_disk() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("steam");
        let cache = temp.path().join("cache");
        let header = root.join("appcache/librarycache/42/hash/library_header_schinese.jpg");
        fs::create_dir_all(header.parent().unwrap()).unwrap();
        fs::write(header, JPEG).unwrap();
        let result = load_game_cover(Some(&root), &cache, 42, "zh-CN", &[])
            .unwrap()
            .unwrap();
        assert_eq!(result.source, "localSteam");
        assert!(result.data_url.starts_with("data:image/jpeg;base64,"));
        assert!(!cache.exists());
    }
    #[test]
    fn valid_disk_cache_returns_without_network() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("42.img"), JPEG).unwrap();
        assert_eq!(
            load_game_cover(None, temp.path(), 42, "en", &[])
                .unwrap()
                .unwrap()
                .source,
            "diskCache"
        );
    }
    #[test]
    fn rejects_html_truncated_and_oversized_payloads() {
        assert!(image_data_url(b"<html>challenge</html>").is_none());
        assert!(image_data_url(&JPEG[..10]).is_none());
        assert!(image_data_url(&vec![0; MAX_IMAGE_BYTES + 1]).is_none());
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"foo"), "Zm9v");
    }
    #[test]
    fn narrow_https_sources_only_and_deduped_candidates() {
        for url in [
            "http://cdn.steamstatic.com/a.jpg",
            "https://steamstatic.com.evil.test/a.jpg",
            "file:///C:/secret",
            "https://user:pw@cdn.steamstatic.com/a.jpg",
        ] {
            assert!(!allowed_cover_url(url));
        }
        let urls = cover_candidates(
            42,
            &["https://cdn.steamstatic.com/steam/apps/42/header.jpg".to_string()],
        );
        assert_eq!(
            urls.iter()
                .filter(|u| u.contains("//cdn.steamstatic.com/"))
                .count(),
            1
        );
    }
    #[test]
    fn achievement_modern_urls_and_color_gray_cache_do_not_collide() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("42-color.jpg.img"), JPEG).unwrap();
        assert!(load_cached_achievement_image(temp.path(), 42, "gray.jpg")
            .unwrap()
            .is_none());
        assert_eq!(
            load_cached_achievement_image(
                temp.path(),
                42,
                "https://cdn.steamstatic.com/steamcommunity/public/images/apps/42/color.jpg"
            )
            .unwrap()
            .unwrap()
            .source,
            "diskCache"
        );
        let urls = achievement_image_candidates(42, "color.jpg");
        assert!(urls[0].contains("/community_assets/images/apps/42/color.jpg"));
        let attempts = achievement_image_attempts(42, "gray.jpg", Some("color.jpg"));
        assert!(!attempts[0].1);
        assert!(attempts[1].1);
        assert!(attempts[1].2.ends_with("/color.jpg"));
        for name in [
            "../secret",
            "https://evil.test/a.jpg",
            "https://shared.akamai.steamstatic.com/community_assets/images/apps/99/a.jpg",
        ] {
            assert!(achievement_image_name(42, name).is_err());
        }
    }
    #[test]
    #[ignore = "read-only warm-cache probe; requires STN_ICON_FIXTURE and STN_ICON_APP_ID"]
    fn benchmark_real_achievement_image_cache() {
        let source = PathBuf::from(std::env::var("STN_ICON_FIXTURE").expect("STN_ICON_FIXTURE"));
        let app_id = std::env::var("STN_ICON_APP_ID")
            .expect("STN_ICON_APP_ID")
            .parse()
            .unwrap();
        let cache = tempfile::tempdir().unwrap();
        fs::copy(
            source,
            cache.path().join(format!("{app_id}-fixture.jpg.img")),
        )
        .unwrap();
        let started = std::time::Instant::now();
        let image = load_cached_achievement_image(cache.path(), app_id, "fixture.jpg")
            .unwrap()
            .unwrap();
        println!("{{\"appId\":{app_id},\"warmIconCacheMs\":{:.3},\"encodedBytes\":{},\"source\":\"{}\"}}",started.elapsed().as_secs_f64()*1000.,image.data_url.len(),image.source);
        assert!(image.data_url.starts_with("data:image/jpeg;base64,"));
    }
    #[test]
    #[ignore = "read-only local machine probe; requires STN_COVER_STEAM_ROOT and STN_COVER_APP_ID"]
    fn benchmark_real_local_cover() {
        let root =
            PathBuf::from(std::env::var("STN_COVER_STEAM_ROOT").expect("STN_COVER_STEAM_ROOT"));
        let app_id = std::env::var("STN_COVER_APP_ID")
            .expect("STN_COVER_APP_ID")
            .parse()
            .unwrap();
        let cache = tempfile::tempdir().unwrap();
        let started = std::time::Instant::now();
        let local = load_cached_game_cover(Some(&root), cache.path(), app_id, "zh-CN")
            .unwrap()
            .expect("local header");
        let local_ms = started.elapsed().as_secs_f64() * 1000.;
        let file = local_cover_candidates(&root, app_id, "zh-CN")
            .into_iter()
            .find(|p| read_cover(p, app_id, "localSteam").is_some())
            .unwrap();
        fs::copy(file, cache.path().join(format!("{app_id}.img"))).unwrap();
        let started = std::time::Instant::now();
        let disk = load_cached_game_cover(None, cache.path(), app_id, "zh-CN")
            .unwrap()
            .expect("disk cache");
        let disk_ms = started.elapsed().as_secs_f64() * 1000.;
        println!("{{\"appId\":{app_id},\"firstLocalMs\":{local_ms:.3},\"warmDiskMs\":{disk_ms:.3},\"encodedBytes\":{},\"localSource\":\"{}\",\"diskSource\":\"{}\"}}", local.data_url.len(), local.source, disk.source);
        assert_eq!(local.data_url, disk.data_url);
    }
}
