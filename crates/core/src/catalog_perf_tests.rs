//! Deterministic local-only import pipeline checks; never writes a live Steam tree.
use super::*;
use std::io::BufRead;
use std::net::TcpListener;
use std::sync::atomic::AtomicBool;

struct FixtureServer {
    url: String,
    requests: Arc<AtomicUsize>,
    maximum: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl FixtureServer {
    fn new() -> Self {
        Self::with_failed_app(None)
    }
    fn with_failed_app(failed_app: Option<u32>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(AtomicUsize::new(0));
        let maximum = Arc::new(AtomicUsize::new(0));
        let active = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let state = (requests.clone(), maximum.clone(), active, stop.clone());
        let thread = std::thread::spawn(move || {
            while !state.3.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut socket, _)) => {
                        let (requests, maximum, active) =
                            (state.0.clone(), state.1.clone(), state.2.clone());
                        std::thread::spawn(move || {
                            socket.set_nonblocking(false).unwrap();
                            socket
                                .set_read_timeout(Some(Duration::from_secs(2)))
                                .unwrap();
                            let mut reader = BufReader::new(socket.try_clone().unwrap());
                            let mut first = String::new();
                            reader.read_line(&mut first).unwrap();
                            let mut header = String::new();
                            loop {
                                header.clear();
                                if reader.read_line(&mut header).unwrap() == 0 || header == "\r\n" {
                                    break;
                                }
                            }
                            let app_id = first
                                .split_whitespace()
                                .nth(1)
                                .unwrap()
                                .rsplit('/')
                                .next()
                                .unwrap()
                                .parse::<u32>()
                                .unwrap();
                            requests.fetch_add(1, Ordering::Relaxed);
                            let current = active.fetch_add(1, Ordering::Relaxed) + 1;
                            maximum.fetch_max(current, Ordering::Relaxed);
                            std::thread::sleep(Duration::from_millis(60));
                            if failed_app == Some(app_id) {
                                socket.write_all(b"HTTP/1.1 500 Error\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}").unwrap();
                                active.fetch_sub(1, Ordering::Relaxed);
                                return;
                            }
                            let depot = if app_id == 42 { 100 } else { 20000 + app_id };
                            let mut app = serde_json::json!({"common":{"name":format!("Fixture {app_id}")},"depots":{depot.to_string():{"manifests":{"public":{"gid":"123"}}}}});
                            if app_id == 42 {
                                app["extended"] = serde_json::json!({"listofdlc":(7..19u32).map(|id|id.to_string()).collect::<Vec<_>>().join(",")});
                            }
                            let body = serde_json::to_vec(
                                &serde_json::json!({"data":{app_id.to_string():app}}),
                            )
                            .unwrap();
                            let response=format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",body.len());
                            socket.write_all(response.as_bytes()).unwrap();
                            socket.write_all(&body).unwrap();
                            active.fetch_sub(1, Ordering::Relaxed);
                        });
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(1))
                    }
                    Err(error) => panic!("fixture server: {error}"),
                }
            }
        });
        Self {
            url,
            requests,
            maximum,
            stop,
            thread: Some(thread),
        }
    }
}

#[test]
fn failed_dlc_is_reported_as_partial_not_silently_successful() {
    let server = FixtureServer::with_failed_app(Some(7));
    let temp = tempfile::tempdir().unwrap();
    let keys: HashMap<String, String> = [42, 100]
        .into_iter()
        .chain(7..19)
        .chain((7..19).map(|id| 20000 + id))
        .map(|id| (id.to_string(), "a".repeat(64)))
        .collect();
    fs::write(
        temp.path().join("depotkeys.json"),
        serde_json::to_vec(&keys).unwrap(),
    )
    .unwrap();
    fs::write(temp.path().join("appaccesstokens.json"), b"{}").unwrap();
    let client = CatalogClient::new(CatalogConfig {
        cache_dir: temp.path().to_path_buf(),
        steamcmd_info_url_template: format!("{}/info/{{appid}}", server.url),
        ..Default::default()
    })
    .unwrap();
    let generation = client
        .generate_lua(42, &LuaGenerationOptions::default())
        .unwrap();
    assert_eq!(generation.incomplete_dlc, vec![7]);
    assert_eq!(generation.dlc_count, 11);
    assert!(generation
        .steps
        .iter()
        .any(|step| step.contains("DLC 7 metadata skipped")));
    assert!(client.cached_depots(7).unwrap().is_none());
}
impl Drop for FixtureServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}

#[test]
fn cold_and_warm_import_use_bounded_dlc_fanout_and_real_fixture_writes() {
    let server = FixtureServer::new();
    let temp = tempfile::tempdir().unwrap();
    let cache = temp.path().join("cache");
    fs::create_dir_all(&cache).unwrap();
    let ids = [42, 100]
        .into_iter()
        .chain(7..19)
        .chain((7..19).map(|id| 20000 + id));
    let keys: HashMap<String, String> = ids.map(|id| (id.to_string(), "a".repeat(64))).collect();
    fs::write(
        cache.join("depotkeys.json"),
        serde_json::to_vec(&keys).unwrap(),
    )
    .unwrap();
    fs::write(cache.join("appaccesstokens.json"), b"{}").unwrap();
    let config = CatalogConfig {
        cache_dir: cache.clone(),
        steamcmd_info_url_template: format!("{}/info/{{appid}}", server.url),
        ..CatalogConfig::default()
    };
    let client = CatalogClient::new(config.clone()).unwrap();
    assert!(client.cached_depots(42).unwrap().is_none());
    assert_eq!(server.requests.load(Ordering::Relaxed), 0);
    let started = Instant::now();
    let stages = Mutex::new(Vec::new());
    let cold = client
        .generate_lua_with_progress(42, &LuaGenerationOptions::default(), &|stage| {
            stages.lock().unwrap().push(stage.stage)
        })
        .unwrap();
    let cold_ms = started.elapsed().as_secs_f64() * 1000.;
    assert_eq!(cold.dlc_count, 12);
    assert_eq!(cold.depot_count, 13);
    assert_eq!(server.requests.load(Ordering::Relaxed), 13);
    assert!(server.maximum.load(Ordering::Relaxed) > 1);
    assert!(server.maximum.load(Ordering::Relaxed) <= DLC_CONCURRENCY);
    let steam = temp.path().join("fixture-steam");
    fs::create_dir_all(steam.join("config/lua")).unwrap();
    let write = crate::library::create_lua_entry(&steam, 42, Some(&cold.lua), false).unwrap();
    assert!(Path::new(&write.destination).is_file());
    let started = Instant::now();
    let warm = CatalogClient::new(config)
        .unwrap()
        .generate_lua(42, &LuaGenerationOptions::default())
        .unwrap();
    let warm_ms = started.elapsed().as_secs_f64() * 1000.;
    assert_eq!(warm.lua, cold.lua);
    assert_eq!(server.requests.load(Ordering::Relaxed), 13);
    assert!(client.cached_depots(42).unwrap().is_some());
    assert!(stages.lock().unwrap().contains(&"queryDlc"));
    println!("fixture import: coldMs={cold_ms:.3}, warmMs={warm_ms:.3}, coldRequests=13, warmRequests=0, maxConcurrent={}",server.maximum.load(Ordering::Relaxed));
}

#[test]
fn subset_key_cache_handles_missing_targets_and_invalidates_changed_files() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("keys.json");
    fs::write(&path, b"{\"1\":\"old\",\"2\":\"second\"}").unwrap();
    let first = lookup_json_string_keys(&path, &BTreeSet::from([1, 3])).unwrap();
    assert_eq!(first.len(), 1);
    let warm = lookup_json_string_keys(&path, &BTreeSet::from([1, 2, 3])).unwrap();
    assert_eq!(warm.len(), 2);
    std::thread::sleep(Duration::from_millis(2));
    fs::write(&path, b"{\"1\":\"new\",\"2\":\"second\",\"3\":\"third\"}").unwrap();
    let changed = lookup_json_string_keys(&path, &BTreeSet::from([1, 3])).unwrap();
    assert_eq!(changed.get(&1).unwrap(), "new");
    assert!(changed.contains_key(&3));
}

#[test]
#[ignore = "read-only actual key cache timing; STN_KEY_CACHE_PATH required"]
fn benchmark_real_key_cache_read_only() {
    let path = PathBuf::from(std::env::var("STN_KEY_CACHE_PATH").expect("STN_KEY_CACHE_PATH"));
    let ids = BTreeSet::from([1161190, 1161191, 1161192]);
    let started = Instant::now();
    let first = lookup_json_string_keys(&path, &ids).unwrap();
    let cold_ms = started.elapsed().as_secs_f64() * 1000.;
    let started = Instant::now();
    let warm = lookup_json_string_keys(&path, &ids).unwrap();
    let warm_ms = started.elapsed().as_secs_f64() * 1000.;
    assert_eq!(first, warm);
    println!("actual read-only keycache: bytes={}, matched={}, firstMs={cold_ms:.3}, warmMs={warm_ms:.3}",fs::metadata(path).unwrap().len(),first.len());
}

#[test]
#[ignore = "read-only public SteamCMD timing; STN_PUBLIC_APP_ID required"]
fn benchmark_public_depots_no_steam_writes() {
    let app_id = std::env::var("STN_PUBLIC_APP_ID")
        .expect("STN_PUBLIC_APP_ID")
        .parse()
        .unwrap();
    let temp = tempfile::tempdir().unwrap();
    let config = CatalogConfig {
        cache_dir: temp.path().to_path_buf(),
        ..Default::default()
    };
    let client = CatalogClient::new(config.clone()).unwrap();
    let started = Instant::now();
    let cold = client.query_depots(app_id).unwrap();
    let cold_ms = started.elapsed().as_secs_f64() * 1000.;
    let started = Instant::now();
    let warm = CatalogClient::new(config)
        .unwrap()
        .query_depots(app_id)
        .unwrap();
    let warm_ms = started.elapsed().as_secs_f64() * 1000.;
    assert_eq!(cold, warm);
    println!("public metadata read-only: appId={app_id}, depots={}, dlc={}, firstMs={cold_ms:.3}, warmMs={warm_ms:.3}",cold.depots.len(),cold.dlc_app_ids.len());
}
