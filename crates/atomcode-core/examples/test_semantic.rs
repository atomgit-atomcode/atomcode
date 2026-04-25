use atomcode_core::semantic::SemanticSearcher;
use std::path::Path;

fn main() {
    let mut s = SemanticSearcher::new();

    println!("=== 场景1: 理解文件结构 ===");
    println!("传统方式: read_file tool/mod.rs → 296行全文 → LLM 从中寻找函数");
    println!("新方式: list_symbols → 只看签名\n");

    let path = Path::new("crates/atomcode-core/src/tool/mod.rs");
    if let Some(symbols) = s.list_symbols(path) {
        println!("list_symbols 输出 ({} 个符号):", symbols.len());
        for sym in &symbols {
            println!(
                "  {:30} L{}-{:>4}  ({})",
                sym.name, sym.start_line, sym.end_line, sym.kind
            );
        }
    }

    println!("\n=== 场景2: 精准读取单个函数 ===");
    println!("传统方式: read_file agent/mod.rs → 3000+行截断 → 再 read_file offset → 再补读");
    println!("新方式: extract_symbol → 只拿目标函数\n");

    let agent_path = Path::new("crates/atomcode-core/src/tool/bash.rs");
    if let Some(slice) = s.extract_symbol(agent_path, "check_destructive_command") {
        let lines = slice.text.lines().count();
        println!(
            "extract_symbol 输出: {} (L{}-{}, {} 行)",
            slice.name, slice.start_line, slice.end_line, lines
        );
        println!("--- 函数体 ---");
        println!("{}", slice.text);
        println!("--- END ---");
    }

    println!("\n=== 场景3: Skeleton 骨架 ===");
    println!("传统方式: read_file 大文件 → 2000行截断");
    println!("新方式: skeleton → 只看签名骨架\n");

    let big_path = Path::new("crates/atomcode-core/src/tool/bash.rs");
    let full_lines = std::fs::read_to_string(big_path).unwrap().lines().count();
    if let Some(skel) = s.skeleton(big_path) {
        let skel_lines = skel.lines().count();
        println!(
            "原文件: {} 行 → Skeleton: {} 行 (压缩 {}x)",
            full_lines,
            skel_lines,
            full_lines / skel_lines.max(1)
        );
        println!("{}", skel);
    }

    println!("\n=== 场景4: 大文件 agent/mod.rs ===");
    let agent_path = Path::new("crates/atomcode-core/src/agent/mod.rs");
    let full_lines = std::fs::read_to_string(agent_path).unwrap().lines().count();
    if let Some(skel) = s.skeleton(agent_path) {
        let skel_lines = skel.lines().count();
        println!(
            "原文件: {} 行 → Skeleton: {} 行 (压缩 {}x)",
            full_lines,
            skel_lines,
            full_lines / skel_lines.max(1)
        );
        println!("{}", skel);
    }
}
