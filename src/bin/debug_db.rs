use std::env;
use std::path::PathBuf;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let db_path = env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("./data/pnl_tracker"));

    println!("📂 打开数据库: {}", db_path.display());
    println!();

    let db = sled::open(&db_path)?;

    println!("数据库信息:");
    println!("  - 路径: {}", db_path.display());
    println!("  - 大小: {} bytes", db.size_on_disk()?);
    println!();

    let mut count = 0;
    let mut total_size = 0;

    println!("========== 数据库内容 (原始键值对) ==========\n");

    for result in db.iter() {
        match result {
            Ok((key, value)) => {
                count += 1;
                total_size += key.len() + value.len();

                // 尝试解析键（应该是 Pubkey 字符串）
                let key_str = String::from_utf8_lossy(&key);

                println!("键 #{}: {}", count, key_str);
                println!("  值大小: {} bytes", value.len());

                // 尝试反序列化
                match bincode::deserialize::<nonce_cache::TokenPnL>(&value) {
                    Ok(pnl) => {
                        println!("  ✅ 成功解析:");
                        println!("     - quote_pnl: {}", pnl.quote_pnl);
                        println!("     - sol_gas_cost: {}", pnl.sol_gas_cost);
                        println!("     - quote_mint: {}", pnl.quote_mint);
                        println!("     - success_tx_count: {}", pnl.success_tx_count);
                    }
                    Err(e) => {
                        println!("  ❌ 解析失败: {}", e);
                        println!("  原始数据前20字节: {:?}", &value[..value.len().min(20)]);
                    }
                }
                println!();
            }
            Err(e) => {
                println!("❌ 读取错误: {}", e);
            }
        }
    }

    println!("========== 统计 ==========");
    println!("总记录数: {}", count);
    println!("总数据大小: {} bytes", total_size);

    if count == 0 {
        println!("\n⚠️  数据库中没有任何记录！");
        println!("\n可能的原因:");
        println!("  1. 盈亏跟踪器从未成功启动");
        println!("  2. 没有成功的交易被记录");
        println!("  3. 数据库路径不正确");
        println!("  4. 数据库被清空或损坏");
    }

    Ok(())
}
