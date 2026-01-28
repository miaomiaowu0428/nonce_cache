use std::env;
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = env::args().collect();

    // 从命令行参数获取数据库路径，默认为 ./data/pnl_tracker
    let db_path = args
        .get(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("./data/pnl_tracker"));

    println!("📂 正在打开数据库: {}", db_path.display());
    println!();

    // 打开 sled 数据库
    let db = sled::open(&db_path)?;

    println!("========== 🌳 Sled Database Trees ==========\n");

    // 获取所有 tree 名称
    let tree_names = db.tree_names();

    if tree_names.is_empty() {
        println!("⚠️  数据库中没有找到任何 tree");
        return Ok(());
    }

    println!("📊 找到 {} 个 tree:\n", tree_names.len());

    // 遍历每个 tree
    for (index, tree_name) in tree_names.iter().enumerate() {
        let tree_name_str = String::from_utf8_lossy(tree_name);

        // 打开对应的 tree
        let tree = db.open_tree(tree_name)?;

        // 获取 tree 中的条目数量
        let count = tree.len();

        println!(
            "{}. Tree: {}",
            index + 1,
            if tree_name.is_empty() {
                "<default>"
            } else {
                tree_name_str.as_ref()
            }
        );
        println!("   📦 条目数: {}", count);

        // 如果条目数较少，可以显示前几个 key
        if count > 0 && count <= 10 {
            println!("   🔑 Keys:");
            for (i, item) in tree.iter().enumerate() {
                if let Ok((key, _value)) = item {
                    let key_str = String::from_utf8_lossy(&key);
                    println!("      {}. {}", i + 1, key_str);
                }
            }
        } else if count > 10 {
            println!("   🔑 前 5 个 Keys:");
            for (i, item) in tree.iter().take(5).enumerate() {
                if let Ok((key, _value)) = item {
                    let key_str = String::from_utf8_lossy(&key);
                    println!("      {}. {}", i + 1, key_str);
                }
            }
            println!("      ... (还有 {} 个)", count - 5);
        }

        println!();
    }

    // 打印数据库总体统计
    println!("========== 📊 数据库总体统计 ==========\n");

    let total_size = db.size_on_disk()?;
    println!(
        "💾 数据库总大小: {} bytes ({:.2} MB)",
        total_size,
        total_size as f64 / 1024.0 / 1024.0
    );

    // 检查是否需要压缩
    println!("\n💡 提示:");
    println!("  - 使用 'db.flush()' 可以将缓冲区数据刷新到磁盘");
    println!("  - Sled 会自动进行垃圾回收和压缩");

    println!("\n========== ✅ 分析完成 ==========\n");

    Ok(())
}
