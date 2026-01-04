use std::{collections::HashSet, str::FromStr, sync::LazyLock};

use grpc_client::TransactionFormat;
use log::info;
use solana_sdk::pubkey::Pubkey;
use tokio::sync::RwLock;

// Key 为 (Owner, Mint)
pub static SELF_TOKEN_BALANCE: LazyLock<whirlwind::ShardMap<(Pubkey, Pubkey), u64>> =
    LazyLock::new(|| whirlwind::ShardMap::with_shards(16));

pub async fn self_balance_of(owner: &Pubkey, mint: &Pubkey) -> u64 {
    SELF_TOKEN_BALANCE
        .get(&(*owner, *mint))
        .await
        .map(|res| *res)
        .unwrap_or(0)
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
    let Some(post_balances) = &meta.post_token_balances else {
        return;
    };

    // 获取当前监控的名单
    let monitored = MONITORED_PAYERS.read().await;

    for tb in post_balances {
        if let Ok(owner_pk) = Pubkey::from_str(&tb.owner) {
            // 如果该 Owner 在我们的监控名单中
            if monitored.contains(&owner_pk) {
                if let Ok(mint_pk) = Pubkey::from_str(&tb.mint) {
                    let amount = tb.ui_token_amount.amount.parse::<u64>().unwrap_or(0);

                    info!(
                        "[余额更新] 账户: {}, Mint: {}, 新余额: {}",
                        owner_pk, mint_pk, amount
                    );

                    // 更新复合 Key 存储
                    SELF_TOKEN_BALANCE.insert((owner_pk, mint_pk), amount).await;
                }
            }
        }
    }
}
