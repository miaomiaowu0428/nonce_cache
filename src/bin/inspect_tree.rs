use chrono::{DateTime, Local};
use solana_sdk::pubkey::Pubkey;
use std::env;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = env::args().collect();

    if args.len() < 2 {
        print_usage();
        return Ok(());
    }

    let tree_name = &args[1];

    // 从命令行参数获取数据库路径，默认为 ./data/pnl_tracker
    let db_path = args
        .get(2)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("./data/pnl_tracker"));

    println!("📂 正在打开数据库: {}", db_path.display());
    println!("🌳 正在检查 tree: {}", tree_name);
    println!();

    // 打开 sled 数据库
    let db = sled::open(&db_path)?;

    // 打开指定的 tree
    let tree = match db.open_tree(tree_name) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("❌ 无法打开 tree '{}': {}", tree_name, e);
            eprintln!("\n💡 可用的 trees:");
            for name in db.tree_names() {
                let name_str = String::from_utf8_lossy(&name);
                eprintln!(
                    "  - {}",
                    if name.is_empty() {
                        "__sled__default"
                    } else {
                        name_str.as_ref()
                    }
                );
            }
            return Ok(());
        }
    };

    let count = tree.len();
    println!("========== 📊 Tree 信息 ==========\n");
    println!("Tree 名称: {}", tree_name);
    println!("条目总数: {}", count);
    println!();

    if count == 0 {
        println!("⚠️  Tree 为空");
        return Ok(());
    }

    // 检测数据格式
    let first_item = tree.iter().next();
    let format_type = if let Some(Ok((key, value))) = first_item {
        detect_format(&key, &value)
    } else {
        FormatType::Unknown
    };

    println!("🔍 检测到的格式: {}", format_type.description());
    println!();

    println!("========== 📋 详细内容 ==========\n");

    // 收集所有条目并排序
    let mut entries: Vec<(Vec<u8>, Vec<u8>)> = tree
        .iter()
        .filter_map(|item| item.ok())
        .map(|(k, v)| (k.to_vec(), v.to_vec()))
        .collect();

    // 根据格式类型排序
    match format_type {
        FormatType::PubkeyTimestamp => {
            // 按时间戳排序（最新的在前）
            entries.sort_by(|a, b| {
                let ts_a = parse_timestamp(&a.1).unwrap_or(0);
                let ts_b = parse_timestamp(&b.1).unwrap_or(0);
                ts_b.cmp(&ts_a)
            });
        }
        FormatType::PayerMintPnl => {
            // 按 key 字符串排序
            entries.sort_by(|a, b| a.0.cmp(&b.0));
        }
        _ => {
            // 默认按 key 排序
            entries.sort_by(|a, b| a.0.cmp(&b.0));
        }
    }

    // 打印所有条目
    for (index, (key, value)) in entries.iter().enumerate() {
        print!("{:>4}. ", index + 1);

        match format_type {
            FormatType::PubkeyTimestamp => {
                print_pubkey_timestamp_entry(key, value);
            }
            FormatType::PayerMintPnl => {
                print_payer_mint_entry(key, value);
            }
            FormatType::Unknown => {
                print_unknown_entry(key, value);
            }
        }

        println!();
    }

    println!("\n========== ✅ 检查完成 ==========\n");

    Ok(())
}

#[derive(Debug, Clone, Copy)]
enum FormatType {
    PubkeyTimestamp, // strategy_tokens_* 格式
    PayerMintPnl,    // __sled__default (pnl tracker) 格式
    Unknown,
}

impl FormatType {
    fn description(&self) -> &str {
        match self {
            FormatType::PubkeyTimestamp => "Pubkey (32 bytes) -> Timestamp (8 bytes)",
            FormatType::PayerMintPnl => "Payer:Mint (string) -> TokenPnL (bincode)",
            FormatType::Unknown => "Unknown format",
        }
    }
}

fn detect_format(key: &[u8], value: &[u8]) -> FormatType {
    // 检查是否是 Pubkey -> Timestamp 格式
    if key.len() == 32 && value.len() == 8 {
        return FormatType::PubkeyTimestamp;
    }

    // 检查是否是 Payer:Mint -> PnL 格式
    if let Ok(key_str) = std::str::from_utf8(key) {
        if key_str.contains(':') && key_str.len() == 88 {
            // "pubkey:pubkey" = 44+1+43 = 88
            return FormatType::PayerMintPnl;
        }
    }

    FormatType::Unknown
}

fn print_pubkey_timestamp_entry(key: &[u8], value: &[u8]) {
    // 解析 Pubkey
    if key.len() == 32 {
        if let Ok(pubkey) = Pubkey::try_from(key) {
            print!("Mint: {} ", pubkey);
        } else {
            print!("Mint: [invalid pubkey] ");
        }
    } else {
        print!("Mint: [invalid length: {}] ", key.len());
    }

    // 解析时间戳
    if let Some(timestamp) = parse_timestamp(value) {
        let datetime = timestamp_to_datetime(timestamp);
        print!("| 时间: {}", datetime);
    } else {
        print!("| 时间: [invalid timestamp]");
    }
}

fn print_payer_mint_entry(key: &[u8], value: &[u8]) {
    // 解析 key (Payer:Mint)
    if let Ok(key_str) = std::str::from_utf8(key) {
        let parts: Vec<&str> = key_str.split(':').collect();
        if parts.len() == 2 {
            println!("Payer: {}", parts[0]);
            println!("       Mint:  {}", parts[1]);
        } else {
            println!("Key: {}", key_str);
        }
    } else {
        println!("Key: [non-UTF8, {} bytes]", key.len());
    }

    // 尝试解析 TokenPnL
    if let Ok(pnl) = bincode::deserialize::<nonce_cache::TokenPnL>(value) {
        if let Some(quote_mint) = pnl.get_quote_mint() {
            let pnl_ui = nonce_cache::to_ui_amount(pnl.quote_pnl, &quote_mint);
            let gas_ui = nonce_cache::to_ui_amount(pnl.sol_gas_cost, &Pubkey::default());

            println!("       PnL:   {:+.6} (quote: {})", pnl_ui, quote_mint);
            println!("       Gas:   {:+.6} SOL", gas_ui);
            println!("       Txs:   {}", pnl.success_tx_count);
        }
    } else {
        println!("       Value: [cannot deserialize, {} bytes]", value.len());
    }
}

fn print_unknown_entry(key: &[u8], value: &[u8]) {
    // 尝试作为字符串打印 key
    if let Ok(key_str) = std::str::from_utf8(key) {
        if key_str.is_ascii() && !key_str.chars().any(|c| c.is_control()) {
            print!("Key: {} ", key_str);
        } else {
            print!("Key: [binary, {} bytes] ", key.len());
        }
    } else {
        print!("Key: [binary, {} bytes] ", key.len());
    }

    // 尝试作为字符串打印 value
    if let Ok(value_str) = std::str::from_utf8(value) {
        if value_str.is_ascii() && !value_str.chars().any(|c| c.is_control()) {
            print!("| Value: {}", value_str);
        } else {
            print!("| Value: [binary, {} bytes]", value.len());
        }
    } else {
        print!("| Value: [binary, {} bytes]", value.len());
    }
}

fn parse_timestamp(bytes: &[u8]) -> Option<u64> {
    if bytes.len() == 8 {
        let mut array = [0u8; 8];
        array.copy_from_slice(bytes);
        Some(u64::from_le_bytes(array))
    } else {
        None
    }
}

fn timestamp_to_datetime(timestamp: u64) -> String {
    let duration = Duration::from_secs(timestamp);
    let system_time = UNIX_EPOCH + duration;
    let datetime: DateTime<Local> = system_time.into();
    datetime.format("%Y-%m-%d %H:%M:%S").to_string()
}

fn print_usage() {
    println!("用法:");
    println!("  cargo run --bin inspect_tree <tree_name> [数据库路径]");
    println!();
    println!("参数:");
    println!("  <tree_name>      要检查的 tree 名称（必需）");
    println!("  [数据库路径]      sled 数据库路径（默认: ./data/pnl_tracker）");
    println!();
    println!("示例:");
    println!("  cargo run --bin inspect_tree __sled__default");
    println!("  cargo run --bin inspect_tree strategy_tokens_monkey_king");
    println!("  cargo run --bin inspect_tree strategy_tokens_bad_sniper ./data/pnl_tracker");
    println!();
    println!("💡 提示: 先运行 'cargo run --bin list_trees' 查看所有可用的 tree");
}
