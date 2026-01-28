use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::LazyLock;

use log::info;
use serde::{Deserialize, Serialize};
use solana_sdk::pubkey::Pubkey;
use tokio::sync::RwLock;

use crate::tx_result_channel;

// ============ 本位币配置 ============
// 按优先级排序：优先匹配靠前的币种作为本位币
const WSOL: &str = "So11111111111111111111111111111111111111112";
const USD1: &str = "USD1ttGY1N17NEEHLmELoaybftRBUSErhqYiQzvEmuB";
const USDC: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
const USDT: &str = "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB";

static QUOTE_CURRENCIES: LazyLock<Vec<Pubkey>> = LazyLock::new(|| {
    vec![
        USD1.parse().expect("USD1 address invalid"), // 优先级 1: USD1
        USDC.parse().expect("USDC address invalid"), // 优先级 2: USDC
        USDT.parse().expect("USDT address invalid"), // 优先级 3: USDT
        WSOL.parse().expect("WSOL address invalid"), // 优先级 4: WSOL
        Pubkey::default(),                           // 优先级 5: SOL (native)
    ]
});

/// 获取本位币的显示名称
fn quote_name(mint: &Pubkey) -> &'static str {
    let mint_str = mint.to_string();
    if mint_str == USD1 {
        "USD1"
    } else if mint_str == USDC {
        "USDC"
    } else if mint_str == USDT {
        "USDT"
    } else if mint_str == WSOL {
        "WSOL"
    } else if *mint == Pubkey::default() {
        "SOL"
    } else {
        "UNKNOWN"
    }
}

/// 获取本位币的小数位数（decimals）
fn quote_decimals(mint: &Pubkey) -> u32 {
    let mint_str = mint.to_string();
    if mint_str == USD1 || mint_str == USDC || mint_str == USDT {
        6 // 稳定币通常是 6 位小数
    } else {
        9 // SOL/WSOL 是 9 位小数
    }
}

/// 将 lamports/最小单位转换为可读的小数格式
pub fn to_ui_amount(amount: i128, mint: &Pubkey) -> f64 {
    let decimals = quote_decimals(mint);
    let divisor = 10_i128.pow(decimals);
    amount as f64 / divisor as f64
}

// ============ 数据结构 ============

/// 代币盈亏统计
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TokenPnL {
    pub payer: String,           // 交易账户地址
    pub quote_pnl: i128,         // 本位币盈亏
    pub sol_gas_cost: i128,      // SOL gas 总成本（稳定币本位时单独统计）
    pub quote_mint: String,      // 使用的本位币类型（存储为字符串方便序列化）
    pub success_tx_count: usize, // 成功交易次数
}

impl TokenPnL {
    /// 添加一笔成功交易的盈亏
    pub fn add_success_trade(
        &mut self,
        quote_change: i128,
        sol_gas: i128,
        quote_mint: Pubkey,
        payer: Pubkey,
    ) {
        self.quote_pnl += quote_change;
        self.sol_gas_cost += sol_gas;
        self.success_tx_count += 1;

        // 首次记录时设置 quote_mint 和 payer
        if self.quote_mint.is_empty() {
            self.quote_mint = quote_mint.to_string();
        }
        if self.payer.is_empty() {
            self.payer = payer.to_string();
        }
    }

    /// 获取本位币 Pubkey
    pub fn get_quote_mint(&self) -> Option<Pubkey> {
        self.quote_mint.parse().ok()
    }

    /// 是否为 SOL 本位
    pub fn is_sol_based(&self) -> bool {
        self.quote_mint == Pubkey::default().to_string() || self.quote_mint == WSOL
    }
}

/// 汇总统计
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PnLSummary {
    pub total_tokens: usize,                   // 交易过的代币总数
    pub win_count: usize,                      // 盈利币种数
    pub loss_count: usize,                     // 亏损币种数
    pub total_by_quote: HashMap<String, i128>, // 按本位币汇总的盈亏
}

// ============ 持久化存储 ============

static DB: LazyLock<RwLock<Option<sled::Db>>> = LazyLock::new(|| RwLock::new(None));

/// 初始化 sled 数据库
pub async fn init_pnl_db(db_path: Option<PathBuf>) -> Result<(), anyhow::Error> {
    let path = db_path.unwrap_or_else(|| PathBuf::from("./data/pnl_tracker"));
    let db = sled::open(path)?;

    let mut db_lock = DB.write().await;
    *db_lock = Some(db);

    // 数据库初始化完成（静默）
    Ok(())
}

/// 获取数据库实例
async fn get_db() -> Option<sled::Db> {
    DB.read().await.clone()
}

/// 保存单个代币的盈亏数据
async fn save_token_pnl(
    payer: &Pubkey,
    mint: &Pubkey,
    pnl: &TokenPnL,
) -> Result<(), anyhow::Error> {
    let Some(db) = get_db().await else {
        return Err(anyhow::anyhow!("数据库未初始化"));
    };

    let key = format!("{}:{}", payer, mint);
    let value = bincode::serialize(pnl)?;
    db.insert(key.as_bytes(), value)?;
    // 使用 flush 而非 flush_async 避免潜在的阻塞问题
    let _ = db.flush();

    Ok(())
}

/// 加载单个代币的盈亏数据（按账户）
pub async fn load_token_pnl(payer: &Pubkey, mint: &Pubkey) -> Option<TokenPnL> {
    let db = get_db().await?;
    let key = format!("{}:{}", payer, mint);

    match db.get(key.as_bytes()) {
        Ok(Some(data)) => bincode::deserialize(&data).ok(),
        _ => None,
    }
}

/// 加载所有代币的盈亏数据
pub async fn load_all_pnl() -> HashMap<(Pubkey, Pubkey), TokenPnL> {
    let mut result = HashMap::new();
    let Some(db) = get_db().await else {
        return result;
    };

    for item in db.iter() {
        if let Ok((key, value)) = item {
            if let Ok(key_str) = std::str::from_utf8(&key) {
                // 解析 "payer:mint" 格式
                if let Some((payer_str, mint_str)) = key_str.split_once(':') {
                    if let (Ok(payer), Ok(mint)) =
                        (payer_str.parse::<Pubkey>(), mint_str.parse::<Pubkey>())
                    {
                        if let Ok(pnl) = bincode::deserialize::<TokenPnL>(&value) {
                            result.insert((payer, mint), pnl);
                        }
                    }
                }
            }
        }
    }

    result
}

/// 清空所有盈亏数据（慎用）
pub async fn clear_all_pnl() -> Result<(), anyhow::Error> {
    let Some(db) = get_db().await else {
        return Err(anyhow::anyhow!("数据库未初始化"));
    };

    db.clear()?;
    db.flush_async().await?;
    // 已清空所有盈亏数据（静默）

    Ok(())
}

// ============ 实时统计 ============

/// 内存缓存，用于快速访问（避免频繁读写 sled）
static MEMORY_CACHE: LazyLock<RwLock<HashMap<(Pubkey, Pubkey), TokenPnL>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

/// 监控的地址列表
static MONITORED_TARGETS: LazyLock<RwLock<Vec<Pubkey>>> = LazyLock::new(|| RwLock::new(Vec::new()));

/// 从 TransactionFormat 提取余额变化
/// 参考 utils::parse_rpc_fetched_json 的实现，但直接使用 TransactionFormat 的字段
fn extract_balance_changes(
    tx: &grpc_client::TransactionFormat,
) -> Result<Vec<BalanceChange>, anyhow::Error> {
    use std::collections::HashSet;

    let Some(meta) = &tx.meta else {
        return Err(anyhow::anyhow!("meta not found"));
    };

    let account_keys = &tx.account_keys;

    // ===============================
    // 1 SOL balance diff
    // ===============================
    let mut sol_changes = Vec::new();
    for (i, owner) in account_keys.iter().enumerate() {
        let pre = *meta.pre_balances.get(i).unwrap_or(&0);
        let post = *meta.post_balances.get(i).unwrap_or(&0);

        if pre != post {
            sol_changes.push(BalanceChange {
                owner: *owner,
                mint: Pubkey::default(),
                pre_balance: pre,
                after_balance: post,
                change: post as i128 - pre as i128,
                decimal: 9,
            });
        }
    }

    // ===============================
    // 2 Token balance diff
    // ===============================
    let mut token_changes = Vec::new();
    if let (Some(pre_tokens), Some(post_tokens)) =
        (&meta.pre_token_balances, &meta.post_token_balances)
    {
        let mut all_keys = HashSet::new();
        let mut pre_map: HashMap<(Pubkey, Pubkey), u64> = HashMap::new();
        let mut post_map: HashMap<(Pubkey, Pubkey), u64> = HashMap::new();
        let mut decimals_map: HashMap<(Pubkey, Pubkey), u8> = HashMap::new();

        for tb in pre_tokens {
            let owner = tb.owner.parse::<Pubkey>()?;
            let mint = tb.mint.parse::<Pubkey>()?;
            let amount = tb.ui_token_amount.amount.parse::<u64>().unwrap_or(0);
            pre_map.insert((owner, mint), amount);
            decimals_map.insert((owner, mint), tb.ui_token_amount.decimals);
            all_keys.insert((owner, mint));
        }

        for tb in post_tokens {
            let owner = tb.owner.parse::<Pubkey>()?;
            let mint = tb.mint.parse::<Pubkey>()?;
            let amount = tb.ui_token_amount.amount.parse::<u64>().unwrap_or(0);
            post_map.insert((owner, mint), amount);
            decimals_map.insert((owner, mint), tb.ui_token_amount.decimals);
            all_keys.insert((owner, mint));
        }

        for key in all_keys {
            let pre = *pre_map.get(&key).unwrap_or(&0);
            let post = *post_map.get(&key).unwrap_or(&0);
            let decimal = *decimals_map.get(&key).unwrap_or(&0);

            if pre != post {
                token_changes.push(BalanceChange {
                    owner: key.0,
                    mint: key.1,
                    pre_balance: pre,
                    after_balance: post,
                    change: post as i128 - pre as i128,
                    decimal,
                });
            }
        }
    }

    // ===============================
    // 3 合并结果
    // ===============================
    let mut changes = sol_changes;
    changes.extend(token_changes);

    Ok(changes)
}

/// BalanceChange 辅助结构，与 utils 中的定义兼容
#[derive(Debug, Clone, Default)]
pub struct BalanceChange {
    pub owner: Pubkey,
    pub mint: Pubkey,
    pub pre_balance: u64,
    pub after_balance: u64,
    pub change: i128,
    pub decimal: u8,
}

impl BalanceChange {
    pub fn combine(&self, other: &BalanceChange) -> Option<BalanceChange> {
        // 安全检查：确保是同一个 owner 和相同的 decimal
        if self.owner != other.owner || self.decimal != other.decimal {
            return None;
        }
        Some(BalanceChange {
            owner: self.owner,
            mint: self.mint,
            pre_balance: self.pre_balance + other.pre_balance,
            after_balance: self.after_balance + other.after_balance,
            change: self.change + other.change,
            decimal: self.decimal,
        })
    }
}

/// 为 TransactionFormat 实现 GetAccounts trait
pub trait GetAccounts {
    fn accounts(&self) -> Vec<Pubkey>;
}

impl GetAccounts for grpc_client::TransactionFormat {
    fn accounts(&self) -> Vec<Pubkey> {
        self.account_keys.clone()
    }
}

/// 处理成功的交易，计算并记录盈亏
async fn process_success_transaction(
    tx: &grpc_client::TransactionFormat,
    target: Pubkey,
) -> Result<(), anyhow::Error> {
    use log::info;

    // 获取余额变化
    let balance_changes = extract_balance_changes(tx)?;
    let self_balance_changes: Vec<BalanceChange> = balance_changes
        .into_iter()
        .filter(|change| change.owner == target)
        .collect();

    // 按优先级确定本位币（quote）
    let wsol_mint: Pubkey = match WSOL.parse() {
        Ok(addr) => addr,
        Err(_) => return Ok(()), // WSOL 地址解析失败，跳过此交易
    };
    let (quote_mint, quote_change) = QUOTE_CURRENCIES
        .iter()
        .find_map(|&currency| {
            // 对于 SOL，需要合并 native SOL 和 WSOL
            if currency == Pubkey::default() || currency == wsol_mint {
                let sol_change = self_balance_changes
                    .iter()
                    .find(|c| c.mint == Pubkey::default());
                let wsol_change = self_balance_changes.iter().find(|c| c.mint == wsol_mint);

                match (sol_change, wsol_change) {
                    (Some(sol), Some(wsol)) => sol
                        .combine(wsol)
                        .map(|combined| (Pubkey::default(), combined)),
                    (Some(sol), None) => Some((Pubkey::default(), sol.clone())),
                    (None, Some(wsol)) => Some((Pubkey::default(), wsol.clone())),
                    (None, None) => None,
                }
            } else {
                // 其他本位币直接查找
                self_balance_changes
                    .iter()
                    .find(|c| c.mint == currency)
                    .map(|change| (currency, change.clone()))
            }
        })
        .unwrap_or((Pubkey::default(), BalanceChange::default()));

    // 找到标的币（base）：排除所有本位币的其他币种
    let Some(base_mint) = self_balance_changes.iter().find_map(|c| {
        if !QUOTE_CURRENCIES.contains(&c.mint) {
            Some(c.mint)
        } else {
            None
        }
    }) else {
        // 没有找到标的币，可能是纯转账或其他操作，跳过
        info!("💰 [PnL] 跳过交易 {} (无标的币)", tx.signature);
        return Ok(());
    };

    info!(
        "💰 [PnL] 处理交易 {} | 标的: {} | 本位: {}",
        tx.signature,
        base_mint,
        quote_name(&quote_mint)
    );

    // 判断是否是 SOL/WSOL 本位
    let is_sol_based = quote_mint == Pubkey::default() || quote_mint == wsol_mint;

    // 计算 gas 成本（仅稳定币本位时需要单独统计）
    let sol_gas = if !is_sol_based {
        // 稳定币本位：统计 SOL 的消耗作为 gas
        let sol_change = self_balance_changes
            .iter()
            .find(|c| c.mint == Pubkey::default());
        sol_change.map(|c| c.change).unwrap_or(0)
    } else {
        0 // SOL 本位：gas 已包含在 quote_change 中
    };

    // 更新内存缓存
    {
        let mut cache = MEMORY_CACHE.write().await;
        let token_stat = cache.entry((target, base_mint)).or_insert_with(|| {
            // 尝试从数据库加载
            tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(async {
                    load_token_pnl(&target, &base_mint)
                        .await
                        .unwrap_or_default()
                })
            })
        });

        token_stat.add_success_trade(quote_change.change, sol_gas, quote_mint, target);

        let quote_mint_pubkey = token_stat.get_quote_mint().unwrap_or_default();
        info!(
            "💰 [PnL] 更新统计 | Payer: {} | Token: {} | 本位盈亏: {:.4} {} | 成功交易数: {}",
            target,
            base_mint,
            to_ui_amount(token_stat.quote_pnl, &quote_mint_pubkey),
            quote_name(&quote_mint_pubkey),
            token_stat.success_tx_count
        );
    }

    // 异步保存到数据库
    let cache = MEMORY_CACHE.read().await;
    if let Some(pnl) = cache.get(&(target, base_mint)) {
        // 静默保存，错误不影响后续处理
        let _ = save_token_pnl(&target, &base_mint, pnl).await;
    }

    Ok(())
}

/// 启动盈亏跟踪器，监听交易事件
/// 支持监控多个地址，所有地址的盈亏数据会合并统计
/// 所有操作都在独立的 task 中执行，不会因 panic 影响主流程
pub async fn start_pnl_tracker(targets: Vec<Pubkey>) {
    if targets.is_empty() {
        return;
    }

    // 在独立的 task 中执行所有初始化和监听操作
    tokio::spawn(async move {
        // 保存监控地址列表
        if let Ok(mut monitored) = MONITORED_TARGETS.try_write() {
            *monitored = targets.clone();
        } else {
            return; // 无法获取锁，放弃启动
        }

        // 从数据库加载历史数据到内存缓存（静默失败）
        if let Ok(historical_data) = tokio::task::spawn_blocking(|| {
            tokio::runtime::Handle::current().block_on(load_all_pnl())
        })
        .await
        {
            if !historical_data.is_empty() {
                if let Ok(mut cache) = MEMORY_CACHE.try_write() {
                    *cache = historical_data;
                }
            }
        }

        // 订阅交易结果事件
        let mut rx = tx_result_channel::subscribe();

        while let Ok(event) = rx.recv().await {
            // 只处理成功的交易
            if event.status.success() {
                // 从交易的账户列表中找出被监控的地址
                let tx_accounts = &event.tx.account_keys;

                if let Ok(monitored) = MONITORED_TARGETS.try_read() {
                    // 找到第一个匹配的监控地址
                    if let Some(target) =
                        monitored.iter().find(|&&addr| tx_accounts.contains(&addr))
                    {
                        // 静默处理错误，不影响后续交易
                        if let Err(e) = process_success_transaction(&event.tx, *target).await {
                            log::warn!("💰 [PnL] 处理交易失败 {}: {:?}", event.tx.signature, e);
                        }
                    }
                }
            }
        }
    });
}

/// 启动定期打印盈亏报告的任务
///
/// # 参数
/// - `interval_secs`: 打印报告的间隔时间（秒）
pub async fn start_periodic_report(interval_secs: u64) {
    if interval_secs == 0 {
        return;
    }

    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(interval_secs));
        interval.tick().await; // 跳过第一次立即触发

        loop {
            interval.tick().await;
            // 静默打印报告，错误不影响循环
            let _ = tokio::task::spawn_blocking(|| {
                tokio::runtime::Handle::current().block_on(print_pnl_report())
            })
            .await;
        }
    });
}

// ============ 查询接口 ============

/// 查询单个代币的盈亏（按账户）
pub async fn query_token_pnl(payer: &Pubkey, mint: &Pubkey) -> Option<TokenPnL> {
    // 优先从内存缓存读取
    let cache = MEMORY_CACHE.read().await;
    if let Some(pnl) = cache.get(&(*payer, *mint)) {
        return Some(pnl.clone());
    }

    // 缓存没有，从数据库加载
    load_token_pnl(payer, mint).await
}

/// 查询某个账户的所有代币盈亏
pub async fn query_payer_pnl(payer: &Pubkey) -> HashMap<Pubkey, TokenPnL> {
    let cache = MEMORY_CACHE.read().await;
    cache
        .iter()
        .filter_map(|((p, mint), pnl)| {
            if p == payer {
                Some((*mint, pnl.clone()))
            } else {
                None
            }
        })
        .collect()
}

/// 查询所有代币的盈亏（所有账户）
pub async fn query_all_pnl() -> HashMap<(Pubkey, Pubkey), TokenPnL> {
    MEMORY_CACHE.read().await.clone()
}

/// 查询盈亏汇总统计
pub async fn query_pnl_summary() -> PnLSummary {
    let cache = MEMORY_CACHE.read().await;

    let mut summary = PnLSummary {
        total_tokens: cache.len(),
        ..Default::default()
    };

    for (_, pnl) in cache.iter() {
        // 统计盈亏币种
        if pnl.quote_pnl > 0 {
            summary.win_count += 1;
        } else if pnl.quote_pnl < 0 {
            summary.loss_count += 1;
        }

        // 按本位币汇总
        *summary
            .total_by_quote
            .entry(pnl.quote_mint.clone())
            .or_insert(0) += pnl.quote_pnl;
    }

    summary
}

/// 查询按盈亏排序的代币列表
pub async fn query_sorted_pnl(ascending: bool) -> Vec<((Pubkey, Pubkey), TokenPnL)> {
    let cache = MEMORY_CACHE.read().await;
    let mut sorted: Vec<_> = cache.iter().map(|(k, v)| (*k, v.clone())).collect();

    if ascending {
        sorted.sort_by(|a, b| a.1.quote_pnl.cmp(&b.1.quote_pnl));
    } else {
        sorted.sort_by(|a, b| b.1.quote_pnl.cmp(&a.1.quote_pnl));
    }

    sorted
}

/// 打印盈亏报告（类似 self_pl.rs 的输出格式）
pub async fn print_pnl_report() {
    let sorted_pnl = query_sorted_pnl(false).await; // 从高到低
    let summary = query_pnl_summary().await;

    info!("\n========== 📊 实时盈亏报告 ==========\n");

    for ((payer, mint), stat) in sorted_pnl.iter() {
        let Some(quote_mint) = stat.get_quote_mint() else {
            continue;
        };
        let quote_ui = to_ui_amount(stat.quote_pnl, &quote_mint);

        let status = if stat.quote_pnl > 0 {
            "WIN "
        } else if stat.quote_pnl < 0 {
            "LOSS"
        } else {
            "EVEN"
        };

        let quote_symbol = quote_name(&quote_mint);

        // 根据本位币类型显示不同格式
        if stat.is_sol_based() {
            // SOL 本位：gas 已包含在 quote_pnl 中
            info!(
                "{:<4} | Payer: {:<8} | Token: {:<45} | {:>+10.4} {:>4} | 交易数: {:>3}",
                status,
                &payer.to_string()[..8],
                mint.to_string(),
                quote_ui,
                quote_symbol,
                stat.success_tx_count
            );
        } else {
            // 稳定币本位：单独显示 gas 成本
            let gas_ui = to_ui_amount(stat.sol_gas_cost, &Pubkey::default());
            info!(
                "{:<4} | Payer: {:<8} | Token: {:<45} | {:>+10.4} {:>4} | 交易数: {:>3} | gas:{:>+7.4} SOL",
                status,
                &payer.to_string()[..8],
                mint.to_string(),
                quote_ui,
                quote_symbol,
                stat.success_tx_count,
                gas_ui
            );
        }
    }

    info!("\n================================================");
    info!("💰 各本位币盈亏汇总：");

    for (quote_str, total_pnl) in summary.total_by_quote.iter() {
        if let Ok(quote_mint) = quote_str.parse::<Pubkey>() {
            let quote_ui = to_ui_amount(*total_pnl, &quote_mint);
            let quote_symbol = quote_name(&quote_mint);

            let emoji = if *total_pnl > 0 {
                "🎉"
            } else if *total_pnl < 0 {
                "😢"
            } else {
                "➖"
            };

            info!("  {} {}: {:>+12.4}", emoji, quote_symbol, quote_ui);
        }
    }

    info!(
        "📈 盈利币种: {} | 📉 亏损币种: {} | 胜率: {:.1}%",
        summary.win_count,
        summary.loss_count,
        if summary.win_count + summary.loss_count > 0 {
            summary.win_count as f64 / (summary.win_count + summary.loss_count) as f64 * 100.0
        } else {
            0.0
        }
    );
    info!("================================================\n");
}
