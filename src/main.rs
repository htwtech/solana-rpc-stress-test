use clap::Parser;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::time::sleep;
use crossbeam::queue::SegQueue;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Number of workers (parallel threads)
    #[arg(short, long, default_value_t = 1)]
    workers: usize,

    /// RPC method to request (e.g., getHealth, getSlot, getVersion)
    #[arg(short, long, default_value = "getHealth")]
    method: String,

    /// Timeout between requests for each worker in milliseconds
    #[arg(short, long, default_value_t = 1)]
    timeout_ms: u64,

    /// URL Solana RPC endpoint
    #[arg(short, long, default_value = "https://api.mainnet-beta.solana.com")]
    url: String,

    /// Test duration in seconds (0 = infinite)
    #[arg(short, long, default_value_t = 60)]
    duration: u64,

    /// HTTP timeout in seconds
    #[arg(long, default_value_t = 30)]
    http_timeout: u64,

    /// Debug mode: output RPC responses to console
    #[arg(short = 'v', long)]
    debug: bool,

    /// Perform preliminary ping test (10 packets)
    #[arg(short = 'p', long)]
    ping: bool,

    /// Path to configuration file (if specified, parameters are taken from it)
    #[arg(short = 'c', long)]
    config: Option<String>,
}

#[derive(Deserialize, Debug)]
struct Config {
    url: Option<String>,
    timeout_ms: Option<u64>,
    duration: Option<u64>,
    http_timeout: Option<u64>,
    methods: Vec<MethodConfig>,
}

#[derive(Deserialize, Debug, Clone)]
struct MethodConfig {
    method: String,
    params: Option<Vec<serde_json::Value>>,
    workers: usize,
}

#[derive(Serialize, Deserialize, Debug)]
struct JsonRpcRequest {
    jsonrpc: String,
    id: u64,
    method: String,
    params: Vec<serde_json::Value>,
}

#[derive(Serialize, Deserialize, Debug)]
struct JsonRpcResponse {
    jsonrpc: String,
    id: u64,
    result: Option<serde_json::Value>,
    error: Option<JsonRpcError>,
}

#[derive(Serialize, Deserialize, Debug)]
struct JsonRpcError {
    code: i32,
    message: String,
}

#[derive(Clone)]
struct Stats {
    total_requests: Arc<std::sync::atomic::AtomicU64>,
    successful_requests: Arc<std::sync::atomic::AtomicU64>,
    http_errors: Arc<Mutex<HashMap<String, Arc<std::sync::atomic::AtomicU64>>>>,
    http_timeouts: Arc<std::sync::atomic::AtomicU64>,
    json_parse_errors: Arc<std::sync::atomic::AtomicU64>,
    network_errors: Arc<std::sync::atomic::AtomicU64>,
    rpc_errors: Arc<std::sync::atomic::AtomicU64>,
    response_times: Arc<SegQueue<u64>>, // микросекунды
}

impl Stats {
    fn new() -> Self {
        Self {
            total_requests: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            successful_requests: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            http_errors: Arc::new(Mutex::new(HashMap::new())),
            http_timeouts: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            json_parse_errors: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            network_errors: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            rpc_errors: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            response_times: Arc::new(SegQueue::new()),
        }
    }

    fn record_success(&self, response_time_micros: u64) {
        self.total_requests.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.successful_requests.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.response_times.push(response_time_micros);
    }

    fn record_http_error(&self, status_code: u16, reason: &str) {
        self.total_requests.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let error_key = format!("{} {}", status_code, reason);
        
        let mut errors = self.http_errors.lock().unwrap();
        let counter = errors
            .entry(error_key)
            .or_insert_with(|| Arc::new(std::sync::atomic::AtomicU64::new(0)))
            .clone();
        drop(errors); // Освобождаем мьютекс как можно быстрее
        counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    fn record_http_timeout(&self) {
        self.total_requests.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.http_timeouts.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    fn record_json_parse_error(&self) {
        self.total_requests.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.json_parse_errors.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    fn record_network_error(&self) {
        self.total_requests.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.network_errors.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    fn record_rpc_error(&self) {
        self.total_requests.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.rpc_errors.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    fn print_summary(&self) {
        let total = self.total_requests.load(std::sync::atomic::Ordering::Relaxed);
        let successful = self.successful_requests.load(std::sync::atomic::Ordering::Relaxed);
        let http_timeouts = self.http_timeouts.load(std::sync::atomic::Ordering::Relaxed);
        let json_parse_errors = self.json_parse_errors.load(std::sync::atomic::Ordering::Relaxed);
        let network_errors = self.network_errors.load(std::sync::atomic::Ordering::Relaxed);
        let rpc_errors = self.rpc_errors.load(std::sync::atomic::Ordering::Relaxed);

        // Собираем все времена ответов
        let mut times: Vec<u64> = Vec::new();
        while let Some(time) = self.response_times.pop() {
            times.push(time);
        }

        let avg_latency = if !times.is_empty() {
            let sum: u64 = times.iter().sum();
            (sum as f64 / times.len() as f64) / 1000.0 // конвертируем в миллисекунды
        } else {
            0.0
        };

        let min_latency = times.iter().min().map(|&t| t as f64 / 1000.0).unwrap_or(0.0);
        let max_latency = times.iter().max().map(|&t| t as f64 / 1000.0).unwrap_or(0.0);

        let success_rate = if total > 0 {
            (successful as f64 / total as f64) * 100.0
        } else {
            0.0
        };

        println!("\n=== Stress Test Statistics ===");
        println!("Total requests: {}", total);
        println!("Successful: {} ({:.2}%)", successful, success_rate);
        println!("\nErrors:");
        
        // Выводим HTTP ошибки по каждому статусу
        let http_errors = self.http_errors.lock().unwrap();
        if !http_errors.is_empty() {
            let mut error_vec: Vec<_> = http_errors.iter().collect();
            error_vec.sort_by(|(k1, _), (k2, _)| k1.cmp(k2));
            for (error_name, counter) in error_vec {
                let count = counter.load(std::sync::atomic::Ordering::Relaxed);
                println!("  {}: {}", error_name, count);
            }
        }
        drop(http_errors);
        
        println!("  HTTP timeouts: {}", http_timeouts);
        println!("  JSON parse errors: {}", json_parse_errors);
        println!("  Network errors: {}", network_errors);
        println!("  RPC errors: {}", rpc_errors);
        println!("\nLatency:");
        println!("  Average: {:.2} ms", avg_latency);
        if !times.is_empty() {
            println!("  Minimum: {:.2} ms", min_latency);
            println!("  Maximum: {:.2} ms", max_latency);
        }
    }
}

async fn send_rpc_request(
    client: &reqwest::Client,
    url: &str,
    method: &str,
    params: Vec<serde_json::Value>,
    request_id: u64,
) -> Result<JsonRpcResponse, reqwest::Error> {
    let request = JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: request_id,
        method: method.to_string(),
        params,
    };

    let response = client
        .post(url)
        .json(&request)
        .send()
        .await?;

    response.json::<JsonRpcResponse>().await
}

async fn get_latest_slot(
    client: &reqwest::Client,
    url: &str,
    request_id: u64,
) -> Option<u64> {
    match send_rpc_request(client, url, "getSlot", vec![], request_id).await {
        Ok(response) => {
            if let Some(result) = response.result {
                if let Ok(slot) = serde_json::from_value::<u64>(result) {
                    return Some(slot);
                }
            }
        }
        Err(_) => {}
    }
    None
}

async fn worker(
    worker_id: usize,
    url: String,
    method: String,
    params: Vec<serde_json::Value>,
    timeout_ms: u64,
    http_timeout: Duration,
    stats: Stats,
    duration: Duration,
    debug: bool,
) {
    let client = reqwest::Client::builder()
        .timeout(http_timeout)
        .build()
        .expect("Failed to create HTTP client");

    let start_time = Instant::now();
    let mut request_id = worker_id as u64 * 1_000_000; // Уникальные ID для каждого воркера

    while start_time.elapsed() < duration || duration.as_secs() == 0 {
        request_id += 1;

        let request_start = Instant::now();
        let (actual_method, actual_params) = if method == "getLatestBlock" {
            // Кастомный метод: сначала получаем актуальный слот, затем getBlock
            let slot_request_id = request_id;
            request_id += 1; // Используем следующий ID для getBlock
            
            match get_latest_slot(&client, &url, slot_request_id).await {
                Some(slot) => {
                    if debug {
                        println!("[Worker {}] Got latest slot: {}", worker_id, slot);
                    }
                    
                    // Формируем параметры для getBlock
                    // Если в params есть опции для getBlock, используем их, иначе дефолтные
                    let block_params = if !params.is_empty() && params.len() > 1 {
                        // params[0] должен быть старый слот (игнорируем), params[1] - опции
                        vec![
                            serde_json::Value::Number(slot.into()),
                            params[1].clone(),
                        ]
                    } else if !params.is_empty() {
                        // Только опции без слота
                        vec![
                            serde_json::Value::Number(slot.into()),
                            params[0].clone(),
                        ]
                    } else {
                        // Дефолтные опции
                        vec![
                            serde_json::Value::Number(slot.into()),
                            serde_json::json!({
                                "commitment": "finalized",
                                "encoding": "json",
                                "transactionDetails": "full",
                                "maxSupportedTransactionVersion": 0,
                                "rewards": false
                            }),
                        ]
                    };
                    
                    ("getBlock".to_string(), block_params)
                }
                None => {
                    if debug {
                        println!("[Worker {}] Failed to get latest slot", worker_id);
                    }
                    stats.record_rpc_error();
                    sleep(Duration::from_millis(timeout_ms)).await;
                    continue;
                }
            }
        } else {
            (method.clone(), params.clone())
        };

        match send_rpc_request(&client, &url, &actual_method, actual_params, request_id).await {
            Ok(json_response) => {
                let response_time = request_start.elapsed();
                let response_time_micros = response_time.as_micros() as u64;
                
                if json_response.error.is_none() {
                    if debug {
                        println!("[Worker {}] Success - Response: {}", worker_id, 
                            serde_json::to_string_pretty(&json_response).unwrap_or_else(|_| format!("{:?}", json_response)));
                    }
                    stats.record_success(response_time_micros);
                } else {
                    if debug {
                        println!("[Worker {}] RPC Error: {:?}", worker_id, json_response.error);
                    }
                    stats.record_rpc_error();
                }
            }
            Err(e) => {
                // Проверяем, является ли это ошибкой парсинга JSON
                if e.is_decode() {
                    if debug {
                        println!("[Worker {}] JSON Parse Error: {}", worker_id, e);
                    }
                    stats.record_json_parse_error();
                } else if e.is_status() {
                    // HTTP ошибка
                    if let Some(status) = e.status() {
                        let status_code = status.as_u16();
                        let reason = status.canonical_reason().unwrap_or("Unknown");
                        if debug {
                            println!("[Worker {}] HTTP Error Status: {} {}", worker_id, status_code, reason);
                        }
                        stats.record_http_error(status_code, reason);
                    } else {
                        if debug {
                            println!("[Worker {}] Request Error: {}", worker_id, e);
                        }
                        stats.record_network_error();
                    }
                } else if e.is_timeout() {
                    if debug {
                        println!("[Worker {}] Request Timeout: {}", worker_id, e);
                    }
                    stats.record_http_timeout();
                } else {
                    if debug {
                        println!("[Worker {}] Request Error: {}", worker_id, e);
                    }
                    stats.record_network_error();
                }
            }
        }

        // Таймаут между запросами
        sleep(Duration::from_millis(timeout_ms)).await;
    }
}

fn load_config(config_path: &str) -> Result<Config, Box<dyn std::error::Error>> {
    let content = fs::read_to_string(config_path)?;
    let config: Config = toml::from_str(&content)?;
    Ok(config)
}

fn extract_host_from_url(url: &str) -> Option<String> {
    // Простой парсинг URL для извлечения хоста
    if let Some(start) = url.find("://") {
        let after_protocol = &url[start + 3..];
        let host_port = if let Some(end) = after_protocol.find('/') {
            &after_protocol[..end]
        } else if let Some(end) = after_protocol.find('?') {
            &after_protocol[..end]
        } else {
            after_protocol
        };
        // Извлекаем хост (без порта)
        Some(host_port.split(':').next().unwrap_or(host_port).to_string())
    } else {
        None
    }
}

fn ping_host(host: &str, count: usize) -> Result<Vec<f64>, Box<dyn std::error::Error>> {
    let output = Command::new("ping")
        .arg("-c")
        .arg(count.to_string())
        .arg(host)
        .output()?;

    if !output.status.success() {
        return Err(format!("Ping failed: {}", String::from_utf8_lossy(&output.stderr)).into());
    }

    let output_str = String::from_utf8_lossy(&output.stdout);
    let mut latencies = Vec::new();

    // Парсим вывод ping (формат: "64 bytes from ... time=12.345 ms" или "time=12.345ms")
    for line in output_str.lines() {
        // Ищем паттерн time=XXX ms или time=XXXms
        if let Some(time_pos) = line.find("time=") {
            let after_time = &line[time_pos + 5..];
            // Пробуем найти " ms" или "ms"
            let latency_str = if let Some(ms_pos) = after_time.find(" ms") {
                &after_time[..ms_pos]
            } else if let Some(ms_pos) = after_time.find("ms") {
                &after_time[..ms_pos]
            } else {
                continue;
            };
            
            if let Ok(latency) = latency_str.trim().parse::<f64>() {
                latencies.push(latency);
            }
        }
    }

    Ok(latencies)
}

fn perform_ping_test(url: &str) {
    println!("\n=== Preliminary Ping Test (10 packets) ===");
    
    let host = match extract_host_from_url(url) {
        Some(h) => h,
        None => {
            println!("Failed to extract host from URL: {}", url);
            return;
        }
    };

    println!("Pinging host: {}", host);
    
    match ping_host(&host, 10) {
        Ok(latencies) => {
            if latencies.is_empty() {
                println!("Failed to get ping results");
                return;
            }

            let avg = latencies.iter().sum::<f64>() / latencies.len() as f64;
            let min = latencies.iter().fold(f64::INFINITY, |a, &b| a.min(b));
            let max = latencies.iter().fold(f64::NEG_INFINITY, |a, &b| a.max(b));

            println!("Ping results:");
            println!("  Packets sent: 10");
            println!("  Responses received: {}", latencies.len());
            println!("  Minimum latency: {:.2} ms", min);
            println!("  Maximum latency: {:.2} ms", max);
            println!("  Average latency: {:.2} ms", avg);
            
            if latencies.len() < 10 {
                println!("  Warning: {} packets lost", 10 - latencies.len());
            }
        }
        Err(e) => {
            println!("Error executing ping: {}", e);
            println!("Make sure 'ping' command is available in the system");
        }
    }
    println!();
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    let stats = Stats::new();
    let mut handles = Vec::new();

    // Если указан конфиг, загружаем параметры из него
    if let Some(config_path) = &args.config {
        if !Path::new(config_path).exists() {
            return Err(format!("Configuration file not found: {}", config_path).into());
        }

        let config = load_config(config_path)?;

        // Используем параметры из конфига, если они указаны, иначе из аргументов
        let url = config.url.as_ref().unwrap_or(&args.url).clone();
        let timeout_ms = config.timeout_ms.unwrap_or(args.timeout_ms);
        let duration_secs = config.duration.unwrap_or(args.duration);
        let http_timeout_secs = config.http_timeout.unwrap_or(args.http_timeout);
        let duration = Duration::from_secs(duration_secs);
        let http_timeout = Duration::from_secs(http_timeout_secs);

        // Выполняем предварительный ping тест, если указан флаг
        if args.ping {
            perform_ping_test(&url);
        }

        println!("=== Stress Test Settings (from config: {}) ===", config_path);
        println!("URL: {}", url);
        println!("Request timeout: {} ms", timeout_ms);
        println!("HTTP timeout: {} sec", http_timeout_secs);
        println!("Duration: {} sec", duration_secs);
        println!("Debug mode: {}", if args.debug { "enabled" } else { "disabled" });
        println!("\nMethods from config:");
        for method_config in &config.methods {
            println!("  - {} (workers: {})", method_config.method, method_config.workers);
        }
        println!("\nStarting test...");

        // Запускаем воркеры для каждого метода из конфига
        let mut worker_id_counter = 0;
        for method_config in &config.methods {
            let params = method_config.params.clone().unwrap_or_default();
            for _ in 0..method_config.workers {
                let handle = tokio::spawn(worker(
                    worker_id_counter,
                    url.clone(),
                    method_config.method.clone(),
                    params.clone(),
                    timeout_ms,
                    http_timeout,
                    stats.clone(),
                    duration,
                    args.debug,
                ));
                handles.push(handle);
                worker_id_counter += 1;
            }
        }
    } else {
        // Используем параметры из командной строки
        println!("=== Stress Test Settings ===");
        println!("URL: {}", args.url);
        println!("Method: {}", args.method);
        println!("Workers: {}", args.workers);
        println!("Request timeout: {} ms", args.timeout_ms);
        println!("HTTP timeout: {} sec", args.http_timeout);
        println!("Duration: {} sec", args.duration);
        println!("Debug mode: {}", if args.debug { "enabled" } else { "disabled" });
        println!("\nStarting test...");

        // Выполняем предварительный ping тест, если указан флаг
        if args.ping {
            perform_ping_test(&args.url);
        }

        let duration = Duration::from_secs(args.duration);
        let http_timeout = Duration::from_secs(args.http_timeout);

        // Запускаем воркеры
        for i in 0..args.workers {
            let handle = tokio::spawn(worker(
                i,
                args.url.clone(),
                args.method.clone(),
                Vec::new(), // Без параметров по умолчанию
                args.timeout_ms,
                http_timeout,
                stats.clone(),
                duration,
                args.debug,
            ));
            handles.push(handle);
        }
    }

    // Ждем завершения всех воркеров
    for handle in handles {
        let _ = handle.await;
    }

    // Выводим статистику
    stats.print_summary();

    Ok(())
}

