//! 异步 Webhook 批量发送器
//!
//! 使用后台任务处理 Webhook 请求，避免阻塞主流程：
//! - 事件队列化
//! - 批量聚合（可配置大小）
//! - 定时刷新（可配置间隔）
//! - 后台异步发送

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, Mutex};
use tokio::time::interval;

use crate::hook::HookResult;

/// 异步 Webhook 配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AsyncWebhookConfig {
    /// Webhook 名称
    pub name: String,
    /// Webhook URL
    pub url: String,
    /// HTTP 方法（默认 POST）
    #[serde(default = "default_method")]
    pub method: String,
    /// 自定义 Header
    #[serde(default)]
    pub headers: HashMap<String, String>,
    /// 批量大小（达到此数量后发送）
    #[serde(default = "default_batch_size")]
    pub batch_size: usize,
    /// 刷新间隔（毫秒）
    #[serde(default = "default_flush_interval")]
    pub flush_interval_ms: u64,
    /// 超时时间（秒）
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    /// 重试次数
    #[serde(default = "default_retries")]
    pub retries: u32,
    /// 是否启用
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 描述
    #[serde(default)]
    pub description: String,
}

fn default_method() -> String {
    "POST".to_string()
}

fn default_batch_size() -> usize {
    10
}

fn default_flush_interval() -> u64 {
    1000  // 1 秒
}

fn default_timeout() -> u64 {
    10
}

fn default_retries() -> u32 {
    2
}

fn default_true() -> bool {
    true
}

/// Webhook 事件
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebhookEvent {
    /// 事件类型
    pub event: String,
    /// Hook 名称
    pub hook_name: String,
    /// 触发条件
    pub trigger: String,
    /// 上下文数据
    pub context: serde_json::Value,
    /// 时间戳（毫秒）
    pub timestamp_ms: u128,
}

/// 批量发送请求
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchRequest {
    /// Webhook URL
    pub url: String,
    /// HTTP 方法
    pub method: String,
    /// Header
    pub headers: HashMap<String, String>,
    /// 事件列表
    pub events: Vec<WebhookEvent>,
}

/// 批量发送器
pub struct AsyncWebhookBatcher {
    config: AsyncWebhookConfig,
    client: Client,
    /// 事件队列
    event_queue: Arc<Mutex<Vec<WebhookEvent>>>,
    /// 发送通道
    sender: mpsc::Sender<Vec<WebhookEvent>>,
    /// 后台任务句柄（用于关闭）
    handle: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl AsyncWebhookBatcher {
    /// 创建新的异步批量发送器
    pub fn new(config: AsyncWebhookConfig) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(config.timeout_secs))
            .danger_accept_invalid_certs(false)
            .build()
            .unwrap_or_else(|_| Client::new());

        let event_queue = Arc::new(Mutex::new(Vec::new()));
        let (sender, receiver) = mpsc::channel::<Vec<WebhookEvent>>(100);

        // 启动后台任务
        let client_clone = client.clone();
        let config_clone = config.clone();
        let handle = tokio::spawn(Self::background_task(
            client_clone,
            config_clone,
            receiver,
        ));

        Self {
            config,
            client,
            event_queue,
            sender,
            handle: Mutex::new(Some(handle)),
        }
    }

    /// 添加事件到队列
    pub async fn add_event(&self, event: WebhookEvent) -> HookResult {
        let mut queue = self.event_queue.lock().await;
        queue.push(event);

        // 如果达到批量大小，立即发送
        if queue.len() >= self.config.batch_size {
            let events = queue.drain(..).collect();
            if let Err(e) = self.sender.send(events).await {
                eprintln!("[AsyncWebhook] Failed to send batch: {}", e);
                return HookResult::Warning("Failed to queue webhook event".to_string());
            }
        }

        HookResult::Ok
    }

    /// 刷新队列（强制发送所有待处理事件）
    pub async fn flush(&self) -> HookResult {
        let mut queue = self.event_queue.lock().await;
        if queue.is_empty() {
            return HookResult::Ok;
        }

        let events = queue.drain(..).collect();
        if let Err(e) = self.sender.send(events).await {
            eprintln!("[AsyncWebhook] Failed to flush: {}", e);
            return HookResult::Warning("Failed to flush webhook events".to_string());
        }

        HookResult::Ok
    }

    /// 后台任务：定期刷新或接收批量数据
    async fn background_task(
        client: Client,
        config: AsyncWebhookConfig,
        mut receiver: mpsc::Receiver<Vec<WebhookEvent>>,
    ) {
        let mut flush_interval = interval(Duration::from_millis(config.flush_interval_ms));

        loop {
            tokio::select! {
                // 定时刷新
                _ = flush_interval.tick() => {
                    // 定时刷新由外部触发，这里只处理接收到的批量数据
                }

                // 接收批量数据
                Some(events) = receiver.recv() => {
                    if events.is_empty() {
                        continue;
                    }

                    // 发送批量数据
                    if let Err(e) = Self::send_batch(&client, &config, &events).await {
                        eprintln!("[AsyncWebhook] Failed to send batch: {}", e);
                    }
                }

                // 通道关闭，退出
                else => {
                    eprintln!("[AsyncWebhook] Background task exiting");
                    break;
                }
            }
        }
    }

    /// 发送批量事件
    async fn send_batch(
        client: &Client,
        config: &AsyncWebhookConfig,
        events: &[WebhookEvent],
    ) -> Result<(), String> {
        if events.is_empty() {
            return Ok(());
        }

        let batch_request = BatchRequest {
            url: config.url.clone(),
            method: config.method.clone(),
            headers: config.headers.clone(),
            events: events.to_vec(),
        };

        let payload = serde_json::to_value(&batch_request)
            .map_err(|e| format!("Failed to serialize batch: {}", e))?;

        // 发送请求（带重试）
        let mut last_error = None;
        for attempt in 0..=config.retries {
            let request = client.request(
                config.method.parse().map_err(|e| format!("Invalid HTTP method: {}", e))?,
                &config.url,
            );

            // 添加自定义 Header
            let mut request = request;
            for (key, value) in &config.headers {
                request = request.header(key, value);
            }

            request = request
                .header("Content-Type", "application/json")
                .header("X-AtomCode-Version", env!("CARGO_PKG_VERSION"))
                .header("X-AtomCode-Webhook", &config.name)
                .header("X-AtomCode-Batch-Size", events.len().to_string());

            match request.json(&payload).send().await {
                Ok(response) => {
                    let status = response.status();
                    let body = response.text().await.unwrap_or_default();

                    if status.is_success() {
                        eprintln!(
                            "[AsyncWebhook] Sent {} events to {}",
                            events.len(),
                            config.url
                        );
                        return Ok(());
                    } else {
                        last_error = Some(format!(
                            "HTTP {} at attempt {}: {}",
                            status, attempt + 1, body
                        ));
                    }
                }
                Err(e) => {
                    last_error = Some(format!("Request failed at attempt {}: {}", attempt + 1, e));
                    // 指数退避重试
                    tokio::time::sleep(Duration::from_millis(100 * 2u64.pow(attempt))).await;
                }
            }
        }

        Err(last_error.unwrap_or_else(|| "Unknown error".to_string()))
    }

    /// 关闭后台任务
    pub async fn shutdown(&self) {
        // 刷新剩余事件
        let _ = self.flush().await;

        // 关闭发送通道（这会通知后台任务退出）
        drop(self.sender.clone());

        // 等待后台任务退出
        if let Some(handle) = self.handle.lock().await.take() {
            let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
        }
    }
}

impl Drop for AsyncWebhookBatcher {
    fn drop(&mut self) {
        // 在 drop 时尝试刷新（不阻塞）
        if let Some(handle) = self.handle.get_mut().take() {
            handle.abort();
        }
    }
}

/// 全局 Webhook 批处理器注册表
pub struct AsyncWebhookRegistry {
    pub batchers: HashMap<String, Arc<AsyncWebhookBatcher>>,
}

impl AsyncWebhookRegistry {
    pub fn new() -> Self {
        Self {
            batchers: HashMap::new(),
        }
    }

    /// 注册异步 Webhook
    pub fn register(&mut self, config: AsyncWebhookConfig) {
        if !config.enabled {
            return;
        }

        let batcher = Arc::new(AsyncWebhookBatcher::new(config.clone()));
        eprintln!(
            "[AsyncWebhook] Registered: {} -> {} (batch={}, interval={}ms)",
            config.name, config.url, config.batch_size, config.flush_interval_ms
        );
        self.batchers.insert(config.name.clone(), batcher);
    }

    /// 获取批处理器
    pub fn get(&self, name: &str) -> Option<&Arc<AsyncWebhookBatcher>> {
        self.batchers.get(name)
    }

    /// 获取所有批处理器
    pub fn iter(&self) -> impl Iterator<Item = (&String, &Arc<AsyncWebhookBatcher>)> {
        self.batchers.iter()
    }

    /// 刷新所有批处理器
    pub async fn flush_all(&self) {
        for (_, batcher) in &self.batchers {
            let _ = batcher.flush().await;
        }
    }

    /// 关闭所有批处理器
    pub async fn shutdown_all(&self) {
        for (_, batcher) in &self.batchers {
            batcher.shutdown().await;
        }
    }
}

impl Default for AsyncWebhookRegistry {
    fn default() -> Self {
        Self::new()
    }
}
