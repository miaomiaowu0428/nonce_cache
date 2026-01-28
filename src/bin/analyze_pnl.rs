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

    // 1. 打印完整报告（手动生成，不用内存缓存）
    print_detailed_report(&all_pnl);

    // 2. 详细统计
    println!("\n========== 📈 详细统计分析 ==========\n");

    // 手动计算 summary（不依赖内存缓存）
    let summary = calculate_summary(&all_pnl);

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
    let mut sorted_pnl: Vec<_> = all_pnl.iter().collect();
    sorted_pnl.sort_by(|a, b| b.1.quote_pnl.cmp(&a.1.quote_pnl)); // 从高到低

    for (i, ((payer, mint), pnl)) in sorted_pnl.iter().take(10).enumerate() {
        print_token_detail(i + 1, payer, mint, pnl);
    }

    println!("\n========== 📉 Top 10 亏损代币 ==========\n");
    sorted_pnl.sort_by(|a, b| a.1.quote_pnl.cmp(&b.1.quote_pnl)); // 从低到高

    for (i, ((payer, mint), pnl)) in sorted_pnl.iter().take(10).enumerate() {
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

    let even_count = summary
        .total_tokens
        .saturating_sub(summary.win_count + summary.loss_count);

    println!("盈利代币: {} 个", summary.win_count);
    println!("亏损代币: {} 个", summary.loss_count);
    println!("持平代币: {} 个", even_count);
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

// 手动计算汇总（不依赖内存缓存）
fn calculate_summary(
    all_pnl: &std::collections::HashMap<(Pubkey, Pubkey), TokenPnL>,
) -> nonce_cache::PnLSummary {
    use std::collections::HashMap;

    let mut summary = nonce_cache::PnLSummary {
        total_tokens: all_pnl.len(),
        win_count: 0,
        loss_count: 0,
        total_by_quote: HashMap::new(),
    };

    for (_, pnl) in all_pnl.iter() {
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

// 手动打印报告（不依赖内存缓存）
fn print_detailed_report(all_pnl: &std::collections::HashMap<(Pubkey, Pubkey), TokenPnL>) {
    let mut sorted_pnl: Vec<_> = all_pnl.iter().collect();
    sorted_pnl.sort_by(|a, b| b.1.quote_pnl.cmp(&a.1.quote_pnl));

    let summary = calculate_summary(all_pnl);

    println!("\n========== 📊 实时盈亏报告 ==========\n");

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

        let quote_symbol = get_quote_name(&quote_mint);

        // 根据本位币类型显示不同格式
        if stat.is_sol_based() {
            // SOL 本位：gas 已包含在 quote_pnl 中
            println!(
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
            println!(
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

    println!("\n================================================");
    println!("💰 各本位币盈亏汇总：");

    for (quote_str, total_pnl) in summary.total_by_quote.iter() {
        if let Ok(quote_mint) = quote_str.parse::<Pubkey>() {
            let quote_ui = to_ui_amount(*total_pnl, &quote_mint);
            let quote_symbol = get_quote_name(&quote_mint);
            println!("  {}: {:>+12.4}", quote_symbol, quote_ui);
        }
    }

    println!();
    println!(
        "📈 盈利币种: {} | 📉 亏损币种: {} | 胜率: {:.1}%",
        summary.win_count,
        summary.loss_count,
        if summary.win_count + summary.loss_count > 0 {
            summary.win_count as f64 / (summary.win_count + summary.loss_count) as f64 * 100.0
        } else {
            0.0
        }
    );
    println!("================================================");
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
