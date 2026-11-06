//! Remote Steam catalogue and OpenSteamTool Lua generation.
//!
//! Networking is blocking by design; the Tauri adapter runs it behind
//! `spawn_blocking`. Parsing and Lua rendering
//! are kept as pure helpers so they can be tested without live services.

use anyhow::{bail, Context, Result};
use reqwest::blocking::{Client, Response};
use reqwest::header::{ACCEPT, ACCEPT_LANGUAGE, USER_AGENT};
use serde::{de::IgnoredAny, Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const DEFAULT_DEPOT_KEYS_URL: &str = "https://api.993499094.xyz/depotkeys.json";
const DEFAULT_APP_TOKENS_URL: &str = "https://api.993499094.xyz/appaccesstokens.json";
const STEAMCMD_INFO_URL: &str = "https://api.steamcmd.net/v1/info/{appid}";
const HEIHE_SEARCH_URL: &str = "https://api.xiaoheihe.cn/game/search/?q={query}&limit=10&offset=0";
const HEIHE_DETAIL_URL: &str = "https://api.xiaoheihe.cn/game/web/get_game_detail/?appid={appid}";
const HEIHE_RANK_URL: &str = "https://api.xiaoheihe.cn/game/web/all_recommend/games/?os_type=web&version=999.0.0&show_type=popular&limit={limit}&offset={offset}";
const STEAM_APP_DETAILS_URL: &str =
    "https://store.steampowered.com/api/appdetails?appids={appid}&l={lang}";
const STEAM_SEARCH_URL: &str =
    "https://store.steampowered.com/api/storesearch/?term={query}&cc={cc}&l={lang}";
const DEFAULT_TIMEOUT_SECS: u64 = 20;
const MAX_JSON_BYTES: u64 = 32 * 1024 * 1024;
const MAX_KEY_CACHE_BYTES: u64 = 768 * 1024 * 1024;
const MAX_SEARCH_RESULTS: usize = 10;
const MAX_RANK_POOL: usize = 120;
const MAX_DLC_QUERIES: usize = 512;
const MAX_LUA_BYTES: usize = 8 * 1024 * 1024;
const DLC_CONCURRENCY: usize = 6;
const DLC_BATCH_TIMEOUT: Duration = Duration::from_secs(25);
const DLC_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
static HTTP_CLIENTS: OnceLock<Mutex<HashMap<u64, Client>>> = OnceLock::new();
static CACHE_GATES: OnceLock<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>> = OnceLock::new();
static KEY_SELECTIONS: OnceLock<Mutex<HashMap<PathBuf, Arc<Mutex<KeySelections>>>>> =
    OnceLock::new();
#[derive(Default)]
struct KeySelections {
    fingerprint: Option<(u64, SystemTime)>,
    values: HashMap<u32, Option<String>>,
}

pub(crate) fn shared_catalog_http_client(timeout_seconds: u64) -> Result<Client> {
    let mut clients = HTTP_CLIENTS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .map_err(|_| anyhow::anyhow!("catalog client cache poisoned"))?;
    if let Some(client) = clients.get(&timeout_seconds) {
        return Ok(client.clone());
    }
    let client = Client::builder()
        .connect_timeout(Duration::from_secs(timeout_seconds.min(5)))
        .timeout(Duration::from_secs(timeout_seconds))
        .pool_max_idle_per_host(8)
        .pool_idle_timeout(Duration::from_secs(60))
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()?;
    if clients.len() >= 16 {
        clients.clear();
    }
    clients.insert(timeout_seconds, client.clone());
    Ok(client)
}

fn cache_gate(path: &Path) -> Result<Arc<Mutex<()>>> {
    let mut gates = CACHE_GATES
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .map_err(|_| anyhow::anyhow!("catalog cache gate poisoned"))?;
    if gates.len() > 512 {
        gates.retain(|_, gate| Arc::strong_count(gate) > 1);
    }
    Ok(gates
        .entry(path.to_path_buf())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone())
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", default)]
pub struct CatalogConfig {
    pub cache_dir: PathBuf,
    pub depot_keys_url: String,
    pub app_tokens_url: String,
    pub steamcmd_info_url_template: String,
    pub timeout_seconds: u64,
    pub user_agent: String,
}

impl Default for CatalogConfig {
    fn default() -> Self {
        Self {
            cache_dir: crate::settings::app_data_dir()
                .unwrap_or_else(|_| std::env::temp_dir().join("SteamToolsNext"))
                .join("cache")
                .join("catalog-v1"),
            depot_keys_url: DEFAULT_DEPOT_KEYS_URL.into(),
            app_tokens_url: DEFAULT_APP_TOKENS_URL.into(),
            steamcmd_info_url_template: STEAMCMD_INFO_URL.into(),
            timeout_seconds: DEFAULT_TIMEOUT_SECS,
            user_agent: format!("SteamTools-Next/{}", env!("CARGO_PKG_VERSION")),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CatalogGame {
    pub app_id: u32,
    pub name: String,
    pub cover_url: String,
    pub cover_candidates: Vec<String>,
    pub release_date: String,
    pub followers: u64,
    pub source: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CatalogRankings {
    pub popular: Vec<CatalogGame>,
    pub recommended: Vec<CatalogGame>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DepotRecord {
    pub depot_id: u32,
    pub manifest_id: Option<String>,
    pub dlc_app_id: Option<u32>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DepotQuery {
    pub app_id: u32,
    pub app_name: String,
    pub depots: Vec<DepotRecord>,
    pub dlc_app_ids: Vec<u32>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", default)]
pub struct LuaGenerationOptions {
    pub include_dlc: bool,
    pub pin_manifest: bool,
    pub fetch_names: bool,
}

impl Default for LuaGenerationOptions {
    fn default() -> Self {
        Self {
            include_dlc: true,
            pin_manifest: false,
            fetch_names: false,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct LuaGenerationResult {
    pub app_id: u32,
    pub app_name: String,
    pub lua: String,
    pub depot_count: usize,
    pub dlc_count: usize,
    pub pinned_manifest_count: usize,
    pub skipped_keyless: Vec<u32>,
    pub incomplete_dlc: Vec<u32>,
    pub steps: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct KeyCacheUpdate {
    pub cache_dir: String,
    pub depot_keys_path: String,
    pub app_tokens_path: String,
    pub depot_keys_bytes: u64,
    pub app_tokens_bytes: u64,
    pub updated_at: u64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogProgress {
    pub stage: &'static str,
    pub done: usize,
    pub total: usize,
    pub elapsed_ms: u64,
}

pub struct CatalogClient {
    config: CatalogConfig,
    http: Client,
}

impl CatalogClient {
    pub fn new(config: CatalogConfig) -> Result<Self> {
        validate_http_url(&config.depot_keys_url)?;
        validate_http_url(&config.app_tokens_url)?;
        validate_http_url(&config.steamcmd_info_url_template)?;
        if !config.steamcmd_info_url_template.contains("{appid}") {
            bail!("SteamCMD URL must contain {{appid}}");
        }
        if config.timeout_seconds == 0 || config.timeout_seconds > 300 {
            bail!("catalog timeout must be between 1 and 300 seconds");
        }
        if config.user_agent.trim().is_empty() {
            bail!("catalog user-agent must not be empty");
        }
        let http = shared_catalog_http_client(config.timeout_seconds)
            .context("could not create the catalogue HTTP client")?;
        Ok(Self { config, http })
    }

    pub fn config(&self) -> &CatalogConfig {
        &self.config
    }

    /// Search by exact AppID or by localized game name.
    pub fn search(&self, query: &str, locale: &str) -> Result<Vec<CatalogGame>> {
        let query = query.trim();
        if query.is_empty() {
            bail!("search query is empty");
        }
        if query.len() > 256 || query.contains('\0') {
            bail!("search query is too long or contains NUL");
        }
        if let Ok(app_id) = query.parse::<u32>() {
            if app_id == 0 {
                bail!("AppID must be greater than zero");
            }
            return Ok(vec![self.game_details(app_id, locale)?]);
        }

        let chinese = locale.to_ascii_lowercase().starts_with("zh");
        if chinese {
            let url = HEIHE_SEARCH_URL.replace("{query}", &url_encode(query));
            if let Ok(bytes) = self.get_cached_json(
                &url,
                Duration::from_secs(3600),
                Duration::from_secs(self.config.timeout_seconds),
            ) {
                let games = parse_heihe_search(&bytes, query)?;
                if !games.is_empty() {
                    return Ok(games);
                }
            }
        }
        for (cc, lang) in if chinese {
            [("cn", "schinese"), ("us", "english")].as_slice()
        } else {
            [("us", "english")].as_slice()
        } {
            let url = STEAM_SEARCH_URL
                .replace("{query}", &url_encode(query))
                .replace("{cc}", cc)
                .replace("{lang}", lang);
            if let Ok(bytes) = self.get_cached_json(
                &url,
                Duration::from_secs(3600),
                Duration::from_secs(self.config.timeout_seconds),
            ) {
                let games = parse_store_search(&bytes)?;
                if !games.is_empty() {
                    return Ok(games);
                }
            }
        }
        Ok(Vec::new())
    }

    pub fn game_details(&self, app_id: u32, locale: &str) -> Result<CatalogGame> {
        validate_app_id(app_id)?;
        let chinese = locale.to_ascii_lowercase().starts_with("zh");
        for lang in if chinese {
            ["schinese", "english"].as_slice()
        } else {
            ["english"].as_slice()
        } {
            let url = STEAM_APP_DETAILS_URL
                .replace("{appid}", &app_id.to_string())
                .replace("{lang}", lang);
            if let Ok(bytes) = self.get_cached_json(
                &url,
                Duration::from_secs(3600),
                Duration::from_secs(self.config.timeout_seconds),
            ) {
                if let Some(game) = parse_app_details(&bytes, app_id)? {
                    return Ok(game);
                }
            }
        }
        if chinese {
            let url = HEIHE_DETAIL_URL.replace("{appid}", &app_id.to_string());
            if let Ok(bytes) = self.get_cached_json(
                &url,
                Duration::from_secs(3600),
                Duration::from_secs(self.config.timeout_seconds),
            ) {
                if let Some(game) = parse_heihe_detail(&bytes, app_id)? {
                    return Ok(game);
                }
            }
        }
        Ok(fallback_game(app_id))
    }

    /// Fetch the XiaoHeiHe popular pool and deterministically split it into
    /// popular and recommended sections. Deterministic ordering keeps GUI
    /// results reproducible for the same response set.
    pub fn rankings(&self) -> Result<CatalogRankings> {
        let mut pool = Vec::new();
        let mut seen = HashSet::new();
        for offset in (0..MAX_RANK_POOL).step_by(30) {
            let url = HEIHE_RANK_URL
                .replace("{limit}", &30usize.min(MAX_RANK_POOL - offset).to_string())
                .replace("{offset}", &offset.to_string());
            let bytes = match self.get_cached_json(
                &url,
                Duration::from_secs(3600),
                Duration::from_secs(self.config.timeout_seconds),
            ) {
                Ok(bytes) => bytes,
                Err(_) => continue,
            };
            for game in parse_heihe_rank(&bytes)? {
                if seen.insert(game.app_id) {
                    pool.push(game);
                }
            }
        }
        let popular: Vec<_> = pool.iter().take(10).cloned().collect();
        let mut rest: Vec<_> = pool.into_iter().skip(popular.len()).collect();
        rest.sort_by_key(|game| stable_recommendation_key(game.app_id));
        Ok(CatalogRankings {
            popular,
            recommended: rest.into_iter().take(10).collect(),
        })
    }

    pub fn query_depots(&self, app_id: u32) -> Result<DepotQuery> {
        self.query_depots_timeout(app_id, Duration::from_secs(self.config.timeout_seconds))
    }

    pub fn cached_depots(&self, app_id: u32) -> Result<Option<DepotQuery>> {
        validate_app_id(app_id)?;
        let url = self
            .config
            .steamcmd_info_url_template
            .replace("{appid}", &app_id.to_string());
        Ok(self
            .read_metadata_cache(&url, None)
            .and_then(|bytes| parse_steamcmd_info(&bytes, app_id).ok()))
    }

    fn query_depots_timeout(&self, app_id: u32, timeout: Duration) -> Result<DepotQuery> {
        validate_app_id(app_id)?;
        let url = self
            .config
            .steamcmd_info_url_template
            .replace("{appid}", &app_id.to_string());
        let bytes = self.get_cached_json(&url, Duration::from_secs(300), timeout)?;
        match parse_steamcmd_info(&bytes, app_id) {
            Ok(query) => Ok(query),
            Err(error) => {
                let _ = fs::remove_file(self.metadata_path(&url));
                Err(error)
            }
        }
    }

    pub fn ensure_key_cache(&self) -> Result<KeyCacheUpdate> {
        let depot = self.depot_keys_path();
        let tokens = self.app_tokens_path();
        if regular_file(&depot) && regular_file(&tokens) {
            return cache_receipt(&self.config.cache_dir, &depot, &tokens);
        }
        let gate = cache_gate(&self.config.cache_dir.join(".key-update"))?;
        let _guard = gate
            .lock()
            .map_err(|_| anyhow::anyhow!("key cache update poisoned"))?;
        if regular_file(&depot) && regular_file(&tokens) {
            return cache_receipt(&self.config.cache_dir, &depot, &tokens);
        }
        self.update_key_cache_inner()
    }

    /// Refresh both key files as one staged operation.  Both downloads and
    /// JSON roots are validated before either existing cache file is replaced.
    pub fn update_key_cache(&self) -> Result<KeyCacheUpdate> {
        let gate = cache_gate(&self.config.cache_dir.join(".key-update"))?;
        let _guard = gate
            .lock()
            .map_err(|_| anyhow::anyhow!("key cache update poisoned"))?;
        self.update_key_cache_inner()
    }

    fn update_key_cache_inner(&self) -> Result<KeyCacheUpdate> {
        create_cache_dir(&self.config.cache_dir)?;
        let (depot_bytes, token_bytes) = std::thread::scope(|scope| {
            let depot =
                scope.spawn(|| self.get_bytes(&self.config.depot_keys_url, MAX_KEY_CACHE_BYTES));
            let tokens =
                scope.spawn(|| self.get_bytes(&self.config.app_tokens_url, MAX_KEY_CACHE_BYTES));
            Ok::<_, anyhow::Error>((
                depot
                    .join()
                    .map_err(|_| anyhow::anyhow!("depot cache download worker failed"))??,
                tokens
                    .join()
                    .map_err(|_| anyhow::anyhow!("token cache download worker failed"))??,
            ))
        })?;
        validate_string_map_json(&depot_bytes, "depot key cache")?;
        validate_string_map_json(&token_bytes, "app token cache")?;
        let depot = self.depot_keys_path();
        let tokens = self.app_tokens_path();
        let gate = cache_gate(&self.config.cache_dir.join(".key-read-write"))?;
        let _guard = gate
            .lock()
            .map_err(|_| anyhow::anyhow!("key cache commit poisoned"))?;
        write_cache_pair(&depot, &depot_bytes, &tokens, &token_bytes)?;
        let stamp = unix_seconds()?;
        atomic_write(
            &self.config.cache_dir.join(".lastupdate"),
            stamp.to_string().as_bytes(),
        )?;
        cache_receipt(&self.config.cache_dir, &depot, &tokens)
    }

    pub fn generate_lua(
        &self,
        app_id: u32,
        options: &LuaGenerationOptions,
    ) -> Result<LuaGenerationResult> {
        self.generate_lua_with_progress(app_id, options, &|_| {})
    }

    pub fn generate_lua_with_progress(
        &self,
        app_id: u32,
        options: &LuaGenerationOptions,
        progress: &(dyn Fn(CatalogProgress) + Sync),
    ) -> Result<LuaGenerationResult> {
        validate_app_id(app_id)?;
        let started = Instant::now();
        let emit = |stage, done, total| {
            progress(CatalogProgress {
                stage,
                done,
                total,
                elapsed_ms: started.elapsed().as_millis().try_into().unwrap_or(u64::MAX),
            })
        };
        emit("queryMain", 0, 1);
        let mut steps = vec![format!("querying app {app_id}")];
        let main = self.query_depots(app_id)?;
        steps.push(format!(
            "found {} main depots and {} DLC entries",
            main.depots.len(),
            main.dlc_app_ids.len()
        ));
        if main.depots.is_empty() {
            bail!("Steam depot metadata is empty for AppID {app_id}");
        }
        emit("ensureKeys", 0, 1);
        self.ensure_key_cache()?;
        steps.push("key cache ready".into());

        let mut dlc_queries = Vec::new();
        let mut incomplete_dlc = Vec::new();
        if options.include_dlc {
            if main.dlc_app_ids.len() > MAX_DLC_QUERIES {
                bail!(
                    "AppID {app_id} exposes more than {MAX_DLC_QUERIES} DLC entries; narrow the request"
                );
            }
            emit("queryDlc", 0, main.dlc_app_ids.len());
            let done = AtomicUsize::new(0);
            for (dlc_id, result) in
                main.dlc_app_ids
                    .iter()
                    .zip(self.query_dlc_batch(&main.dlc_app_ids, &|| {
                        emit(
                            "queryDlc",
                            done.fetch_add(1, Ordering::Relaxed) + 1,
                            main.dlc_app_ids.len(),
                        );
                    })?)
            {
                match result {
                    Ok(query) => dlc_queries.push(query),
                    Err(error) => {
                        incomplete_dlc.push(*dlc_id);
                        steps.push(format!("DLC {dlc_id} metadata skipped: {error:#}"));
                    }
                }
            }
        }

        let mut key_ids = BTreeSet::new();
        let mut token_ids = BTreeSet::new();
        key_ids.insert(app_id);
        token_ids.insert(app_id);
        for depot in &main.depots {
            key_ids.insert(depot.depot_id);
        }
        for query in &dlc_queries {
            key_ids.insert(query.app_id);
            token_ids.insert(query.app_id);
            for depot in &query.depots {
                key_ids.insert(depot.depot_id);
            }
        }
        emit("lookupKeys", 0, 2);
        let gate = cache_gate(&self.config.cache_dir.join(".key-read-write"))?;
        let (depot_keys, app_tokens) = {
            let _guard = gate
                .lock()
                .map_err(|_| anyhow::anyhow!("key cache read poisoned"))?;
            let depot_keys = lookup_json_string_keys(&self.depot_keys_path(), &key_ids)?;
            emit("lookupKeys", 1, 2);
            let app_tokens = lookup_json_string_keys(&self.app_tokens_path(), &token_ids)?;
            emit("lookupKeys", 2, 2);
            (depot_keys, app_tokens)
        };
        steps.push(format!(
            "matched {} depot keys and {} app tokens",
            depot_keys.len(),
            app_tokens.len()
        ));
        emit("renderLua", 0, 1);
        let render = render_lua(&main, &dlc_queries, &depot_keys, &app_tokens, options)?;
        steps.push(format!("generated {} Lua bytes", render.lua.len()));
        emit("complete", 1, 1);
        Ok(LuaGenerationResult {
            app_id,
            app_name: main.app_name,
            lua: render.lua,
            depot_count: render.depot_count,
            dlc_count: render.dlc_count,
            pinned_manifest_count: render.pinned_manifest_count,
            skipped_keyless: render.skipped_keyless,
            incomplete_dlc,
            steps,
        })
    }

    fn query_dlc_batch(
        &self,
        ids: &[u32],
        completed: &(dyn Fn() + Sync),
    ) -> Result<Vec<Result<DepotQuery, String>>> {
        let next = AtomicUsize::new(0);
        let deadline = Instant::now() + DLC_BATCH_TIMEOUT;
        let results = Mutex::new(vec![None; ids.len()]);
        std::thread::scope(|scope| {
            for _ in 0..ids.len().min(DLC_CONCURRENCY) {
                scope.spawn(|| loop {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    if index >= ids.len() {
                        break;
                    }
                    let result =
                        if let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
                            self.query_depots_timeout(
                                ids[index],
                                remaining
                                    .min(DLC_REQUEST_TIMEOUT)
                                    .min(Duration::from_secs(self.config.timeout_seconds)),
                            )
                            .map_err(|error| format!("{error:#}"))
                        } else {
                            Err("DLC metadata batch time budget reached; not queried".into())
                        };
                    if let Ok(mut results) = results.lock() {
                        results[index] = Some(result);
                    }
                    completed();
                });
            }
        });
        Ok(results
            .into_inner()
            .map_err(|_| anyhow::anyhow!("DLC metadata result queue poisoned"))?
            .into_iter()
            .map(|result| {
                result.unwrap_or_else(|| Err("DLC metadata worker returned no result".into()))
            })
            .collect())
    }

    fn depot_keys_path(&self) -> PathBuf {
        self.config.cache_dir.join("depotkeys.json")
    }

    fn app_tokens_path(&self) -> PathBuf {
        self.config.cache_dir.join("appaccesstokens.json")
    }

    fn get_bytes(&self, url: &str, limit: u64) -> Result<Vec<u8>> {
        self.get_bytes_timeout(url, limit, Duration::from_secs(self.config.timeout_seconds))
    }

    fn get_bytes_timeout(&self, url: &str, limit: u64, timeout: Duration) -> Result<Vec<u8>> {
        validate_http_url(url)?;
        let response = self
            .http
            .get(url)
            .timeout(timeout)
            .header(ACCEPT, "application/json,text/plain,*/*")
            .header(ACCEPT_LANGUAGE, "zh-CN,zh;q=0.9,en;q=0.7")
            .header(USER_AGENT, &self.config.user_agent)
            .send()
            .with_context(|| format!("request failed: {url}"))?;
        read_limited_response(response, limit, url)
    }

    fn metadata_path(&self, url: &str) -> PathBuf {
        self.config
            .cache_dir
            .join("metadata-v2")
            .join(format!("{:x}.json", Sha256::digest(url.as_bytes())))
    }
    fn read_metadata_cache(&self, url: &str, max_age: Option<Duration>) -> Option<Vec<u8>> {
        let path = self.metadata_path(url);
        let metadata = fs::symlink_metadata(&path).ok()?;
        if !metadata.is_file()
            || metadata.file_type().is_symlink()
            || metadata.len() > MAX_JSON_BYTES
        {
            return None;
        }
        if max_age.is_some_and(|age| {
            metadata
                .modified()
                .ok()
                .and_then(|modified| modified.elapsed().ok())
                .is_none_or(|elapsed| elapsed > age)
        }) {
            return None;
        }
        let bytes = fs::read(path).ok()?;
        let value: Value = serde_json::from_slice(&bytes).ok()?;
        value.is_object().then_some(bytes)
    }
    fn get_cached_json(&self, url: &str, ttl: Duration, timeout: Duration) -> Result<Vec<u8>> {
        if let Some(bytes) = self.read_metadata_cache(url, Some(ttl)) {
            return Ok(bytes);
        }
        let deadline = Instant::now() + timeout;
        let gate = cache_gate(&self.metadata_path(url))?;
        let _guard = loop {
            match gate.try_lock() {
                Ok(guard) => break guard,
                Err(std::sync::TryLockError::Poisoned(_)) => bail!("metadata cache queue poisoned"),
                Err(std::sync::TryLockError::WouldBlock) => {
                    if Instant::now() >= deadline {
                        bail!(
                            "metadata request time budget reached waiting for an in-flight request"
                        );
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        };
        if let Some(bytes) = self.read_metadata_cache(url, Some(ttl)) {
            return Ok(bytes);
        }
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .context("metadata request time budget reached")?;
        let bytes = self.get_bytes_timeout(url, MAX_JSON_BYTES, remaining)?;
        let value: Value =
            serde_json::from_slice(&bytes).context("metadata response is not JSON")?;
        if !value.is_object() {
            bail!("metadata response must be a JSON object");
        }
        let path = self.metadata_path(url);
        if create_cache_dir(path.parent().unwrap()).is_ok() {
            let _ = atomic_write(&path, &bytes);
        }
        Ok(bytes)
    }
}

#[derive(Debug)]
struct RenderedLua {
    lua: String,
    depot_count: usize,
    dlc_count: usize,
    pinned_manifest_count: usize,
    skipped_keyless: Vec<u32>,
}

fn render_lua(
    main: &DepotQuery,
    dlc_queries: &[DepotQuery],
    depot_keys: &HashMap<u32, String>,
    app_tokens: &HashMap<u32, String>,
    options: &LuaGenerationOptions,
) -> Result<RenderedLua> {
    let mut lines = vec!["-- Generated by SteamTools Next".to_string(), String::new()];
    let main_name = if options.fetch_names {
        sanitize_lua_comment(&main.app_name)
    } else {
        String::new()
    };
    push_app_line(
        &mut lines,
        main.app_id,
        depot_keys.get(&main.app_id),
        &main_name,
    );
    let mut matched_items = usize::from(depot_keys.contains_key(&main.app_id));
    let dlc_ids: HashSet<u32> = main.dlc_app_ids.iter().copied().collect();
    let content_names: HashMap<u32, String> = dlc_queries
        .iter()
        .map(|query| (query.app_id, sanitize_lua_comment(&query.app_name)))
        .collect();
    let main_depot_ids: HashSet<u32> = main.depots.iter().map(|d| d.depot_id).collect();
    let mut depot_count = 0;
    let mut pinned = 0;
    let mut skipped_keyless = BTreeSet::new();
    let mut emitted = HashSet::new();

    for depot in &main.depots {
        if !options.include_dlc && dlc_ids.contains(&depot.depot_id) {
            continue;
        }
        let Some(key) = depot_keys.get(&depot.depot_id) else {
            if depot.dlc_app_id.is_some() || dlc_ids.contains(&depot.depot_id) {
                skipped_keyless.insert(depot.depot_id);
            }
            continue;
        };
        if !valid_depot_key(key) || !emitted.insert(("app", depot.depot_id)) {
            continue;
        }
        let name = if options.fetch_names {
            content_names
                .get(&depot.depot_id)
                .map(String::as_str)
                .unwrap_or("")
        } else {
            ""
        };
        push_app_line(&mut lines, depot.depot_id, Some(key), name);
        depot_count += 1;
        matched_items += 1;
        if options.pin_manifest {
            if let Some(manifest) = depot
                .manifest_id
                .as_deref()
                .filter(|value| valid_manifest(value))
            {
                lines.push(format!(
                    "setManifestid({},\"{}\",0)",
                    depot.depot_id, manifest
                ));
                pinned += 1;
            }
        }
    }
    if let Some(token) = app_tokens
        .get(&main.app_id)
        .filter(|value| valid_app_token(value))
    {
        lines.push(format!("addtoken({}, \"{}\")", main.app_id, token));
        matched_items += 1;
    }

    let mut dlc_count = 0;
    if options.include_dlc {
        for query in dlc_queries {
            let dlc_name = if options.fetch_names {
                sanitize_lua_comment(&query.app_name)
            } else {
                String::new()
            };
            if !main_depot_ids.contains(&query.app_id) {
                if let Some(key) = depot_keys
                    .get(&query.app_id)
                    .filter(|value| valid_depot_key(value))
                {
                    if emitted.insert(("app", query.app_id)) {
                        push_app_line(&mut lines, query.app_id, Some(key), &dlc_name);
                        matched_items += 1;
                        dlc_count += 1;
                    }
                } else if query.depots.is_empty() {
                    if emitted.insert(("app", query.app_id)) {
                        push_app_line(&mut lines, query.app_id, None, &dlc_name);
                        dlc_count += 1;
                    }
                } else {
                    skipped_keyless.insert(query.app_id);
                }
                if let Some(token) = app_tokens
                    .get(&query.app_id)
                    .filter(|value| valid_app_token(value))
                {
                    lines.push(format!("addtoken({}, \"{}\")", query.app_id, token));
                    matched_items += 1;
                }
            } else if !depot_keys.contains_key(&query.app_id) {
                skipped_keyless.insert(query.app_id);
            }

            for depot in &query.depots {
                if depot.depot_id == query.app_id {
                    continue;
                }
                let Some(key) = depot_keys
                    .get(&depot.depot_id)
                    .filter(|value| valid_depot_key(value))
                else {
                    continue;
                };
                if !emitted.insert(("app", depot.depot_id)) {
                    continue;
                }
                push_app_line(&mut lines, depot.depot_id, Some(key), "");
                depot_count += 1;
                matched_items += 1;
                if options.pin_manifest {
                    if let Some(manifest) = depot
                        .manifest_id
                        .as_deref()
                        .filter(|value| valid_manifest(value))
                    {
                        lines.push(format!(
                            "setManifestid({},\"{}\",0)",
                            depot.depot_id, manifest
                        ));
                        pinned += 1;
                    }
                }
            }
        }
    }
    if matched_items == 0 {
        bail!(
            "key cache contains no usable depot key or app token for AppID {}",
            main.app_id
        );
    }
    let mut lua = lines.join("\r\n");
    lua.push_str("\r\n");
    if lua.len() > MAX_LUA_BYTES {
        bail!("generated Lua exceeds the {MAX_LUA_BYTES} byte limit");
    }
    Ok(RenderedLua {
        lua,
        depot_count,
        dlc_count,
        pinned_manifest_count: pinned,
        skipped_keyless: skipped_keyless.into_iter().collect(),
    })
}

fn push_app_line(lines: &mut Vec<String>, app_id: u32, key: Option<&String>, name: &str) {
    let mut line = match key.filter(|value| valid_depot_key(value)) {
        Some(key) => format!("addappid({app_id}, 1, \"{key}\")"),
        None => format!("addappid({app_id})"),
    };
    if !name.is_empty() {
        line.push_str(" -- ");
        line.push_str(name);
    }
    lines.push(line);
}

fn parse_steamcmd_info(bytes: &[u8], app_id: u32) -> Result<DepotQuery> {
    let root: Value = serde_json::from_slice(bytes).context("SteamCMD response is invalid JSON")?;
    let app = root
        .get("data")
        .and_then(|data| data.get(app_id.to_string()))
        .context("SteamCMD response does not contain the requested AppID")?;
    let app_name = app
        .get("common")
        .and_then(|common| common.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    let mut depots = Vec::new();
    let mut dlc_app_ids = Vec::new();
    let mut seen_dlc = HashSet::new();
    if let Some(object) = app.get("depots").and_then(Value::as_object) {
        for (id, value) in object {
            let Ok(depot_id) = id.parse::<u32>() else {
                continue;
            };
            if depot_id == 0 || value.get("depotfromapp").is_some() {
                continue;
            }
            let dlc_app_id = json_u32(value.get("dlcappid"));
            if let Some(dlc) = dlc_app_id.filter(|value| *value != 0) {
                if seen_dlc.insert(dlc) {
                    dlc_app_ids.push(dlc);
                }
            }
            let manifest_id = value
                .get("manifests")
                .and_then(|value| value.get("public"))
                .and_then(|value| value.get("gid"))
                .and_then(json_scalar_string)
                .filter(|value| valid_manifest(value));
            depots.push(DepotRecord {
                depot_id,
                manifest_id,
                dlc_app_id,
            });
        }
    }
    if let Some(value) = app.get("extended").and_then(|value| value.get("listofdlc")) {
        match value {
            Value::Array(values) => {
                for value in values {
                    if let Some(id) = json_u32(Some(value)).filter(|value| *value != 0) {
                        if seen_dlc.insert(id) {
                            dlc_app_ids.push(id);
                        }
                    }
                }
            }
            Value::String(values) => {
                for value in values.split(',') {
                    if let Ok(id) = value.trim().parse::<u32>() {
                        if id != 0 && seen_dlc.insert(id) {
                            dlc_app_ids.push(id);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    depots.sort_by_key(|depot| depot.depot_id);
    dlc_app_ids.sort_unstable();
    dlc_app_ids.dedup();
    Ok(DepotQuery {
        app_id,
        app_name: if app_name.is_empty() {
            format!("App {app_id}")
        } else {
            app_name
        },
        depots,
        dlc_app_ids,
    })
}

fn parse_app_details(bytes: &[u8], requested_app_id: u32) -> Result<Option<CatalogGame>> {
    let root: Value =
        serde_json::from_slice(bytes).context("Steam appdetails response is invalid JSON")?;
    let object = root
        .as_object()
        .context("Steam appdetails response is not an object")?;
    let mut candidates = Vec::new();
    if let Some(exact) = object.get(&requested_app_id.to_string()) {
        candidates.push(exact);
    } else {
        candidates.extend(
            object
                .values()
                .filter(|value| value.get("success").and_then(Value::as_bool) == Some(true)),
        );
    }
    for candidate in candidates {
        if candidate.get("success").and_then(Value::as_bool) == Some(false) {
            continue;
        }
        let Some(data) = candidate.get("data") else {
            continue;
        };
        let Some(name) = data.get("name").and_then(Value::as_str) else {
            continue;
        };
        if name.trim().is_empty() {
            continue;
        }
        let cover = data
            .get("header_image")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        let release_date = data
            .get("release_date")
            .and_then(|value| value.get("date"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        return Ok(Some(game_with_cover(
            requested_app_id,
            name,
            &cover,
            release_date,
            0,
            "steam-store",
        )));
    }
    Ok(None)
}

fn parse_store_search(bytes: &[u8]) -> Result<Vec<CatalogGame>> {
    let root: Value =
        serde_json::from_slice(bytes).context("Steam store search response is invalid JSON")?;
    let mut games = Vec::new();
    for item in root
        .get("items")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(app_id) = json_u32(item.get("id")).filter(|value| *value != 0) else {
            continue;
        };
        let Some(name) = item.get("name").and_then(Value::as_str) else {
            continue;
        };
        if name.trim().is_empty() {
            continue;
        }
        let cover = item.get("tiny_image").and_then(Value::as_str).unwrap_or("");
        games.push(game_with_cover(
            app_id,
            name,
            cover,
            String::new(),
            0,
            "steam-search",
        ));
        if games.len() >= MAX_SEARCH_RESULTS {
            break;
        }
    }
    Ok(games)
}

fn parse_heihe_detail(bytes: &[u8], app_id: u32) -> Result<Option<CatalogGame>> {
    let root: Value =
        serde_json::from_slice(bytes).context("XiaoHeiHe detail response is invalid JSON")?;
    let Some(result) = root.get("result").and_then(Value::as_object) else {
        return Ok(None);
    };
    let Some(name) = result.get("name").and_then(Value::as_str) else {
        return Ok(None);
    };
    if name.trim().is_empty() {
        return Ok(None);
    }
    let cover = result.get("image").and_then(Value::as_str).unwrap_or("");
    let release = result
        .get("release_date")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    Ok(Some(game_with_cover(
        app_id,
        name,
        cover,
        release,
        0,
        "xiaoheihe-detail",
    )))
}

fn parse_heihe_search(bytes: &[u8], keyword: &str) -> Result<Vec<CatalogGame>> {
    let root: Value =
        serde_json::from_slice(bytes).context("XiaoHeiHe search response is invalid JSON")?;
    let keyword = normalize_search(keyword);
    let mut scored = Vec::new();
    for item in root
        .get("result")
        .and_then(|value| value.get("games"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(app_id) = valid_heihe_steam_app(item) else {
            continue;
        };
        let Some(name) = item.get("name").and_then(Value::as_str) else {
            continue;
        };
        if name.trim().is_empty() {
            continue;
        }
        let item_type = item.get("type").and_then(Value::as_str).unwrap_or("");
        let mut covers = Vec::new();
        for key in ["head_image_616_353", "image", "vertical_head_image"] {
            if let Some(url) = item.get(key).and_then(Value::as_str) {
                if !url.trim().is_empty() && !covers.iter().any(|known| known == url) {
                    covers.push(url.to_string());
                }
            }
        }
        add_cover_fallbacks(app_id, &mut covers);
        let followers = item.get("follow_num").and_then(json_u64).unwrap_or(0);
        let release = item
            .get("release_date")
            .or_else(|| item.get("release_date_desc"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        let name_hit = normalize_search(name).contains(&keyword);
        scored.push((
            CatalogGame {
                app_id,
                name: name.trim().into(),
                cover_url: covers.first().cloned().unwrap_or_default(),
                cover_candidates: covers,
                release_date: release,
                followers,
                source: "xiaoheihe-search".into(),
            },
            name_hit,
            item_type == "game",
        ));
    }
    scored.sort_by(|left, right| {
        right
            .1
            .cmp(&left.1)
            .then_with(|| right.2.cmp(&left.2))
            .then_with(|| right.0.followers.cmp(&left.0.followers))
            .then_with(|| left.0.app_id.cmp(&right.0.app_id))
    });
    let useful = scored.iter().any(|item| item.1)
        || scored
            .first()
            .is_some_and(|item| item.2 && item.0.followers >= 10_000);
    if !useful {
        return Ok(Vec::new());
    }
    Ok(scored
        .into_iter()
        .take(MAX_SEARCH_RESULTS)
        .map(|item| item.0)
        .collect())
}

fn parse_heihe_rank(bytes: &[u8]) -> Result<Vec<CatalogGame>> {
    let root: Value =
        serde_json::from_slice(bytes).context("XiaoHeiHe rank response is invalid JSON")?;
    let mut games = Vec::new();
    for item in root
        .get("result")
        .and_then(|value| value.get("list"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(app_id) = json_u32(item.get("appid")).filter(|value| *value != 0) else {
            continue;
        };
        let name = item
            .get("game_name")
            .or_else(|| item.get("name"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        if name.is_empty() {
            continue;
        }
        let cover = item.get("game_img").and_then(Value::as_str).unwrap_or("");
        let followers = item.get("follow_num").and_then(json_u64).unwrap_or(0);
        games.push(game_with_cover(
            app_id,
            name,
            cover,
            String::new(),
            followers,
            "xiaoheihe-rank",
        ));
    }
    Ok(games)
}

fn game_with_cover(
    app_id: u32,
    name: &str,
    cover: &str,
    release_date: String,
    followers: u64,
    source: &str,
) -> CatalogGame {
    let mut covers = Vec::new();
    if !cover.trim().is_empty() {
        covers.push(cover.trim().to_string());
    }
    add_cover_fallbacks(app_id, &mut covers);
    CatalogGame {
        app_id,
        name: name.trim().into(),
        cover_url: covers.first().cloned().unwrap_or_default(),
        cover_candidates: covers,
        release_date,
        followers,
        source: source.into(),
    }
}

fn fallback_game(app_id: u32) -> CatalogGame {
    game_with_cover(
        app_id,
        &format!("AppID: {app_id}"),
        "",
        String::new(),
        0,
        "fallback",
    )
}

fn add_cover_fallbacks(app_id: u32, covers: &mut Vec<String>) {
    for url in [
        format!("https://cdn.cloudflare.steamstatic.com/steam/apps/{app_id}/header.jpg"),
        format!("https://cdn.akamai.steamstatic.com/steam/apps/{app_id}/header.jpg"),
    ] {
        if !covers.iter().any(|known| known == &url) {
            covers.push(url);
        }
    }
}

fn valid_heihe_steam_app(value: &Value) -> Option<u32> {
    let steam = json_u32(value.get("steam_appid"))?;
    if json_u32(value.get("appid"))? != steam || steam == 0 {
        return None;
    }
    let platforms = value.get("platforms")?.as_array()?;
    platforms
        .iter()
        .any(|platform| {
            platform
                .as_str()
                .is_some_and(|value| value.eq_ignore_ascii_case("steam"))
        })
        .then_some(steam)
}

fn validate_string_map_json(bytes: &[u8], label: &str) -> Result<()> {
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let count = serde::de::Deserializer::deserialize_map(&mut deserializer, CountMapVisitor)
        .with_context(|| format!("{label} is not a valid JSON object"))?;
    deserializer
        .end()
        .with_context(|| format!("{label} has trailing JSON data"))?;
    if count == 0 {
        bail!("{label} is empty");
    }
    Ok(())
}

struct CountMapVisitor;

impl<'de> serde::de::Visitor<'de> for CountMapVisitor {
    type Value = usize;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a JSON object")
    }

    fn visit_map<A>(self, mut map: A) -> std::result::Result<Self::Value, A::Error>
    where
        A: serde::de::MapAccess<'de>,
    {
        let mut count = 0usize;
        while map.next_key::<IgnoredAny>()?.is_some() {
            let _: IgnoredAny = map.next_value()?;
            count = count.saturating_add(1);
        }
        Ok(count)
    }
}

fn lookup_json_string_keys(path: &Path, targets: &BTreeSet<u32>) -> Result<HashMap<u32, String>> {
    if targets.is_empty() {
        return Ok(HashMap::new());
    }
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("could not inspect key cache {}", path.display()))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        bail!("key cache must be a regular file: {}", path.display());
    }
    if metadata.len() > MAX_KEY_CACHE_BYTES {
        bail!("key cache exceeds the {MAX_KEY_CACHE_BYTES} byte limit");
    }
    let fingerprint = (metadata.len(), metadata.modified()?);
    let selected = {
        let mut caches = KEY_SELECTIONS
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .map_err(|_| anyhow::anyhow!("key selection cache poisoned"))?;
        if caches.len() > 16 {
            caches.retain(|_, cache| Arc::strong_count(cache) > 1);
        }
        caches
            .entry(path.to_path_buf())
            .or_insert_with(|| Arc::new(Mutex::new(KeySelections::default())))
            .clone()
    };
    let mut selected = selected
        .lock()
        .map_err(|_| anyhow::anyhow!("key selection cache entry poisoned"))?;
    if selected.fingerprint != Some(fingerprint) {
        selected.values.clear();
        selected.fingerprint = Some(fingerprint);
    }
    let known: HashMap<u32, String> = targets
        .iter()
        .filter_map(|id| {
            selected
                .values
                .get(id)
                .and_then(|value| value.clone())
                .map(|value| (*id, value))
        })
        .collect();
    let missing: BTreeSet<u32> = targets
        .iter()
        .copied()
        .filter(|id| !selected.values.contains_key(id))
        .collect();
    if missing.is_empty() {
        return Ok(targets
            .iter()
            .filter_map(|id| {
                selected
                    .values
                    .get(id)
                    .and_then(|value| value.clone())
                    .map(|value| (*id, value))
            })
            .collect());
    }
    let file = File::open(path).with_context(|| format!("could not open {}", path.display()))?;
    let target_strings: HashSet<String> = missing.iter().map(ToString::to_string).collect();
    let seed = FilterMapSeed {
        targets: &target_strings,
    };
    let mut deserializer =
        serde_json::Deserializer::from_reader(BufReader::with_capacity(256 * 1024, file));
    let raw: HashMap<String, String> =
        serde::de::DeserializeSeed::deserialize(seed, &mut deserializer)
            .with_context(|| format!("could not parse {}", path.display()))?;
    deserializer
        .end()
        .with_context(|| format!("key cache contains trailing JSON: {}", path.display()))?;
    let found: HashMap<u32, String> = raw
        .into_iter()
        .filter_map(|(key, value)| key.parse().ok().map(|id| (id, value)))
        .collect();
    let after = fs::symlink_metadata(path)?;
    if (after.len(), after.modified()?) != fingerprint {
        bail!("key cache changed during selection; retry the operation");
    }
    if selected.values.len() + missing.len() > 100_000 {
        selected.values.clear();
    }
    for id in missing {
        if selected.values.len() < 100_000 {
            selected.values.insert(id, found.get(&id).cloned());
        }
    }
    // Preserve matched targets when bounded memoization evicts older entries.
    Ok(targets
        .iter()
        .filter_map(|id| {
            found
                .get(id)
                .cloned()
                .or_else(|| known.get(id).cloned())
                .map(|value| (*id, value))
        })
        .collect())
}

struct FilterMapSeed<'a> {
    targets: &'a HashSet<String>,
}

impl<'de> serde::de::DeserializeSeed<'de> for FilterMapSeed<'_> {
    type Value = HashMap<String, String>;

    fn deserialize<D>(self, deserializer: D) -> std::result::Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_map(FilterMapVisitor {
            targets: self.targets,
        })
    }
}

struct FilterMapVisitor<'a> {
    targets: &'a HashSet<String>,
}

impl<'de> serde::de::Visitor<'de> for FilterMapVisitor<'_> {
    type Value = HashMap<String, String>;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a JSON object mapping numeric IDs to strings")
    }

    fn visit_map<A>(self, mut map: A) -> std::result::Result<Self::Value, A::Error>
    where
        A: serde::de::MapAccess<'de>,
    {
        let mut output = HashMap::new();
        while let Some(key) = map.next_key::<String>()? {
            if self.targets.contains(&key) {
                let value = map.next_value::<StringOrNumber>()?.0;
                output.insert(key, value);
            } else {
                let _: IgnoredAny = map.next_value()?;
            }
        }
        Ok(output)
    }
}

struct StringOrNumber(String);

impl<'de> Deserialize<'de> for StringOrNumber {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = StringOrNumber;
            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a string or integer")
            }
            fn visit_str<E>(self, value: &str) -> std::result::Result<Self::Value, E> {
                Ok(StringOrNumber(value.to_string()))
            }
            fn visit_string<E>(self, value: String) -> std::result::Result<Self::Value, E> {
                Ok(StringOrNumber(value))
            }
            fn visit_u64<E>(self, value: u64) -> std::result::Result<Self::Value, E> {
                Ok(StringOrNumber(value.to_string()))
            }
            fn visit_i64<E>(self, value: i64) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                if value < 0 {
                    return Err(E::custom("negative IDs/tokens are invalid"));
                }
                Ok(StringOrNumber(value.to_string()))
            }
        }
        deserializer.deserialize_any(Visitor)
    }
}

fn read_limited_response(response: Response, limit: u64, url: &str) -> Result<Vec<u8>> {
    let status = response.status();
    if !status.is_success() {
        bail!("request returned HTTP {}: {url}", status.as_u16());
    }
    if response
        .content_length()
        .is_some_and(|length| length > limit)
    {
        bail!("response exceeds the {limit} byte limit: {url}");
    }
    let mut bytes = Vec::with_capacity(
        response
            .content_length()
            .unwrap_or(0)
            .min(limit)
            .try_into()
            .unwrap_or(0),
    );
    response
        .take(limit + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("could not read response: {url}"))?;
    if bytes.len() as u64 > limit {
        bail!("response grew beyond the {limit} byte limit: {url}");
    }
    Ok(bytes)
}

fn write_cache_pair(path_a: &Path, bytes_a: &[u8], path_b: &Path, bytes_b: &[u8]) -> Result<()> {
    let temp_a = stage_file(path_a, bytes_a)?;
    let temp_b = match stage_file(path_b, bytes_b) {
        Ok(path) => path,
        Err(error) => {
            let _ = fs::remove_file(&temp_a);
            return Err(error);
        }
    };
    let backup_a = backup_existing(path_a)?;
    let backup_b = match backup_existing(path_b) {
        Ok(path) => path,
        Err(error) => {
            restore_backup(path_a, backup_a.as_deref());
            let _ = fs::remove_file(&temp_a);
            let _ = fs::remove_file(&temp_b);
            return Err(error);
        }
    };
    if let Err(error) = atomic_replace(&temp_a, path_a) {
        restore_backup(path_a, backup_a.as_deref());
        restore_backup(path_b, backup_b.as_deref());
        let _ = fs::remove_file(&temp_a);
        let _ = fs::remove_file(&temp_b);
        return Err(error);
    }
    if let Err(error) = atomic_replace(&temp_b, path_b) {
        let _ = fs::remove_file(path_a);
        restore_backup(path_a, backup_a.as_deref());
        restore_backup(path_b, backup_b.as_deref());
        let _ = fs::remove_file(&temp_b);
        return Err(error);
    }
    if let Some(path) = backup_a {
        let _ = fs::remove_file(path);
    }
    if let Some(path) = backup_b {
        let _ = fs::remove_file(path);
    }
    Ok(())
}

fn stage_file(destination: &Path, bytes: &[u8]) -> Result<PathBuf> {
    let parent = destination.parent().context("cache path has no parent")?;
    fs::create_dir_all(parent)?;
    let temp = unique_sibling(destination, "tmp")?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)?;
    if let Err(error) = file.write_all(bytes).and_then(|_| file.sync_all()) {
        let _ = fs::remove_file(&temp);
        return Err(error).context("could not stage cache file");
    }
    Ok(temp)
}

fn backup_existing(path: &Path) -> Result<Option<PathBuf>> {
    if !path.exists() {
        return Ok(None);
    }
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        bail!("cache target is not a regular file: {}", path.display());
    }
    let backup = unique_sibling(path, "bak")?;
    fs::rename(path, &backup)?;
    Ok(Some(backup))
}

fn restore_backup(destination: &Path, backup: Option<&Path>) {
    if let Some(backup) = backup {
        let _ = fs::rename(backup, destination);
    }
}

fn atomic_write(destination: &Path, bytes: &[u8]) -> Result<()> {
    let temp = stage_file(destination, bytes)?;
    let backup = backup_existing(destination)?;
    if let Err(error) = atomic_replace(&temp, destination) {
        restore_backup(destination, backup.as_deref());
        let _ = fs::remove_file(&temp);
        return Err(error);
    }
    if let Some(path) = backup {
        let _ = fs::remove_file(path);
    }
    Ok(())
}

#[cfg(windows)]
fn atomic_replace(from: &Path, to: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };
    let from: Vec<u16> = from.as_os_str().encode_wide().chain(Some(0)).collect();
    let to: Vec<u16> = to.as_os_str().encode_wide().chain(Some(0)).collect();
    if unsafe {
        MoveFileExW(
            from.as_ptr(),
            to.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error()).context("could not replace cache file");
    }
    Ok(())
}

#[cfg(not(windows))]
fn atomic_replace(from: &Path, to: &Path) -> Result<()> {
    fs::rename(from, to).context("could not replace cache file")
}

fn cache_receipt(cache_dir: &Path, depot: &Path, tokens: &Path) -> Result<KeyCacheUpdate> {
    let updated_at = fs::read_to_string(cache_dir.join(".lastupdate"))
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(0);
    Ok(KeyCacheUpdate {
        cache_dir: cache_dir.to_string_lossy().into_owned(),
        depot_keys_path: depot.to_string_lossy().into_owned(),
        app_tokens_path: tokens.to_string_lossy().into_owned(),
        depot_keys_bytes: fs::metadata(depot)?.len(),
        app_tokens_bytes: fs::metadata(tokens)?.len(),
        updated_at,
    })
}

fn create_cache_dir(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty() {
        bail!("cache path is empty");
    }
    fs::create_dir_all(path).with_context(|| format!("could not create {}", path.display()))?;
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        bail!("cache path must be a regular directory: {}", path.display());
    }
    Ok(())
}

fn regular_file(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .map(|metadata| metadata.is_file() && !metadata.file_type().is_symlink())
        .unwrap_or(false)
}

fn unique_sibling(path: &Path, tag: &str) -> Result<PathBuf> {
    let parent = path.parent().context("path has no parent")?;
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .context("path file name is not Unicode")?;
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    for attempt in 0..100u32 {
        let candidate = parent.join(format!(
            ".{name}.stn-{tag}-{}-{stamp}-{attempt}",
            std::process::id()
        ));
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    bail!("could not allocate a temporary cache path")
}

fn validate_http_url(value: &str) -> Result<()> {
    let lower = value.trim().to_ascii_lowercase();
    if !lower.starts_with("https://") && !lower.starts_with("http://") {
        bail!("catalog URL must use HTTP or HTTPS");
    }
    if value.contains('\0') || value.len() > 4096 {
        bail!("catalog URL is invalid");
    }
    Ok(())
}

fn validate_app_id(app_id: u32) -> Result<()> {
    if app_id == 0 {
        bail!("AppID must be greater than zero");
    }
    Ok(())
}

fn valid_depot_key(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|value| value.is_ascii_hexdigit())
}

fn valid_app_token(value: &str) -> bool {
    value.parse::<u64>().is_ok()
}

fn valid_manifest(value: &str) -> bool {
    !value.is_empty() && value.len() <= 32 && value.bytes().all(|value| value.is_ascii_digit())
}

fn sanitize_lua_comment(value: &str) -> String {
    value
        .chars()
        .filter(|value| !matches!(*value, '\r' | '\n' | '\0'))
        .collect::<String>()
        .replace("--", "—")
        .trim()
        .chars()
        .take(160)
        .collect()
}

fn normalize_search(value: &str) -> String {
    value
        .chars()
        .filter(|value| value.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn url_encode(value: &str) -> String {
    value
        .as_bytes()
        .iter()
        .flat_map(|byte| {
            if byte.is_ascii_alphanumeric() || matches!(*byte, b'-' | b'_' | b'.' | b'~') {
                vec![char::from(*byte)]
            } else {
                format!("%{byte:02X}").chars().collect()
            }
        })
        .collect()
}

fn stable_recommendation_key(app_id: u32) -> u64 {
    let mut value = u64::from(app_id) ^ 0x9E37_79B9_7F4A_7C15;
    value ^= value >> 30;
    value = value.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    value ^= value >> 27;
    value = value.wrapping_mul(0x94D0_49BB_1331_11EB);
    value ^ (value >> 31)
}

fn json_u32(value: Option<&Value>) -> Option<u32> {
    match value? {
        Value::Number(value) => value.as_u64().and_then(|value| value.try_into().ok()),
        Value::String(value) => value.parse().ok(),
        _ => None,
    }
}

fn json_u64(value: &Value) -> Option<u64> {
    match value {
        Value::Number(value) => value.as_u64(),
        Value::String(value) => value.parse().ok(),
        _ => None,
    }
}

fn json_scalar_string(value: &Value) -> Option<String> {
    match value {
        Value::String(value) => Some(value.clone()),
        Value::Number(value) => Some(value.to_string()),
        _ => None,
    }
}

fn unix_seconds() -> Result<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn targeted_key_cache_lookup_is_buffered_and_fast() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("fixture-keys.json");
        let contents = serde_json::to_vec(&std::collections::BTreeMap::from_iter(
            (1..=12000u32).map(|id| (id.to_string(), "a".repeat(64))),
        ))
        .unwrap();
        fs::write(&path, &contents).unwrap();
        let started = std::time::Instant::now();
        let found = lookup_json_string_keys(&path, &BTreeSet::from([11999, 12000])).unwrap();
        let elapsed = started.elapsed();
        println!(
            "keylookup fixture: bytes={}, matched={}, elapsedMs={:.3}",
            contents.len(),
            found.len(),
            elapsed.as_secs_f64() * 1000.
        );
        assert_eq!(found.len(), 2);
        assert!(
            elapsed < Duration::from_millis(600),
            "sub-megabyte key selection must not spend seconds in per-byte file syscalls"
        );
    }

    #[test]
    fn parses_steamcmd_depots_and_merges_dlc_sources() {
        let bytes = br#"{
          "data":{"42":{"common":{"name":"Sample"},"depots":{
            "100":{"manifests":{"public":{"gid":"123"}}},
            "200":{"dlcappid":"7","manifests":{"public":{"gid":456}}},
            "201":{"dlcappid":7},
            "300":{"depotfromapp":"9"},"branches":{}
          },"extended":{"listofdlc":"6, 7"}}}}
        "#;
        let parsed = parse_steamcmd_info(bytes, 42).unwrap();
        assert_eq!(parsed.app_name, "Sample");
        assert_eq!(parsed.depots.len(), 3);
        assert_eq!(parsed.depots[0].manifest_id.as_deref(), Some("123"));
        assert_eq!(parsed.dlc_app_ids, vec![6, 7]);
    }

    #[test]
    fn parses_store_and_appdetails_fallbacks() {
        let search = parse_store_search(
            br#"{"items":[{"id":42,"name":"Portal","tiny_image":"https://img"}]}"#,
        )
        .unwrap();
        assert_eq!(search[0].app_id, 42);
        assert_eq!(search[0].cover_url, "https://img");
        let detail = parse_app_details(
            br#"{"99":{"success":true,"data":{"name":"Redirected","header_image":"h","release_date":{"date":"Today"}}}}"#,
            42,
        )
        .unwrap()
        .unwrap();
        assert_eq!(detail.app_id, 42);
        assert_eq!(detail.name, "Redirected");
        assert_eq!(detail.release_date, "Today");
    }

    #[test]
    fn parses_and_scores_heihe_search() {
        let games = parse_heihe_search(
            br#"{"result":{"games":[
              {"appid":42,"steam_appid":42,"platforms":["steam"],"name":"Portal Two","type":"game","follow_num":20,"image":"x"},
              {"appid":7,"steam_appid":7,"platforms":["mobile"],"name":"Portal Mobile","type":"game","follow_num":999}
            ]}}"#,
            "portal",
        )
        .unwrap();
        assert_eq!(games.len(), 1);
        assert_eq!(games[0].app_id, 42);
    }

    #[test]
    fn renders_keyed_lua_tokens_dlc_and_pins() {
        let key = "a".repeat(64);
        let main = DepotQuery {
            app_id: 42,
            app_name: "Main -- Game".into(),
            depots: vec![DepotRecord {
                depot_id: 100,
                manifest_id: Some("123".into()),
                dlc_app_id: None,
            }],
            dlc_app_ids: vec![7],
        };
        let dlc = DepotQuery {
            app_id: 7,
            app_name: "DLC".into(),
            depots: Vec::new(),
            dlc_app_ids: Vec::new(),
        };
        let keys = HashMap::from([(100, key)]);
        let tokens = HashMap::from([(42, "99".into())]);
        let rendered = render_lua(
            &main,
            &[dlc],
            &keys,
            &tokens,
            &LuaGenerationOptions {
                include_dlc: true,
                pin_manifest: true,
                fetch_names: true,
            },
        )
        .unwrap();
        assert!(rendered.lua.contains("addappid(42) -- Main — Game"));
        assert!(rendered.lua.contains("addappid(100, 1"));
        assert!(rendered.lua.contains("setManifestid(100,\"123\",0)"));
        assert!(rendered.lua.contains("addtoken(42, \"99\")"));
        assert!(rendered.lua.contains("addappid(7) -- DLC"));
        assert_eq!(rendered.pinned_manifest_count, 1);
    }

    #[test]
    fn empty_key_material_is_rejected() {
        let main = DepotQuery {
            app_id: 42,
            app_name: "Main".into(),
            depots: vec![DepotRecord {
                depot_id: 100,
                manifest_id: None,
                dlc_app_id: None,
            }],
            dlc_app_ids: Vec::new(),
        };
        assert!(render_lua(
            &main,
            &[],
            &HashMap::new(),
            &HashMap::new(),
            &LuaGenerationOptions::default()
        )
        .is_err());
    }

    #[test]
    fn streaming_key_lookup_reads_only_requested_values() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("keys.json");
        fs::write(&path, br#"{"1":"one","2":22,"3":"three"}"#).unwrap();
        let values = lookup_json_string_keys(&path, &BTreeSet::from([1, 2])).unwrap();
        assert_eq!(values.get(&1).map(String::as_str), Some("one"));
        assert_eq!(values.get(&2).map(String::as_str), Some("22"));
        assert!(!values.contains_key(&3));
    }

    #[test]
    fn cache_pair_is_replaced_without_partial_old_new_mix() {
        let temp = tempdir().unwrap();
        let a = temp.path().join("a.json");
        let b = temp.path().join("b.json");
        fs::write(&a, b"old-a").unwrap();
        fs::write(&b, b"old-b").unwrap();
        write_cache_pair(&a, b"new-a", &b, b"new-b").unwrap();
        assert_eq!(fs::read(a).unwrap(), b"new-a");
        assert_eq!(fs::read(b).unwrap(), b"new-b");
    }

    #[test]
    fn url_encoding_and_comment_sanitizing_are_deterministic() {
        assert_eq!(url_encode("a b/中"), "a%20b%2F%E4%B8%AD");
        assert_eq!(sanitize_lua_comment("x--y\nz"), "x—yz");
        assert_ne!(stable_recommendation_key(1), stable_recommendation_key(2));
    }
}

#[cfg(test)]
#[path = "catalog_perf_tests.rs"]
mod perf_tests;
