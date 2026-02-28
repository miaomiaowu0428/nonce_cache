use std::{collections::HashSet, str::FromStr, sync::LazyLock};

use grpc_client::TransactionFormat;
use log::info;
use solana_sdk::pubkey::Pubkey;
use tokio::sync::RwLock;

// Key 为 (Owner, Mint)
pub static SELF_TOKEN_BALANCE: LazyLock<whirlwind::ShardMap<(Pubkey, Pubkey), u64>> =
    LazyLock::new(|| whirlwind::ShardMap::with_shards(16));

pub async fn self_balance_of(owner: &Pubkey, mint: &Pubkey) -> Option<u64> {
    SELF_TOKEN_BALANCE
        .get(&(*owner, *mint))
        .await
        .as_deref()
        .copied()
}

pub static MONITORED_PAYERS: LazyLock<RwLock<HashSet<Pubkey>>> =
    LazyLock::new(|| RwLock::new(HashSet::new()));

// 在 subscribe_nonce_and_transaction 开始时调用
pub async fn set_monitored_payers(payers: &[Pubkey]) {
    let mut lock = MONITORED_PAYERS.write().await;
    for p in payers {
        lock.insert(*p);
    }
}

pub async fn update_balances_from_tx(tx: &TransactionFormat) {
    let Some(meta) = &tx.meta else { return };

    // 获取当前监控的名单
    let monitored = MONITORED_PAYERS.read().await;

    // 收集 post_token_balances 中出现的 (owner, mint) 组合
    let mut post_keys = HashSet::new();

    // 更新 post_token_balances 中的余额
    if let Some(post_balances) = &meta.post_token_balances {
        for tb in post_balances {
            if let Ok(owner_pk) = Pubkey::from_str(&tb.owner) {
                if monitored.contains(&owner_pk) {
                    if let Ok(mint_pk) = Pubkey::from_str(&tb.mint) {
                        let amount = tb.ui_token_amount.amount.parse::<u64>().unwrap_or(0);

                        info!(
                            "[余额更新] 账户: {}, Mint: {}, 新余额: {}",
                            owner_pk, mint_pk, amount
                        );

                        post_keys.insert((owner_pk, mint_pk));
                        SELF_TOKEN_BALANCE.insert((owner_pk, mint_pk), amount).await;
                    }
                }
            }
        }
    }

    // 检查 pre_token_balances 中有但 post 中消失的账户（清仓归零）
    if let Some(pre_balances) = &meta.pre_token_balances {
        for tb in pre_balances {
            if let Ok(owner_pk) = Pubkey::from_str(&tb.owner) {
                if monitored.contains(&owner_pk) {
                    if let Ok(mint_pk) = Pubkey::from_str(&tb.mint) {
                        let key = (owner_pk, mint_pk);

                        // 如果这个账户在 pre 中有余额，但 post 中没出现，说明清仓归零了
                        if !post_keys.contains(&key) {
                            info!("[余额归零] 账户: {}, Mint: {}, 清仓完成", owner_pk, mint_pk);
                            SELF_TOKEN_BALANCE.insert(key, 0).await;
                        }
                    }
                }
            }
        }
    }
}
