//! Integration tests for the guide knowledge base pipeline.
//!
//! Tests the full flow: KnowledgeBase::embedded() -> search() -> render_for_query()

use atomcode_core::agent::guide::kb::KnowledgeBase;

#[test]
fn test_embedded_kb_loads_without_errors() {
    // Smoke test: all knowledge files should load without panicking
    let kb = KnowledgeBase::embedded();
    let rendered = kb.render_for_query("", 100);
    // Empty query returns all entries
    assert!(
        !rendered.is_empty(),
        "embedded KB should produce output for empty query"
    );
}

#[test]
fn test_chinese_query_matches_mcp() {
    let _guard = atomcode_core::i18n::test_lock();
    atomcode_core::i18n::set_locale(atomcode_core::locale::Locale::ZhCn);
    let kb = KnowledgeBase::embedded();
    let hits = kb.search("怎么配置mcp");
    assert!(
        !hits.is_empty(),
        "Chinese query '怎么配置mcp' should match MCP entry"
    );
}

#[test]
fn test_english_query_matches_memory() {
    let _guard = atomcode_core::i18n::test_lock();
    atomcode_core::i18n::set_locale(atomcode_core::locale::Locale::En);
    let kb = KnowledgeBase::embedded();
    let hits = kb.search("memory");
    assert!(
        !hits.is_empty(),
        "English query 'memory' should match memory entry"
    );
}

#[test]
fn test_mixed_query_matches_config() {
    let _guard = atomcode_core::i18n::test_lock();
    atomcode_core::i18n::set_locale(atomcode_core::locale::Locale::ZhCn);
    let kb = KnowledgeBase::embedded();
    let hits = kb.search("切换模型");
    assert!(
        !hits.is_empty(),
        "Mixed query '切换模型' should match config entry"
    );
}

#[test]
fn test_unknown_query_returns_fallback() {
    let _guard = atomcode_core::i18n::test_lock();
    // render_for_query with ASCII query uses English KB automatically,
    // but the i18n fallback message follows the global locale.
    // Use English locale so the assertion matches the English message.
    atomcode_core::i18n::set_locale(atomcode_core::locale::Locale::En);
    let kb = KnowledgeBase::embedded();
    let rendered = kb.render_for_query("xyznonexistent123", 100);
    assert!(
        rendered.contains("No entries found"),
        "Unknown query should return fallback message, got: {}",
        &rendered[..rendered.len().min(200)]
    );
}

#[test]
fn test_token_budget_truncation() {
    let _guard = atomcode_core::i18n::test_lock();
    atomcode_core::i18n::set_locale(atomcode_core::locale::Locale::ZhCn);
    let kb = KnowledgeBase::embedded();
    // Very tight budget should trigger truncation
    let rendered = kb.render_for_query("功能", 30);
    // Should contain either content or truncation marker
    assert!(
        rendered.contains("相关知识") || rendered.contains("截断") || rendered.contains("未找到"),
        "Tight budget should produce some output, got: {}",
        &rendered[..rendered.len().min(200)]
    );
}

#[test]
fn test_or_fallback_for_partial_keywords() {
    let _guard = atomcode_core::i18n::test_lock();
    atomcode_core::i18n::set_locale(atomcode_core::locale::Locale::En);
    let kb = KnowledgeBase::embedded();
    // "debug" is a keyword added to multiple files via expansion
    let hits = kb.search("debug");
    assert!(
        !hits.is_empty(),
        "OR fallback should match entries with 'debug' keyword"
    );
}

#[test]
fn test_troubleshooting_doc_is_searchable() {
    let _guard = atomcode_core::i18n::test_lock();
    atomcode_core::i18n::set_locale(atomcode_core::locale::Locale::ZhCn);
    let kb = KnowledgeBase::embedded();
    let hits = kb.search("故障");
    assert!(
        !hits.is_empty(),
        "Troubleshooting doc should be searchable via '故障' keyword"
    );

    let hits = kb.search("报错");
    assert!(
        !hits.is_empty(),
        "Troubleshooting doc should be searchable via '报错' keyword"
    );
}

#[test]
fn test_doc_urls_doc_is_searchable() {
    let _guard = atomcode_core::i18n::test_lock();
    atomcode_core::i18n::set_locale(atomcode_core::locale::Locale::ZhCn);
    let kb = KnowledgeBase::embedded();
    let hits = kb.search("文档");
    assert!(
        !hits.is_empty(),
        "Doc URLs doc should be searchable via '文档' keyword"
    );
}

#[test]
fn test_keybindings_query_returns_useful_content() {
    let _guard = atomcode_core::i18n::test_lock();
    atomcode_core::i18n::set_locale(atomcode_core::locale::Locale::ZhCn);
    let kb = KnowledgeBase::embedded();
    // Use a generous budget to ensure keybindings content is included
    let rendered = kb.render_for_query("键盘快捷键", 5000);
    assert!(
        rendered.contains("Ctrl"),
        "Keybindings content should contain actual shortcut keys, got: {}",
        &rendered[..rendered.len().min(300)]
    );
    assert!(
        rendered.contains("键盘快捷键"),
        "Keybindings content should contain title '键盘快捷键', got: {}",
        &rendered[..rendered.len().min(300)]
    );
}
