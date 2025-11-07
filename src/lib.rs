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

#[derive(Debug, Default)]
struct NonceInfo {
    pub pre_hash: Hash,
    pub cur_hash: Hash,
}

static NONCE_ACCOUNT_KEY: LazyLock<RwLock<Pubkey>> =
    LazyLock::new(|| RwLock::new(Pubkey::default()));

async fn init_nonce_account_key(nonce_account: Pubkey) {
    let mut key = NONCE_ACCOUNT_KEY.write().await;
    if *key != Pubkey::default() {
        return;
    }
    *key = nonce_account;
}

/// 缓存Nonce hash
static NONCE_CACHE: LazyLock<RwLock<NonceInfo>> =
    LazyLock::new(|| RwLock::new(NonceInfo::default()));

async fn init_nonce_cache() {
    let cache = &*NONCE_CACHE;

    if {
        let current = cache.read().await;
        current.pre_hash == Hash::default() && current.cur_hash == Hash::default()
    } {
        let account = JSON_RPC_CLIENT
            .get_account(&*NONCE_ACCOUNT_KEY.read().await)
            .await
            .expect("获取 nonce 账户失败");
        let data = account.data;
        let Ok(hash) = Hash::try_from_slice(&data[40..72]) else {
            return;
        };

        let mut cache_mut = cache.write().await;
        cache_mut.pre_hash = cache_mut.cur_hash;
        cache_mut.cur_hash = hash;
    }
}

pub async fn get_nonce_hash() -> Hash {
    let _cache = init_nonce_cache().await; // 保证初始化一次
    let read = NONCE_CACHE.read().await;
    read.cur_hash
}

pub async fn update_nonce_hash(hash: Hash) {
    let mut write = NONCE_CACHE.write().await;
    (write.pre_hash, write.cur_hash) = (write.cur_hash, hash);
}

pub async fn subscribe_nonce_and_transaction(
    nonce_account: Pubkey,
    payer_pubkey: Pubkey,
) -> Result<(), anyhow::Error> {
    init_nonce_account_key(nonce_account).await;
    let _ = init_nonce_cache().await;

    info!("Starting to monitor account: {}", nonce_account);
    info!("Starting to monitor payer: {}", payer_pubkey);

    let mut client = setup_client().await?;
    info!("Connected to gRPC endpoint");

    let subscribe_request = SubscribeRequest {
        accounts: HashMap::from([(
            "subscribe nonce account".to_string(),
            SubscribeRequestFilterAccounts {
                account: vec![
                    // account.clone(),
                    payer_pubkey.to_string(),
                    nonce_account.to_string(),
                ],
                owner: vec![],
                filters: vec![],
                nonempty_txn_signature: None,
            },
        )]),
        transactions: HashMap::from([(
            "transaction subscribe".to_string(),
            SubscribeRequestFilterTransactions {
                account_include: vec![
                    // account,
                    payer_pubkey.to_string(),
                    nonce_account.to_string(),
                ],
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

    while let Some(message) = stream.next().await {
        match message {
            Ok(msg) => match msg.update_oneof {
                // 监听nonce账户
                Some(UpdateOneof::Account(account)) => {
                    let data = account.account.clone().unwrap().data;
                    // 只处理nonce账户格式的数据，其他账户忽略
                    if let Ok(hash) = Hash::try_from_slice(&data) {
                        update_nonce_hash(hash).await;
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
                    info!("检测到交易: {:?}", sig); // 👈 显示所有检测到的交易
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
