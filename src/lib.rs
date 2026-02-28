use std::{collections::HashSet, time::Duration};

use crate::self_balance::{set_monitored_payers, update_balances_from_tx};

use {
    anyhow,
    borsh::BorshDeserialize,
    futures::stream::StreamExt,
    grpc_client::TransactionFormat,
    log::{error, info, warn},
    solana_client::nonblocking::rpc_client::RpcClient,
    solana_commitment_config::CommitmentConfig,
    solana_sdk::{hash::Hash, pubkey::Pubkey, signature::Signature},
    std::{
        collections::HashMap,
        env,
        sync::{
            Arc, LazyLock,
            atomic::{AtomicBool, AtomicU64, Ordering},
        },
    },
    tokio::{self, sync::RwLock, time::Instant},
    tonic::{service::Interceptor, transport::ClientTlsConfig},
    utils::global_broadcast,
    yellowstone_grpc_client::GeyserGrpcClient,
    yellowstone_grpc_proto::{
        geyser::{SubscribeRequestAccountsDataSlice, SubscribeRequestFilterAccounts},
        prelude::{
            CommitmentLevel, SubscribeRequest, SubscribeRequestFilterTransactions,
            subscribe_update::UpdateOneof,
        },
    },
};

pub mod confirm;
pub mod pnl_tracker;
pub mod self_balance;
pub use confirm::confirm_first::confirm_tx;
pub use confirm::confirm_success::confirm_success_tx;
pub use pnl_tracker::{
    PnLSummary, TokenPnL, clear_all_pnl, get_db, init_pnl_db, load_all_pnl, print_pnl_report,
    query_all_pnl, query_payer_pnl, query_pnl_summary, query_sorted_pnl, query_token_pnl,
    start_periodic_report, start_pnl_tracker, to_ui_amount,
};

// 全局连接健康状态
static CONNECTION_HEALTHY: AtomicBool = AtomicBool::new(false);
static LAST_MESSAGE_TIME: AtomicU64 = AtomicU64::new(0);
static TOTAL_RECONNECTS: AtomicU64 = AtomicU64::new(0);
static TOTAL_MESSAGES: AtomicU64 = AtomicU64::new(0);

// 定义交易结果的全局广播 channel
global_broadcast! {
    mod tx_result_channel {
        struct TxResultEvent {
            signature: Signature,
            tx: TransactionFormat,
            status: TradeStatus,
        }
    }
}

/// 交易确认错误类型
#[derive(Clone, Debug)]
pub enum TxConfirmError {
    /// 超时：在指定时间内没有收到交易结果
    Timeout {
        expected_sigs: Vec<Signature>,
        timeout_secs: u64,
    },
    /// 交易失败：收到失败状态
    Failed {
        signature: Signature,
        tx: TransactionFormat,
        error_msg: String,
    },
    /// Meta 缺失：交易没有 meta 数据
    MetaMissing {
        signature: Signature,
        tx: TransactionFormat,
    },
    /// 其他错误
    Other(String),
}

impl std::fmt::Display for TxConfirmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Timeout {
                expected_sigs,
                timeout_secs,
            } => {
                write!(
                    f,
                    "交易超时: 等待 {}秒，期望签名: {:?}",
                    timeout_secs, expected_sigs
                )
            }
            Self::Failed {
                signature,
                error_msg,
                ..
            } => {
                write!(f, "交易失败: {} - {}", signature, error_msg)
            }
            Self::MetaMissing { signature, .. } => {
                write!(f, "交易 Meta 缺失: {}", signature)
            }
            Self::Other(msg) => write!(f, "其他错误: {}", msg),
        }
    }
}

impl std::error::Error for TxConfirmError {}

impl TxConfirmError {
    /// 如果不是超时错误，执行提供的闭包
    ///
    /// # 设计理念
    /// 超时通常表示 nonce 被抢占（可能是其他机器买走了订单），这种情况不需要发送告警通知。
    /// 只有真正的失败（Failed/MetaMissing/Other）才需要用户关注。
    ///
    /// # 参数
    /// - 闭包接收 `&TxConfirmError`，可以访问错误的详细信息
    ///
    /// # 示例
    /// ```rust,no_run
    /// match send_fast(&ixs, &ctx, None, cu).await {
    ///     Ok(sig) => { /* 处理成功 */ }
    ///     Err(e) => {
    ///         e.if_not_timeout(|err| {
    ///             // 只在非超时情况下发送 TG 通知，可以访问 err 的详细信息
    ///             send_tg_alert(&format!("交易失败: {}", err));
    ///         });
    ///     }
    /// }
    /// ```
    pub fn if_not_timeout<F: FnOnce(&Self)>(&self, f: F) {
        if !matches!(self, TxConfirmError::Timeout { .. }) {
            f(self);
        }
    }

    /// 如果不是超时错误，执行提供的异步闭包
    ///
    /// 异步版本，闭包接收 `&TxConfirmError` 并返回 Future
    ///
    /// # 参数
    /// - 闭包接收 `&TxConfirmError`，可以访问错误的详细信息
    ///
    /// # 示例
    /// ```rust,no_run
    /// match send_fast(&ixs, &ctx, None, cu).await {
    ///     Ok(sig) => { /* 处理成功 */ }
    ///     Err(e) => {
    ///         e.if_not_timeout_async(|err| async move {
    ///             send_tg_alert(&format!("交易失败: {}", err)).await;
    ///         }).await;
    ///     }
    /// }
    /// ```
    pub async fn if_not_timeout_async<F, Fut>(&self, f: F)
    where
        F: FnOnce(&Self) -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        if !matches!(self, TxConfirmError::Timeout { .. }) {
            f(self).await;
        }
    }

    /// 判断是否为超时错误
    pub fn is_timeout(&self) -> bool {
        matches!(self, TxConfirmError::Timeout { .. })
    }
}

// 自动转换实现
impl From<Box<dyn std::error::Error + Send + Sync>> for TxConfirmError {
    fn from(e: Box<dyn std::error::Error + Send + Sync>) -> Self {
        Self::Other(e.to_string())
    }
}

impl From<&str> for TxConfirmError {
    fn from(s: &str) -> Self {
        Self::Other(s.to_string())
    }
}

impl From<String> for TxConfirmError {
    fn from(s: String) -> Self {
        Self::Other(s)
    }
}

// Replace with your QuickNode Yellowstone gRPC endpoint
const ENDPOINT: LazyLock<String> = LazyLock::new(|| {
    std::env::var("YELLOWSTONE_GRPC_URL").unwrap_or_else(|_| {
        info!("YELLOWSTONE_GRPC_URL not set, using default endpoint");
        "http://localhost:10000".to_string()
    })
});

pub static JSON_RPC_CLIENT: LazyLock<Arc<RpcClient>> = LazyLock::new(|| {
    let url = env::var("JSON_RPC_URL").expect("JSON_RPC_URL not set");
    Arc::new(RpcClient::new_with_commitment(
        url,
        CommitmentConfig::processed(),
    ))
});

#[derive(Clone, Debug)]
pub enum TradeStatus {
    /// 交易成功
    Success {
        signature: Signature,
        tx: TransactionFormat,
    },
    /// 交易失败：有 meta 且 status 为 Err
    Failed {
        signature: Signature,
        tx: TransactionFormat,
        error_msg: String,
    },
    /// Meta 缺失：交易没有 meta 数据
    MetaMissing {
        signature: Signature,
        tx: TransactionFormat,
    },
}

impl TradeStatus {
    pub fn success(&self) -> bool {
        matches!(self, TradeStatus::Success { .. })
    }

    pub fn signature(&self) -> Signature {
        match self {
            TradeStatus::Success { signature, .. } => *signature,
            TradeStatus::Failed { signature, .. } => *signature,
            TradeStatus::MetaMissing { signature, .. } => *signature,
        }
    }

    pub fn tx(&self) -> &TransactionFormat {
        match self {
            TradeStatus::Success { tx, .. } => tx,
            TradeStatus::Failed { tx, .. } => tx,
            TradeStatus::MetaMissing { tx, .. } => tx,
        }
    }
}

// 假设你已定义的NonceInfo结构体（存储每个账户的前后hash）
#[derive(Debug, Clone, Default)]
struct NonceInfo {
    pre_hash: Hash,
    cur_hash: Hash,
}

// 全局缓存：key=Nonce账户Pubkey，value=该账户的hash信息
static NONCE_CACHE: LazyLock<RwLock<HashMap<Pubkey, NonceInfo>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

/// 初始化指定Nonce账户的hash缓存（不存在则创建，存在且未初始化则更新）
async fn init_nonce(nonce_account: Pubkey) {
    let cache = &*NONCE_CACHE;

    // 检查是否需要初始化：1.缓存中无该账户 2.有账户但hash都是默认值
    let need_init = {
        let current_cache = cache.read().await;
        match current_cache.get(&nonce_account) {
            None => true,
            Some(info) => info.pre_hash == Hash::default() && info.cur_hash == Hash::default(),
        }
    };

    if need_init {
        // 从链上获取Nonce账户数据（保持原解析逻辑：40-72字节是hash字段）
        let account = JSON_RPC_CLIENT
            .get_account(&nonce_account)
            .await
            .expect(&format!("获取Nonce账户[{}]失败", nonce_account));

        let new_hash = match Hash::try_from_slice(&account.data[40..72]) {
            Ok(hash) => hash,
            Err(e) => {
                eprintln!("解析Nonce账户[{}]hash失败: {}", nonce_account, e);
                return;
            }
        };

        // 写入缓存：不存在则创建默认值，存在则更新
        let mut cache_mut = cache.write().await;
        let info = cache_mut.entry(nonce_account).or_default(); // 无则创建默认NonceInfo
        info.pre_hash = info.cur_hash; // 旧当前hash变为前hash
        info.cur_hash = new_hash; // 新hash作为当前hash
    }
}

/// 获取指定Nonce账户的当前hash（自动确保初始化）
pub async fn get_nonce_hash(nonce_account: Pubkey) -> Hash {
    // 确保该账户已初始化（首次调用会触发链上查询，后续直接读缓存）
    init_nonce(nonce_account).await;

    let cache = NONCE_CACHE.read().await;
    // 因init_nonce已确保存在，unwrap安全（或用expect给出更友好错误）
    cache
        .get(&nonce_account)
        .expect(&format!(
            "Nonce账户[{}]未初始化，请检查链上账户是否存在",
            nonce_account
        ))
        .cur_hash
}

/// 更新指定Nonce账户的hash（前hash = 旧当前hash，当前hash = 新传入hash）
pub async fn update_nonce_hash(nonce_account: Pubkey, new_hash: Hash) {
    let mut cache = NONCE_CACHE.write().await;
    // 不存在则创建默认值，避免更新时panic
    let info = cache.entry(nonce_account).or_default();
    (info.pre_hash, info.cur_hash) = (info.cur_hash, new_hash);
}

pub async fn subscribe_nonce_and_transaction(
    nonce_accounts: Vec<Pubkey>,
    payer_pubkeys: Vec<Pubkey>,
) -> Result<(), anyhow::Error> {
    let nonce_accounts = nonce_accounts
        .into_iter()
        .collect::<HashSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let payer_pubkeys = payer_pubkeys
        .into_iter()
        .collect::<HashSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();

    let auto_reconnect = env::var("GRPC_AUTO_RECONNECT")
        .unwrap_or_else(|_| "true".to_string())
        .parse::<bool>()
        .unwrap_or(true);

    let max_retries = env::var("GRPC_MAX_RETRIES")
        .unwrap_or_else(|_| "999999".to_string())
        .parse::<usize>()
        .unwrap_or(999999);

    let health_check_interval = env::var("GRPC_HEALTH_CHECK_INTERVAL_SECS")
        .unwrap_or_else(|_| "60".to_string())
        .parse::<u64>()
        .unwrap_or(60);

    // 启动健康检查任务
    if health_check_interval > 0 {
        tokio::spawn(health_check_task(Duration::from_secs(
            health_check_interval,
        )));
    }

    let mut retry_count = 0;
    let mut last_error_type = String::new();
    let mut consecutive_same_errors = 0;

    loop {
        // 记录连接尝试时间
        let connect_start = Instant::now();

        match subscribe_nonce_and_transaction_inner(nonce_accounts.clone(), payer_pubkeys.clone())
            .await
        {
            Ok(_) => {
                warn!("⚠️  gRPC 订阅正常退出（这不应该发生，可能是流自然结束）");
                CONNECTION_HEALTHY.store(false, Ordering::Relaxed);
                break;
            }
            Err(e) => {
                CONNECTION_HEALTHY.store(false, Ordering::Relaxed);
                let connection_duration = connect_start.elapsed();

                // 分析错误类型
                let error_str = format!("{:?}", e);
                let error_type =
                    if error_str.contains("broken pipe") || error_str.contains("BrokenPipe") {
                        "BROKEN_PIPE"
                    } else if error_str.contains("connection refused")
                        || error_str.contains("ConnectionRefused")
                    {
                        "CONNECTION_REFUSED"
                    } else if error_str.contains("timeout") || error_str.contains("Timeout") {
                        "TIMEOUT"
                    } else if error_str.contains("dns") || error_str.contains("DNS") {
                        "DNS_ERROR"
                    } else if error_str.contains("tls") || error_str.contains("TLS") {
                        "TLS_ERROR"
                    } else {
                        "UNKNOWN"
                    };

                // 检测是否是相同类型的重复错误
                if error_type == last_error_type {
                    consecutive_same_errors += 1;
                } else {
                    consecutive_same_errors = 1;
                    last_error_type = error_type.to_string();
                }

                error!("🔴 gRPC 订阅异常退出");
                error!("   错误类型: {}", error_type);
                error!(
                    "   连接持续时长: {:.2}秒",
                    connection_duration.as_secs_f64()
                );
                error!("   连续相同错误次数: {}", consecutive_same_errors);
                error!("   详细错误: {:?}", e);
                error!(
                    "   统计: 总重连次数={}, 总接收消息={}",
                    TOTAL_RECONNECTS.load(Ordering::Relaxed),
                    TOTAL_MESSAGES.load(Ordering::Relaxed)
                );

                if !auto_reconnect {
                    error!("❌ 自动重连已禁用 (GRPC_AUTO_RECONNECT=false)，程序终止");
                    return Err(e);
                }

                retry_count += 1;
                TOTAL_RECONNECTS.fetch_add(1, Ordering::Relaxed);

                if retry_count > max_retries {
                    error!("❌ 达到最大重试次数 ({}), 程序终止", max_retries);
                    return Err(e);
                }

                // 如果连续相同错误超过5次，使用更长的退避时间
                let base_backoff = std::cmp::min(retry_count * 2, 30);
                let backoff_secs = if consecutive_same_errors > 5 {
                    warn!(
                        "⚠️  检测到连续 {} 次相同错误 [{}]，延长退避时间",
                        consecutive_same_errors, error_type
                    );
                    std::cmp::min(base_backoff * 2, 60)
                } else {
                    base_backoff
                };

                warn!(
                    "⏳ 第 {} 次重连尝试，等待 {} 秒后重新连接... (错误类型: {})",
                    retry_count, backoff_secs, error_type
                );

                tokio::time::sleep(Duration::from_secs(backoff_secs as u64)).await;
                info!("🔄 开始第 {} 次重新连接 gRPC...", retry_count);
            }
        }
    }

    Ok(())
}

/// 健康检查任务：定期检查连接状态和最后接收消息的时间
async fn health_check_task(interval: Duration) {
    loop {
        tokio::time::sleep(interval).await;

        let is_healthy = CONNECTION_HEALTHY.load(Ordering::Relaxed);
        let last_msg_time = LAST_MESSAGE_TIME.load(Ordering::Relaxed);
        let total_reconnects = TOTAL_RECONNECTS.load(Ordering::Relaxed);
        let total_messages = TOTAL_MESSAGES.load(Ordering::Relaxed);

        if last_msg_time == 0 {
            warn!("⚠️  健康检查: 尚未接收到任何消息");
            continue;
        }

        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let seconds_since_last_msg = now_secs.saturating_sub(last_msg_time);

        if is_healthy {
            if seconds_since_last_msg > 300 {
                // 5分钟没收到消息
                error!(
                    "🚨 健康检查告警: 连接状态显示正常，但已 {} 秒未收到消息！",
                    seconds_since_last_msg
                );
                error!("   可能原因: 订阅过滤器不匹配、网络静默、或数据流异常");
            } else if seconds_since_last_msg > 120 {
                // 2分钟没收到消息
                warn!(
                    "⚠️  健康检查提醒: 已 {} 秒未收到消息",
                    seconds_since_last_msg
                );
            } else {
                info!(
                    "✅ 健康检查: 连接正常 | 最后消息: {}秒前 | 总重连: {} | 总消息: {}",
                    seconds_since_last_msg, total_reconnects, total_messages
                );
            }
        } else {
            warn!(
                "🔴 健康检查: 连接断开中 | 最后消息: {}秒前 | 总重连: {}",
                seconds_since_last_msg, total_reconnects
            );
        }
    }
}

async fn subscribe_nonce_and_transaction_inner(
    nonce_accounts: Vec<Pubkey>,
    payer_pubkeys: Vec<Pubkey>,
) -> Result<(), anyhow::Error> {
    info!("🔧 初始化 nonce 账户和 payer 监控...");

    for nonce_account in &nonce_accounts {
        init_nonce(*nonce_account).await;
        info!("   📌 监控 nonce 账户: {}", nonce_account);
    }

    for payer in &payer_pubkeys {
        info!("   💰 监控 payer 账户: {}", payer);
    }

    tokio::spawn(sync_nonce_for_every(
        Duration::from_secs(30),
        nonce_accounts.clone(),
    ));
    set_monitored_payers(&payer_pubkeys[..]).await;

    // 初始化盈亏跟踪数据库并启动跟踪器（静默失败，不影响主流程）
    tokio::spawn({
        let payer_pubkeys = payer_pubkeys.clone();
        async move {
            if pnl_tracker::init_pnl_db(None).await.is_ok() {
                // 启动盈亏跟踪器，监控所有 payer
                tokio::spawn(pnl_tracker::start_pnl_tracker(payer_pubkeys));

                // 可选：启动定期盈亏报告（设置为 0 表示不启动）
                if let Ok(interval) = env::var("PNL_REPORT_INTERVAL_SECS")
                    && let Ok(secs) = interval.parse::<u64>()
                {
                    pnl_tracker::start_periodic_report(secs).await;
                }
            }
        }
    });

    let mut client = setup_client().await?;
    let mut subscribe_accounts = payer_pubkeys
        .iter()
        .map(|p| {
            info!("Starting to monitor account: {}", p);
            p.to_string()
        })
        .collect::<Vec<String>>();
    for nonce_account in nonce_accounts {
        subscribe_accounts.push(nonce_account.to_string());
    }

    let subscribe_request = SubscribeRequest {
        accounts: HashMap::from([(
            "subscribe nonce account".to_string(),
            SubscribeRequestFilterAccounts {
                account: subscribe_accounts.clone(),
                owner: vec![],
                filters: vec![],
                nonempty_txn_signature: None,
            },
        )]),
        transactions: HashMap::from([(
            "transaction subscribe".to_string(),
            SubscribeRequestFilterTransactions {
                account_include: subscribe_accounts,
                ..Default::default()
            },
        )]),
        accounts_data_slice: vec![SubscribeRequestAccountsDataSlice {
            offset: 40,
            length: 32,
        }],
        commitment: Some(CommitmentLevel::Processed.into()),
        ..Default::default()
    };
    info!("🔌 建立 gRPC 订阅流: {}", *ENDPOINT);
    let (mut _subscribe_tx, mut stream) = client
        .subscribe_with_request(Some(subscribe_request))
        .await?;

    info!("✅ gRPC 订阅流已建立，开始接收消息...");
    CONNECTION_HEALTHY.store(true, Ordering::Relaxed);

    let mut message_count = 0u64;
    let start_time = Instant::now();
    let mut last_ping_time = Instant::now();

    while let Some(message) = stream.next().await {
        // 更新最后消息时间
        LAST_MESSAGE_TIME.store(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
            Ordering::Relaxed,
        );

        message_count += 1;
        TOTAL_MESSAGES.fetch_add(1, Ordering::Relaxed);
        match message {
            Ok(msg) => match msg.update_oneof {
                // 监听nonce账户
                Some(UpdateOneof::Account(account)) => {
                    let data = account.account.clone().unwrap().data;
                    // 只处理nonce账户格式的数据，其他账户忽略
                    if let Ok(hash) = Hash::try_from_slice(&data)
                        && let Some(account) = account.account.clone().map(|acc| {
                            Pubkey::new_from_array(acc.pubkey[0..32].try_into().unwrap())
                        })
                    {
                        info!("检测到 Nonce 更新 | 账户: {} | 新 Hash: {}", account, hash);
                        update_nonce_hash(account, hash).await;
                    } else {
                        // 非nonce格式的账户更新，忽略
                        let pubkey_bytes = &account.account.unwrap_or_default().pubkey;
                        if pubkey_bytes.len() < 32 {
                            error!("账户公钥长度不足32字节: {:?}", pubkey_bytes);
                            continue;
                        }
                        let pubkey_array: [u8; 32] =
                            pubkey_bytes[0..32].try_into().unwrap_or_default();
                        info!("忽略非nonce账户更新: {}", Pubkey::from(pubkey_array));
                    }
                }
                // 监听交易
                Some(UpdateOneof::Transaction(tnx)) => {
                    let tx: TransactionFormat = tnx.into();
                    let sig = tx.signature;
                    info!("检测到交易: {}", sig);

                    match &tx.meta {
                        Some(meta) => match &meta.status {
                            Ok(_) => {
                                info!("交易成功: {:?}", sig);
                                update_balances_from_tx(&tx).await;
                                let event = tx_result_channel::TxResultEvent {
                                    signature: sig,
                                    tx: tx.clone(),
                                    status: TradeStatus::Success {
                                        signature: sig,
                                        tx: tx.clone(),
                                    },
                                };
                                let _ = tx_result_channel::send(event);
                            }
                            Err(err) => {
                                info!("交易失败: {:?}, 错误: {:?}", sig, err);
                                let event = tx_result_channel::TxResultEvent {
                                    signature: sig,
                                    tx: tx.clone(),
                                    status: TradeStatus::Failed {
                                        signature: sig,
                                        tx: tx.clone(),
                                        error_msg: format!("{:?}", err),
                                    },
                                };
                                let _ = tx_result_channel::send(event);
                            }
                        },
                        None => {
                            warn!("交易 Meta 缺失: {:?}", sig);
                            let event = tx_result_channel::TxResultEvent {
                                signature: sig,
                                tx: tx.clone(),
                                status: TradeStatus::MetaMissing {
                                    signature: sig,
                                    tx: tx.clone(),
                                },
                            };
                            let _ = tx_result_channel::send(event);
                        }
                    }
                }
                Some(UpdateOneof::Ping(_)) => {
                    let ping_interval = last_ping_time.elapsed();
                    last_ping_time = Instant::now();

                    // 每10个ping记录一次（避免日志过多）
                    if message_count % 10 == 0 {
                        info!(
                            "💓 收到 Ping 心跳 | 间隔: {:.1}s | 本次连接: {:.0}s | 消息数: {}",
                            ping_interval.as_secs_f64(),
                            start_time.elapsed().as_secs_f64(),
                            message_count
                        );
                    }
                }
                _ => {}
            },
            Err(error) => {
                println!("blacklist_monitor error: {:?}", error);
                break;
            }
        }
    }

    Ok(())
}

async fn setup_client() -> Result<GeyserGrpcClient<impl Interceptor>, anyhow::Error> {
    info!("🔌 正在连接 gRPC 端点: {}", &*ENDPOINT);

    let keep_alive_interval = env::var("GRPC_KEEP_ALIVE_INTERVAL_SECS")
        .unwrap_or_else(|_| "30".to_string())
        .parse::<u64>()
        .unwrap_or(30);

    let keep_alive_timeout = env::var("GRPC_KEEP_ALIVE_TIMEOUT_SECS")
        .unwrap_or_else(|_| "10".to_string())
        .parse::<u64>()
        .unwrap_or(10);

    let connect_timeout = env::var("GRPC_CONNECT_TIMEOUT_SECS")
        .unwrap_or_else(|_| "15".to_string())
        .parse::<u64>()
        .unwrap_or(15);

    let request_timeout = env::var("GRPC_REQUEST_TIMEOUT_SECS")
        .unwrap_or_else(|_| "60".to_string())
        .parse::<u64>()
        .unwrap_or(60);

    info!("   Keep-Alive 间隔: {}秒", keep_alive_interval);
    info!("   Keep-Alive 超时: {}秒", keep_alive_timeout);
    info!("   连接超时: {}秒", connect_timeout);
    info!("   请求超时: {}秒", request_timeout);

    // Build the gRPC client with TLS config and HTTP/2 keep-alive
    let client = GeyserGrpcClient::build_from_shared(ENDPOINT.to_string())?
        // .x_token(Some(AUTH_TOKEN.to_string()))?
        .tls_config(ClientTlsConfig::new().with_native_roots())?
        // 配置 HTTP/2 keep-alive 防止 broken pipe
        .http2_keep_alive_interval(Duration::from_secs(keep_alive_interval))
        .keep_alive_timeout(Duration::from_secs(keep_alive_timeout))
        .keep_alive_while_idle(true) // 即使空闲也保持连接
        .connect_timeout(Duration::from_secs(connect_timeout))
        .timeout(Duration::from_secs(request_timeout))
        .connect()
        .await?;

    info!("✅ gRPC 客户端连接成功！");
    Ok(client)
}

/// 获取当前连接健康状态（供外部调用）
pub fn is_connection_healthy() -> bool {
    CONNECTION_HEALTHY.load(Ordering::Relaxed)
}

/// 获取统计信息（供外部调用）
pub fn get_connection_stats() -> (u64, u64, u64) {
    (
        TOTAL_RECONNECTS.load(Ordering::Relaxed),
        TOTAL_MESSAGES.load(Ordering::Relaxed),
        LAST_MESSAGE_TIME.load(Ordering::Relaxed),
    )
}

async fn sync_nonce_for_every(time: Duration, nonce_accounts: Vec<Pubkey>) {
    loop {
        for account in &nonce_accounts {
            // 从链上获取Nonce账户数据（保持原解析逻辑：40-72字节是hash字段）
            let Ok(account_info) = JSON_RPC_CLIENT.get_account(account).await else {
                eprintln!("获取Nonce账户[{}]失败", account);
                continue;
            };

            let new_hash = match Hash::try_from_slice(&account_info.data[40..72]) {
                Ok(hash) => hash,
                Err(e) => {
                    eprintln!("解析Nonce账户[{}]hash失败: {}", account, e);
                    continue;
                }
            };

            let _ = flash_nonce(account, new_hash).await;
        }

        tokio::time::sleep(time).await;
    }
}

async fn flash_nonce(account: &Pubkey, fetched_hash: Hash) {
    // 1. 获取当前缓存中的值进行初步比对
    let current_cached_hash = {
        let cache = NONCE_CACHE.read().await;
        cache
            .get(account)
            .map(|info| info.cur_hash)
            .unwrap_or_default()
    };

    // 如果 fetch 到的值和缓存一致，直接跳过
    if fetched_hash == current_cached_hash {
        return;
    }

    // 2. 如果不匹配，触发“再次确认” (Second Fetch)
    // 稍微延迟一小会儿，避开瞬间的网络抖动
    tokio::time::sleep(Duration::from_millis(1000)).await;

    match JSON_RPC_CLIENT.get_account(account).await {
        Ok(confirm_account) => {
            if let Ok(confirm_hash) = Hash::try_from_slice(&confirm_account.data[40..72]) {
                // 3. 核心逻辑：两次 Fetch 的值必须相同，且依然与缓存不同
                if confirm_hash == fetched_hash {
                    warn!(
                        "🚨 Nonce 不一致纠正 | 账户: {} | 缓存旧值: {} | 链上新值: {}",
                        account, current_cached_hash, confirm_hash
                    );

                    // 执行更新逻辑
                    update_nonce_hash(*account, confirm_hash).await;
                } else {
                    // 如果两次 fetch 都不一样，说明该账户非常活跃，或者 RPC 节点数据极度不稳定
                    error!(
                        "⚠️ Nonce 验证失败 (数据抖动) | 账户: {} | 第一次: {} | 第二次: {}",
                        account, fetched_hash, confirm_hash
                    );
                }
            }
        }
        Err(e) => error!("二次确认 Nonce 失败 [{}]: {}", account, e),
    }
}
