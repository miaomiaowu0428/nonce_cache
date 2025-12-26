use {
    crate::tx_result_channel::TxResultEvent,
    anyhow,
    borsh::BorshDeserialize,
    futures::stream::StreamExt,
    grpc_client::TransactionFormat,
    log::{error, info},
    solana_client::nonblocking::rpc_client::RpcClient,
    solana_commitment_config::CommitmentConfig,
    solana_sdk::{hash::Hash, pubkey::Pubkey, signature::Signature},
    std::{
        collections::{HashMap, HashSet},
        env,
        sync::{Arc, LazyLock},
    },
    tokio::{self, sync::RwLock},
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TradeStatus {
    Success(Signature),
    Failed(String, String), // (tx, meta)
}

impl TradeStatus {
    pub fn success(&self) -> bool {
        matches!(self, TradeStatus::Success(_))
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
    payer_pubkey: Pubkey,
) -> Result<(), anyhow::Error> {
    for nonce_account in &nonce_accounts {
        init_nonce(*nonce_account).await;
        info!("Starting to monitor account: {}", nonce_account);
    }

    info!("Starting to monitor payer: {}", payer_pubkey);

    let mut client = setup_client().await?;
    info!("Connected to gRPC endpoint");
    let mut subscribe_accounts = vec![payer_pubkey.to_string()];
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
    let (mut _subscribe_tx, mut stream) = client
        .subscribe_with_request(Some(subscribe_request))
        .await?;

    info!("start to monitor self: {payer_pubkey} tx");

    while let Some(message) = stream.next().await {
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
                        update_nonce_hash(account.into(), hash).await;
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
                    info!("检测到交易: {}", sig); // 👈 显示所有检测到的交易
                    let Some(meta) = &tx.meta else {
                        let event = tx_result_channel::TxResultEvent {
                            signature: sig,
                            tx,
                            status: TradeStatus::Failed(
                                "tx failed".to_string(),
                                "meta not found".to_string(),
                            ),
                        };
                        let _ = tx_result_channel::send(event);
                        continue;
                    };
                    match &meta.status {
                        Ok(_) => {
                            info!("交易成功: {:?}", sig);
                            let event = tx_result_channel::TxResultEvent {
                                signature: sig,
                                tx,
                                status: TradeStatus::Success(sig.clone()),
                            };
                            let _ = tx_result_channel::send(event);
                        }
                        Err(err) => {
                            info!("交易失败: {:?}, 错误: {:?}", sig, err);
                            let tx_str = format!("{:?}", tx);
                            let meta_str = format!("{:?}", meta);
                            let event = tx_result_channel::TxResultEvent {
                                signature: sig,
                                tx,
                                status: TradeStatus::Failed(tx_str, meta_str),
                            };
                            let _ = tx_result_channel::send(event);
                        }
                    }
                }
                Some(UpdateOneof::Ping(_)) => {
                    // info!("ping ...");
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
    info!("Connecting to gRPC endpoint: {}", &*ENDPOINT);

    // Build the gRPC client with TLS config
    let client = GeyserGrpcClient::build_from_shared(ENDPOINT.to_string())?
        // .x_token(Some(AUTH_TOKEN.to_string()))?
        .tls_config(ClientTlsConfig::new().with_native_roots())?
        .connect()
        .await?;

    Ok(client)
}

/// 监听交易结果的通用函数
///
/// # 参数
/// - `tx_result_rx`: 已订阅的交易结果接收端
/// - `expected_signatures`: 期望的交易签名集合
/// - `timeout_secs`: 超时时间（秒）
///
/// # 返回
/// - `Ok(Signature)`: 成功获取到交易签名
/// - `Err(...)`: 超时或其他错误
pub async fn confirm_tx(
    mut tx_result_rx: tokio::sync::broadcast::Receiver<TxResultEvent>,
    expected_signatures: HashSet<Signature>,
    timeout_secs: u64,
) -> Result<(Signature, TransactionFormat), Box<dyn std::error::Error + Sync + Send>> {
    info!("confirming: {expected_signatures:#?}");
    let res = tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), async {
        loop {
            if let Ok(TxResultEvent {
                signature,
                tx,
                status,
            }) = tx_result_rx.recv().await
            {
                if expected_signatures.contains(&signature) {
                    info!("交易确认: {:?} -> {:#?}", signature, status);
                    match status {
                        TradeStatus::Success(_) => return Ok((signature, tx)),
                        TradeStatus::Failed(_, _) => {
                            error!("交易失败: {:?}", signature);
                            return Err("交易失败".into());
                        }
                    }
                } else {
                    // 广播模式下，直接忽略不属于我们的交易结果
                    info!("非本组交易, 忽略: {:?}", signature);
                }
            }
        }
    })
    .await;

    match res {
        Ok(Ok((sig, tx))) => Ok((sig, tx)),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(format!("交易监听超时").into()),
    }
}

pub async fn confirm_success_tx(
    mut tx_result_rx: tokio::sync::broadcast::Receiver<TxResultEvent>,
    expected_signatures: HashSet<Signature>,
    timeout_secs: u64,
) -> Result<(Signature, TransactionFormat), Box<dyn std::error::Error + Sync + Send>> {
    let res = tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), async {
        loop {
            if let Ok(TxResultEvent {
                signature: sig,
                tx,
                status,
            }) = tx_result_rx.recv().await
            {
                if expected_signatures.contains(&sig) {
                    info!("交易确认: {:?} -> {:#?}", sig, status);
                    match status {
                        TradeStatus::Success(_) => return Ok((sig, tx)),
                        TradeStatus::Failed(_, _) => {
                            // 只记录失败，但继续等待其他交易的成功
                            error!("交易失败: {:?}，继续等待其他交易", sig);
                            continue;
                        }
                    }
                } else {
                    // 广播模式下，直接忽略不属于我们的交易结果
                    info!("非本组交易, 忽略: {:?}", sig);
                }
            }
        }
    })
    .await;

    match res {
        Ok(Ok((sig, tx))) => Ok((sig, tx)),
        Ok(Err(e)) => {
            error!("this should not happen, a loop should not return Err");
            Err(e)
        }
        Err(_) => Err(format!("所有交易都失败或超时").into()),
    }
}
