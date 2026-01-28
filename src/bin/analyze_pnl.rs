use nonce_cache::{
    TokenPnL, init_pnl_db, print_pnl_report, query_all_pnl, query_pnl_summary, query_sorted_pnl,
    to_ui_amount,
};
use solana_sdk::pubkey::Pubkey;
use std::env;
use std::path::PathBuf;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 从命令行参数获取数据库路径，默认为 ./data/pnl_tracker
    let db_path = env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("./data/pnl_tracker"));

    println!("📂 正在加载数据库: {}", db_path.display());
    println!();

    // 初始化数据库
    if let Err(e) = init_pnl_db(Some(db_path.clone())).await {
        eprintln!("❌ 无法打开数据库: {}", e);
        eprintln!("请确保路径正确: {}", db_path.display());
        return Err(e.into());
    }

    // 直接从数据库加载所有数据（不依赖内存缓存）
    use nonce_cache::load_all_pnl;
    let all_pnl = load_all_pnl().await;

    if all_pnl.is_empty() {
        println!("⚠️  数据库为空，没有找到任何盈亏记录");
        return Ok(());
    }

    println!("✅ 成功加载 {} 个代币的盈亏数据", all_pnl.len());
    println!();

    // 1. 打印完整报告
    print_pnl_report().await;

    // 2. 详细统计
    println!("\n========== 📈 详细统计分析 ==========\n");

    let summary = query_pnl_summary().await;

    // 按本位币分组统计
    let mut by_quote: std::collections::HashMap<String, (usize, i128, i128)> =
        std::collections::HashMap::new();

    for (_, pnl) in all_pnl.iter() {
        let entry = by_quote.entry(pnl.quote_mint.clone()).or_insert((0, 0, 0));
        entry.0 += 1; // 代币数量
        entry.1 += pnl.quote_pnl; // 总盈亏
        entry.2 += pnl.sol_gas_cost; // 总 gas
    }

    println!("🔍 按本位币分组统计：");
    for (quote_str, (count, total_pnl, total_gas)) in by_quote.iter() {
        if let Ok(quote_mint) = quote_str.parse::<Pubkey>() {
            let quote_name = get_quote_name(&quote_mint);
            let pnl_ui = to_ui_amount(*total_pnl, &quote_mint);
            let gas_ui = to_ui_amount(*total_gas, &Pubkey::default());

            println!(
                "  {} - {} 个代币 | 盈亏: {:+.4} | Gas: {:+.4} SOL",
                quote_name, count, pnl_ui, gas_ui
            );
        }
    }

    // 3. Top 盈利/亏损
    println!("\n========== 🏆 Top 10 盈利代币 ==========\n");
    let top_winners = query_sorted_pnl(false).await; // 从高到低
    for (i, ((payer, mint), pnl)) in top_winners.iter().take(10).enumerate() {
        print_token_detail(i + 1, payer, mint, pnl);
    }

    println!("\n========== 📉 Top 10 亏损代币 ==========\n");
    let top_losers = query_sorted_pnl(true).await; // 从低到高
    for (i, ((payer, mint), pnl)) in top_losers.iter().take(10).enumerate() {
        if pnl.quote_pnl >= 0 {
            break; // 已经没有亏损的了
        }
        print_token_detail(i + 1, payer, mint, pnl);
    }

    // 4. 交易频率分析
    println!("\n========== 📊 交易频率分析 ==========\n");

    let mut tx_counts: Vec<_> = all_pnl
        .iter()
        .map(|((payer, mint), pnl)| ((*payer, *mint), pnl.success_tx_count))
        .collect();
    tx_counts.sort_by(|a, b| b.1.cmp(&a.1));

    let total_txs: usize = all_pnl.values().map(|p| p.success_tx_count).sum();
    let avg_txs = total_txs as f64 / all_pnl.len() as f64;

    println!("总交易次数: {}", total_txs);
    println!("平均每个代币: {:.1} 笔", avg_txs);
    println!("\nTop 5 交易最频繁的代币:");

    for (i, ((payer, mint), count)) in tx_counts.iter().take(5).enumerate() {
        if let Some(pnl) = all_pnl.get(&(*payer, *mint)) {
            let quote_mint = pnl.get_quote_mint().unwrap_or_default();
            let pnl_ui = to_ui_amount(pnl.quote_pnl, &quote_mint);
            println!(
                "  {}. Payer: {:<8} Token: {} - {} 笔交易 | 盈亏: {:+.4} {}",
                i + 1,
                &payer.to_string()[..8],
                mint,
                count,
                pnl_ui,
                get_quote_name(&quote_mint)
            );
        }
    }

    // 5. 胜率分析
    println!("\n========== 🎯 胜率分析 ==========\n");

    let win_rate = if summary.win_count + summary.loss_count > 0 {
        summary.win_count as f64 / (summary.win_count + summary.loss_count) as f64 * 100.0
    } else {
        0.0
    };

    println!("盈利代币: {} 个", summary.win_count);
    println!("亏损代币: {} 个", summary.loss_count);
    println!(
        "持平代币: {} 个",
        summary.total_tokens - summary.win_count - summary.loss_count
    );
    println!("胜率: {:.2}%", win_rate);

    // 6. 盈亏分布
    println!("\n========== 📉 盈亏分布 ==========\n");

    let ranges = vec![
        ("巨盈 (>1000)", 0, i128::MAX),
        ("大盈 (100-1000)", 100_000_000, 1000_000_000_000), // 按最小单位
        ("小盈 (0-100)", 0, 100_000_000),
        ("小亏 (-100-0)", -100_000_000, 0),
        ("大亏 (-1000--100)", -1000_000_000_000, -100_000_000),
        ("巨亏 (<-1000)", i128::MIN, -1000_000_000_000),
    ];

    for (label, min, max) in ranges.iter() {
        let count = all_pnl
            .values()
            .filter(|pnl| {
                let normalized_pnl = if let Some(_quote) = pnl.get_quote_mint() {
                    // 简单归一化：假设 6 decimals（稳定币）
                    pnl.quote_pnl / 1_000_000
                } else {
                    pnl.quote_pnl / 1_000_000
                };
                normalized_pnl >= *min && normalized_pnl < *max
            })
            .count();

        if count > 0 {
            println!("  {}: {} 个代币", label, count);
        }
    }

    println!("\n========== ✅ 分析完成 ==========\n");

    Ok(())
}

fn print_token_detail(rank: usize, payer: &Pubkey, mint: &Pubkey, pnl: &TokenPnL) {
    let quote_mint = pnl.get_quote_mint().unwrap_or_default();
    let pnl_ui = to_ui_amount(pnl.quote_pnl, &quote_mint);
    let quote_name = get_quote_name(&quote_mint);

    let status = if pnl.quote_pnl > 0 {
        "🟢"
    } else if pnl.quote_pnl < 0 {
        "🔴"
    } else {
        "⚪"
    };

    if pnl.is_sol_based() {
        println!(
            "{:>2}. {} Payer: {:<8} Token: {} | {:>+12.4} {} | {} 笔交易",
            rank,
            status,
            &payer.to_string()[..8],
            mint,
            pnl_ui,
            quote_name,
            pnl.success_tx_count
        );
    } else {
        let gas_ui = to_ui_amount(pnl.sol_gas_cost, &Pubkey::default());
        println!(
            "{:>2}. {} Payer: {:<8} Token: {} | {:>+12.4} {} | {} 笔交易 | gas: {:+.4} SOL",
            rank,
            status,
            &payer.to_string()[..8],
            mint,
            pnl_ui,
            quote_name,
            pnl.success_tx_count,
            gas_ui
        );
    }
}

fn get_quote_name(mint: &Pubkey) -> &'static str {
    let mint_str = mint.to_string();
    if mint_str == "USD1ttGY1N17NEEHLmELoaybftRBUSErhqYiQzvEmuB" {
        "USD1"
    } else if mint_str == "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v" {
        "USDC"
    } else if mint_str == "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB" {
        "USDT"
    } else if mint_str == "So11111111111111111111111111111111111111112" {
        "WSOL"
    } else if *mint == Pubkey::default() {
        "SOL"
    } else {
        "UNKNOWN"
    }
}
