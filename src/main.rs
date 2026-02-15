use clap::Parser;
use serde::{Deserialize, Serialize};
use dashmap::DashMap;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::time::sleep;
use tokio::signal;
use crossbeam::queue::SegQueue;
use solana_sdk::{
    signature::{Keypair, Signer},
    transaction::Transaction,
    pubkey::Pubkey,
    system_instruction,
    hash::Hash,
};
use std::str::FromStr;

// Максимальное количество времен ответов для перцентилей (sampling)
const MAX_SAMPLES: usize = 100_000;

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
    /// Private key in base58 format or array of bytes for signing transactions
    #[serde(default)]
    private_key: Option<serde_json::Value>,
    /// Skip preflight simulation for sendTransaction (default: true)
    #[serde(default)]
    skip_preflight: Option<bool>,
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
struct MethodStats {
    total_requests: Arc<AtomicU64>,
    successful_requests: Arc<AtomicU64>,
    http_errors: Arc<DashMap<String, Arc<AtomicU64>>>,
    http_timeouts: Arc<AtomicU64>,
    json_parse_errors: Arc<AtomicU64>,
    network_errors: Arc<AtomicU64>,
    rpc_errors: Arc<AtomicU64>,
    response_times: Arc<SegQueue<u64>>, // microseconds
    sample_count: Arc<AtomicU64>, // для sampling
}

impl MethodStats {
    fn new() -> Self {
        Self {
            total_requests: Arc::new(AtomicU64::new(0)),
            successful_requests: Arc::new(AtomicU64::new(0)),
            http_errors: Arc::new(DashMap::new()),
            http_timeouts: Arc::new(AtomicU64::new(0)),
            json_parse_errors: Arc::new(AtomicU64::new(0)),
            network_errors: Arc::new(AtomicU64::new(0)),
            rpc_errors: Arc::new(AtomicU64::new(0)),
            response_times: Arc::new(SegQueue::new()),
            sample_count: Arc::new(AtomicU64::new(0)),
        }
    }

    fn record_success(&self, response_time_micros: u64) {
        self.total_requests.fetch_add(1, Ordering::Relaxed);
        self.successful_requests.fetch_add(1, Ordering::Relaxed);
        
        // Sampling: сохраняем только первые MAX_SAMPLES
        let count = self.sample_count.fetch_add(1, Ordering::Relaxed);
        if count < MAX_SAMPLES as u64 {
            self.response_times.push(response_time_micros);
        }
    }

    fn record_http_error(&self, status_code: u16, reason: &str) {
        self.total_requests.fetch_add(1, Ordering::Relaxed);
        
        // Оптимизация: используем Cow для избежания лишних аллокаций
        // Для частых ошибок (429, 500) строка будет переиспользована
        let error_key = format!("{} {}", status_code, reason);
        
        // DashMap: lock-free доступ
        self.http_errors
            .entry(error_key)
            .or_insert_with(|| Arc::new(AtomicU64::new(0)))
            .fetch_add(1, Ordering::Relaxed);
    }

    fn record_http_timeout(&self) {
        self.total_requests.fetch_add(1, Ordering::Relaxed);
        self.http_timeouts.fetch_add(1, Ordering::Relaxed);
    }

    fn record_json_parse_error(&self) {
        self.total_requests.fetch_add(1, Ordering::Relaxed);
        self.json_parse_errors.fetch_add(1, Ordering::Relaxed);
    }

    fn record_network_error(&self) {
        self.total_requests.fetch_add(1, Ordering::Relaxed);
        self.network_errors.fetch_add(1, Ordering::Relaxed);
    }

    fn record_rpc_error(&self) {
        self.total_requests.fetch_add(1, Ordering::Relaxed);
        self.rpc_errors.fetch_add(1, Ordering::Relaxed);
    }

    fn print_summary(&self, method_name: &str) {
        let total = self.total_requests.load(Ordering::Relaxed);
        let successful = self.successful_requests.load(Ordering::Relaxed);
        let http_timeouts = self.http_timeouts.load(Ordering::Relaxed);
        let json_parse_errors = self.json_parse_errors.load(Ordering::Relaxed);
        let network_errors = self.network_errors.load(Ordering::Relaxed);
        let rpc_errors = self.rpc_errors.load(Ordering::Relaxed);

        // Собираем все времена ответов (уже ограничены MAX_SAMPLES)
        let mut times: Vec<u64> = Vec::new();
        while let Some(time) = self.response_times.pop() {
            times.push(time);
        }

        let avg_latency = if !times.is_empty() {
            let sum: u64 = times.iter().sum();
            (sum as f64 / times.len() as f64) / 1000.0
        } else {
            0.0
        };

        let min_latency = times.iter().min().map(|&t| t as f64 / 1000.0).unwrap_or(0.0);
        let max_latency = times.iter().max().map(|&t| t as f64 / 1000.0).unwrap_or(0.0);

        // Вычисляем перцентили
        let (p50, p95, p99) = if !times.is_empty() {
            times.sort_unstable(); // sort_unstable быстрее чем sort
            let len = times.len();
            let p50_idx = ((len as f64 * 0.50).ceil() as usize).min(len - 1);
            let p95_idx = ((len as f64 * 0.95).ceil() as usize).min(len - 1);
            let p99_idx = ((len as f64 * 0.99).ceil() as usize).min(len - 1);
            
            (
                times[p50_idx] as f64 / 1000.0,
                times[p95_idx] as f64 / 1000.0,
                times[p99_idx] as f64 / 1000.0,
            )
        } else {
            (0.0, 0.0, 0.0)
        };

        let success_rate = if total > 0 {
            (successful as f64 / total as f64) * 100.0
        } else {
            0.0
        };

        println!("\n=== Method: {} ===", method_name);
        println!("Total requests: {}", total);
        println!("Successful: {} ({:.2}%)", successful, success_rate);
        println!("\nErrors:");

        // Выводим HTTP ошибки по каждому статусу
        // Оптимизация: используем итератор напрямую, избегая лишних аллокаций
        if !self.http_errors.is_empty() {
            // Собираем только при необходимости (для сортировки)
            let mut error_vec: Vec<_> = self.http_errors.iter()
                .map(|entry| (entry.key().clone(), entry.value().load(Ordering::Relaxed)))
                .collect();
            error_vec.sort_unstable_by(|(k1, _), (k2, _)| k1.cmp(k2));
            for (error_name, count) in error_vec {
                println!("  {}: {}", error_name, count);
            }
        }

        println!("  HTTP timeouts: {}", http_timeouts);
        println!("  JSON parse errors: {}", json_parse_errors);
        println!("  Network errors: {}", network_errors);
        println!("  RPC errors: {}", rpc_errors);
        println!("\nLatency:");
        println!("  Average: {:.2} ms", avg_latency);
        if !times.is_empty() {
            println!("  Minimum: {:.2} ms", min_latency);
            println!("  Maximum: {:.2} ms", max_latency);
            println!("  p50: {:.2} ms", p50);
            println!("  p95: {:.2} ms", p95);
            println!("  p99: {:.2} ms", p99);
            if total > MAX_SAMPLES as u64 {
                println!("  Note: Latency stats based on {} samples (sampling)", times.len());
            }
        }
    }
}

#[derive(Clone)]
struct Stats {
    methods: Arc<DashMap<String, MethodStats>>,
}

impl Stats {
    fn new() -> Self {
        Self {
            methods: Arc::new(DashMap::new()),
        }
    }

    // Оптимизация: возвращаем ссылку вместо клонирования, где возможно
    fn get_or_create_method_stats(&self, method: &str) -> MethodStats {
        // DashMap: lock-free доступ, без двойного lock
        // Используем Cow для избежания лишних аллокаций
        self.methods
            .entry(method.to_string())
            .or_insert_with(MethodStats::new)
            .clone()
    }

    // Оптимизация: прямой доступ к MethodStats без промежуточных вызовов
    fn record_success(&self, method: &str, response_time_micros: u64) {
        self.get_or_create_method_stats(method).record_success(response_time_micros);
    }

    fn record_http_error(&self, method: &str, status_code: u16, reason: &str) {
        self.get_or_create_method_stats(method).record_http_error(status_code, reason);
    }

    fn record_http_timeout(&self, method: &str) {
        self.get_or_create_method_stats(method).record_http_timeout();
    }

    fn record_json_parse_error(&self, method: &str) {
        self.get_or_create_method_stats(method).record_json_parse_error();
    }

    fn record_network_error(&self, method: &str) {
        self.get_or_create_method_stats(method).record_network_error();
    }

    fn record_rpc_error(&self, method: &str) {
        self.get_or_create_method_stats(method).record_rpc_error();
    }

    fn print_summary(&self) {
        if self.methods.is_empty() {
            println!("\n=== Stress Test Statistics ===");
            println!("No statistics collected");
            return;
        }

        println!("\n=== Stress Test Statistics ===");
        
        // Выводим статистику для каждого метода
        // Оптимизация: используем sort_unstable для лучшей производительности
        let mut method_vec: Vec<_> = self.methods.iter()
            .map(|entry| (entry.key().clone(), entry.value().clone()))
            .collect();
        method_vec.sort_unstable_by(|(k1, _), (k2, _)| k1.cmp(k2));
        
        for (method_name, method_stats) in method_vec {
            method_stats.print_summary(&method_name);
        }

        // Общая статистика
        let total_requests: u64 = self.methods
            .iter()
            .map(|s| s.value().total_requests.load(Ordering::Relaxed))
            .sum();
        
        let total_successful: u64 = self.methods
            .iter()
            .map(|s| s.value().successful_requests.load(Ordering::Relaxed))
            .sum();

        let total_success_rate = if total_requests > 0 {
            (total_successful as f64 / total_requests as f64) * 100.0
        } else {
            0.0
        };

        println!("\n=== Overall Statistics ===");
        println!("Total requests (all methods): {}", total_requests);
        println!("Total successful (all methods): {} ({:.2}%)", total_successful, total_success_rate);
    }
}

// Оптимизация: используем статические строки где возможно
const JSONRPC_VERSION: &str = "2.0";

async fn send_rpc_request(
    client: &reqwest::Client,
    url: &str,
    method: &str,
    params: Vec<serde_json::Value>,
    request_id: u64,
) -> Result<JsonRpcResponse, reqwest::Error> {
    let request = JsonRpcRequest {
        jsonrpc: JSONRPC_VERSION.to_string(),
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

// Получаем последний blockhash для обновления транзакций
async fn get_latest_blockhash(
    client: &reqwest::Client,
    url: &str,
    request_id: u64,
    debug: bool,
) -> Option<String> {
    // HTTP RPC запрос
    match send_rpc_request(client, url, "getLatestBlockhash", vec![serde_json::json!({"commitment": "finalized"})], request_id).await {
        Ok(response) => {
            if let Some(error) = response.error {
                if debug {
                    eprintln!("getLatestBlockhash RPC error: code={}, message={}", error.code, error.message);
                }
                return None;
            }
            
            if let Some(result) = response.result {
                // Проверяем разные форматы ответа
                if let Ok(result_obj) = serde_json::from_value::<serde_json::Value>(result.clone()) {
                    // Формат 1: result.value.blockhash (стандартный формат Solana RPC)
                    if let Some(value_obj) = result_obj.get("value") {
                        if let Some(blockhash) = value_obj.get("blockhash").and_then(|v| v.as_str()) {
                            return Some(blockhash.to_string());
                        }
                    }
                    
                    // Формат 2: result.blockhash (прямой формат)
                    if let Some(blockhash) = result_obj.get("blockhash").and_then(|v| v.as_str()) {
                        return Some(blockhash.to_string());
                    }
                }
                
                // Формат 3: строка напрямую (некоторые RPC возвращают так)
                if let Ok(blockhash_str) = serde_json::from_value::<String>(result.clone()) {
                    return Some(blockhash_str);
                }
                
                if debug {
                    eprintln!("getLatestBlockhash: unexpected response format: {:?}", result);
                }
            } else {
                if debug {
                    eprintln!("getLatestBlockhash: response has no result field");
                }
            }
        }
        Err(e) => {
            if debug {
                eprintln!("getLatestBlockhash request failed: {}", e);
            }
        }
    }
    None
}

// Парсит приватный ключ из base58 строки или массива байтов
fn parse_private_key(key_value: &serde_json::Value) -> Result<Keypair, Box<dyn std::error::Error>> {
    let key_bytes = match key_value {
        serde_json::Value::String(key_str) => {
            // Формат: base58 строка
            bs58::decode(key_str)
                .into_vec()
                .map_err(|e| format!("Failed to decode private key from base58: {}", e))?
        }
        serde_json::Value::Array(bytes_array) => {
            // Формат: массив байтов [112, 23, 3, ...]
            bytes_array
                .iter()
                .map(|v| {
                    v.as_u64()
                        .and_then(|n| u8::try_from(n).ok())
                        .ok_or_else(|| "Invalid byte value in array".to_string())
                })
                .collect::<Result<Vec<u8>, String>>()
                .map_err(|e| format!("Failed to parse byte array: {}", e))?
        }
        _ => {
            return Err("private_key must be either a base58 string or an array of bytes".into());
        }
    };
    
    if key_bytes.len() != 64 {
        return Err(format!("Invalid key length: expected 64 bytes, got {}", key_bytes.len()).into());
    }
    
    Keypair::from_bytes(&key_bytes)
        .map_err(|e| format!("Failed to create keypair: {}", e))
        .map_err(|e| e.into())
}

// Создает простую транзакцию перевода (для stress test)
fn create_test_transaction(
    from_keypair: &Keypair,
    to_pubkey: &Pubkey,
    recent_blockhash: Hash,
) -> Transaction {
    let instruction = system_instruction::transfer(
        &from_keypair.pubkey(),
        to_pubkey,
        0, // 0 lamports - просто для теста
    );
    
    Transaction::new_signed_with_payer(
        &[instruction],
        Some(&from_keypair.pubkey()),
        &[from_keypair],
        recent_blockhash,
    )
}

// Обновляет транзакцию с новым blockhash и переподписывает
fn update_transaction_blockhash(
    transaction: &mut Transaction,
    keypair: &Keypair,
    new_blockhash: Hash,
) {
    transaction.message.recent_blockhash = new_blockhash;
    transaction.sign(&[keypair], new_blockhash);
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
    private_key: Option<serde_json::Value>,
    skip_preflight: bool,
    should_stop: Arc<AtomicBool>,
) {
    // Оптимизация HTTP клиента: keep-alive, connection pooling, TCP_NODELAY
    let client = reqwest::Client::builder()
        .timeout(http_timeout)
        .tcp_keepalive(Duration::from_secs(60))
        .tcp_nodelay(true) // Отключаем Nagle algorithm для низкой латентности
        .pool_max_idle_per_host(20) // Увеличиваем pool для лучшей производительности
        .pool_idle_timeout(Duration::from_secs(90))
        .build()
        .expect("Failed to create HTTP client");

    // Оптимизация: кешируем MethodStats для этого воркера, чтобы избежать повторных lookup
    // Для обычных методов используем кеш, для getLatestBlock - нет (разные stats_method)
    let base_method = method.clone();
    let base_method_stats = stats.get_or_create_method_stats(&base_method);

    // Парсим приватный ключ если указан
    let keypair = if let Some(ref key_value) = private_key {
        match parse_private_key(key_value) {
            Ok(kp) => {
                if debug {
                    println!("[Worker {}] Loaded private key: {}", worker_id, kp.pubkey());
                }
                Some(kp)
            }
            Err(e) => {
                eprintln!("[Worker {}] Failed to parse private key: {}", worker_id, e);
                return;
            }
        }
    } else {
        None
    };

    let start_time = Instant::now();
    let mut request_id = worker_id as u64 * 1_000_000; // Уникальные ID для каждого воркера
    
    // Оптимизация: предварительно вычисляем timeout Duration
    let timeout_duration = Duration::from_millis(timeout_ms);
    
    // Если есть приватный ключ и метод sendTransaction, создаем шаблон транзакции
    let mut transaction_template: Option<Transaction> = if keypair.is_some() && method == "sendTransaction" {
        // Получаем начальный blockhash для создания транзакции
        // Продолжаем пытаться до получения blockhash (без ограничений)
        let mut initial_blockhash_str = None;
        let mut attempts = 0;
        
        while initial_blockhash_str.is_none() {
            attempts += 1;
            if let Some(hash_str) = get_latest_blockhash(&client, &url, request_id + attempts, debug).await {
                initial_blockhash_str = Some(hash_str);
                if debug && attempts > 1 {
                    println!("[Worker {}] Got initial blockhash after {} attempts", worker_id, attempts);
                }
            } else {
                if debug && attempts % 10 == 0 {
                    println!("[Worker {}] Still trying to get initial blockhash (attempt {})...", worker_id, attempts);
                }
                // Небольшая задержка перед следующей попыткой
                sleep(Duration::from_millis(50)).await;
            }
        }
        
        let initial_blockhash_str = initial_blockhash_str.unwrap();
        
        request_id += 1;
        
        if let Ok(initial_blockhash) = Hash::from_str(&initial_blockhash_str) {
            let kp = keypair.as_ref().unwrap();
            let to_pubkey = kp.pubkey(); // Отправляем на самого себя
            Some(create_test_transaction(kp, &to_pubkey, initial_blockhash))
        } else {
            eprintln!("[Worker {}] Failed to parse initial blockhash", worker_id);
            return;
        }
    } else {
        None
    };

    while (start_time.elapsed() < duration || duration.as_secs() == 0) && !should_stop.load(Ordering::Relaxed) {
        request_id += 1;

        let request_start = Instant::now();
        let (actual_method, actual_params, stats_method) = if method == "getLatestBlock" {
            // Кастомный метод: сначала получаем актуальный слот, затем getBlock
            let slot_request_id = request_id;
            request_id += 1; // Используем следующий ID для getBlock
            
            match get_latest_slot(&client, &url, slot_request_id).await {
                Some(slot) => {
                    if debug {
                        println!("[Worker {}] Got latest slot: {}", worker_id, slot);
                    }
                    
                    // Формируем параметры для getBlock
                    let block_params = if !params.is_empty() && params.len() > 1 {
                        vec![
                            serde_json::Value::Number(slot.into()),
                            params[1].clone(),
                        ]
                    } else if !params.is_empty() {
                        vec![
                            serde_json::Value::Number(slot.into()),
                            params[0].clone(),
                        ]
                    } else {
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
                    
                    ("getBlock".to_string(), block_params, "getLatestBlock".to_string())
                }
                None => {
                    if debug {
                        println!("[Worker {}] Failed to get latest slot", worker_id);
                    }
                    // Для getLatestBlock используем stats_method из конфига
                    stats.record_rpc_error(&method);
                    sleep(timeout_duration).await;
                    continue;
                }
            }
        } else if method == "sendTransaction" && keypair.is_some() {
            // Обновляем blockhash и переподписываем транзакцию
            if let Some(ref mut tx) = transaction_template {
                let blockhash_request_id = request_id;
                request_id += 1;
                
                // Продолжаем пытаться получить blockhash до успеха (без ограничений)
                let mut latest_blockhash_str = None;
                let mut blockhash_attempts = 0;
                
                while latest_blockhash_str.is_none() {
                    blockhash_attempts += 1;
                    if let Some(hash_str) = get_latest_blockhash(&client, &url, blockhash_request_id + blockhash_attempts as u64, debug).await {
                        latest_blockhash_str = Some(hash_str);
                    } else {
                        // Небольшая задержка перед следующей попыткой
                        sleep(Duration::from_millis(10)).await;
                    }
                }
                
                let latest_blockhash_str = latest_blockhash_str.unwrap();
                
                if let Ok(new_blockhash) = Hash::from_str(&latest_blockhash_str) {
                    update_transaction_blockhash(tx, keypair.as_ref().unwrap(), new_blockhash);
                    
                    // Сериализуем транзакцию в base64
                    match bincode::serialize(tx) {
                        Ok(bytes) => {
                            use base64::{Engine as _, engine::general_purpose};
                            let base64_tx = general_purpose::STANDARD.encode(&bytes);
                            let tx_params = vec![
                                serde_json::Value::String(base64_tx),
                                serde_json::json!({
                                    "encoding": "base64",
                                    "skipPreflight": skip_preflight
                                })
                            ];
                            
                            (method.clone(), tx_params, method.clone())
                        }
                        Err(e) => {
                            if debug {
                                println!("[Worker {}] Serialization error: {}", worker_id, e);
                            }
                            base_method_stats.record_network_error();
                            sleep(timeout_duration).await;
                            continue;
                        }
                    }
                } else {
                    if debug {
                        println!("[Worker {}] Failed to parse blockhash", worker_id);
                    }
                    base_method_stats.record_rpc_error();
                    sleep(timeout_duration).await;
                    continue;
                }
            } else {
                // Fallback на старый способ (если транзакция не была создана)
                let tx_params = if params.is_empty() {
                    Vec::new()
                } else {
                    params.clone()
                };
                (method.clone(), tx_params, method.clone())
            }
        } else if method == "simulateTransaction" {
            // Для simulateTransaction используем старый способ или replaceRecentBlockhash
            let tx_params = if params.is_empty() {
                Vec::new()
            } else {
                params.clone()
            };
            (method.clone(), tx_params, method.clone())
        } else {
            (method.clone(), params.clone(), method.clone())
        };

        match send_rpc_request(&client, &url, &actual_method, actual_params, request_id).await {
            Ok(json_response) => {
                let response_time = request_start.elapsed();
                let response_time_micros = response_time.as_micros() as u64;
                
                // Детальное логирование для sendTransaction (только в debug режиме)
                if actual_method == "sendTransaction" && debug {
                    println!("\n=== [Worker {}] sendTransaction Response ===", worker_id);
                    println!("Latency: {:.2}ms", response_time_micros as f64 / 1000.0);
                    if let Some(ref result) = json_response.result {
                        println!("Result: {}", serde_json::to_string_pretty(result).unwrap_or_else(|_| format!("{:?}", result)));
                    }
                    if let Some(ref error) = json_response.error {
                        println!("Error: {}", serde_json::to_string_pretty(error).unwrap_or_else(|_| format!("{:?}", error)));
                    }
                    println!("Full Response: {}", serde_json::to_string_pretty(&json_response).unwrap_or_else(|_| format!("{:?}", json_response)));
                    println!("==========================================\n");
                }
                
                if json_response.error.is_none() {
                    if debug && actual_method != "sendTransaction" {
                        // Упрощенный вывод без pretty printing для производительности
                        println!("[Worker {}] Success - Method: {}, Latency: {:.2}ms", 
                            worker_id, actual_method, response_time_micros as f64 / 1000.0);
                    }
                    // Оптимизация: используем кешированный method_stats для обычных методов
                    if stats_method == base_method {
                        base_method_stats.record_success(response_time_micros);
                    } else {
                        stats.record_success(&stats_method, response_time_micros);
                    }
                } else {
                    if debug && actual_method != "sendTransaction" {
                        if let Some(ref err) = json_response.error {
                            println!("[Worker {}] RPC Error: code={}, msg={}", 
                                worker_id, err.code, err.message);
                        }
                    }
                    // Оптимизация: используем кешированный method_stats для обычных методов
                    if stats_method == base_method {
                        base_method_stats.record_rpc_error();
                    } else {
                        stats.record_rpc_error(&stats_method);
                    }
                }
            }
            Err(e) => {
                // Проверяем, является ли это ошибкой парсинга JSON
                if e.is_decode() {
                    if debug {
                        println!("[Worker {}] JSON Parse Error: {}", worker_id, e);
                    }
                    if stats_method == base_method {
                        base_method_stats.record_json_parse_error();
                    } else {
                        stats.record_json_parse_error(&stats_method);
                    }
                } else if e.is_status() {
                    // HTTP ошибка
                    if let Some(status) = e.status() {
                        let status_code = status.as_u16();
                        let reason = status.canonical_reason().unwrap_or("Unknown");
                        if debug {
                            println!("[Worker {}] HTTP Error: {} {}", worker_id, status_code, reason);
                        }
                        if stats_method == base_method {
                            base_method_stats.record_http_error(status_code, reason);
                        } else {
                            stats.record_http_error(&stats_method, status_code, reason);
                        }
                    } else {
                        if debug {
                            println!("[Worker {}] Request Error: {}", worker_id, e);
                        }
                        if stats_method == base_method {
                            base_method_stats.record_network_error();
                        } else {
                            stats.record_network_error(&stats_method);
                        }
                    }
                } else if e.is_timeout() {
                    if debug {
                        println!("[Worker {}] Request Timeout", worker_id);
                    }
                    if stats_method == base_method {
                        base_method_stats.record_http_timeout();
                    } else {
                        stats.record_http_timeout(&stats_method);
                    }
                } else {
                    if debug {
                        println!("[Worker {}] Network Error: {}", worker_id, e);
                    }
                    if stats_method == base_method {
                        base_method_stats.record_network_error();
                    } else {
                        stats.record_network_error(&stats_method);
                    }
                }
            }
        }

        // Таймаут между запросами - используем предвычисленный Duration
        sleep(timeout_duration).await;
    }
}

fn load_config(config_path: &str) -> Result<Config, Box<dyn std::error::Error>> {
    let content = fs::read_to_string(config_path)?;
    let config: Config = serde_json::from_str(&content)?;
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
    let should_stop = Arc::new(AtomicBool::new(false));

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
                    method_config.private_key.clone(),
                    method_config.skip_preflight.unwrap_or(true), // По умолчанию true
                    should_stop.clone(),
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
                None, // Нет приватного ключа в CLI режиме
                true, // По умолчанию skip_preflight = true
                should_stop.clone(),
            ));
            handles.push(handle);
        }
    }

    // Запускаем задачу для обработки Ctrl+C
    let should_stop_clone = should_stop.clone();
    tokio::spawn(async move {
        if let Ok(()) = signal::ctrl_c().await {
            println!("\n\nReceived interrupt signal (Ctrl+C). Stopping workers gracefully...");
            should_stop_clone.store(true, Ordering::Relaxed);
        }
    });

    // Ждем завершения всех воркеров
    for handle in handles {
        let _ = handle.await;
    }

    // Выводим статистику
    println!("\n=== Final Statistics ===");
    stats.print_summary();

    Ok(())
}
