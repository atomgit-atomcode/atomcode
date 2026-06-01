//! Model pricing — maps model names to per-million-token costs.

/// Returns (input_price, output_price) in USD per million tokens.
/// Uses substring matching to handle version suffixes (e.g. "claude-sonnet-4-20250514").
///
/// Match order matters: specific names (gpt-4o-mini, o1-mini) are tested
/// before their broader prefixes (gpt-4o, o1) to prevent premature hits.
/// This is tested by the ordering of the if-else chain — adding a new model
/// that is a substring of an existing one must go ABOVE the broader match.
/// Unknown models get a conservative (1.0, 3.0) fallback so cost tracking
/// never silently returns zero for a billable model.
pub fn cost_per_million(model: &str) -> (f64, f64) {
    let m = model.to_lowercase();
    // Claude
    if m.contains("opus") { (15.0, 75.0) }
    else if m.contains("sonnet") { (3.0, 15.0) }
    else if m.contains("haiku") { (0.25, 1.25) }
    // OpenAI (gpt-4o-mini must come before gpt-4o to avoid premature match)
    else if m.contains("gpt-4o-mini") { (0.15, 0.6) }
    else if m.contains("gpt-4o") { (2.5, 10.0) }
    else if m.contains("gpt-4.1") { (2.0, 8.0) }
    // o1/o3 mini must come before o1/o3 to avoid premature match
    else if m.contains("o1-mini") || m.contains("o3-mini") { (3.0, 12.0) }
    else if m.contains("o1") { (15.0, 60.0) }
    else if m.contains("o3") { (10.0, 40.0) }
    // DeepSeek
    else if m.contains("deepseek") { (0.27, 1.1) }
    // Qwen
    else if m.contains("qwen") { (0.5, 2.0) }
    // GLM / Zhipu
    else if m.contains("glm") { (0.5, 2.0) }
    // SiliconFlow / open models
    else if m.contains("llama") || m.contains("mistral") { (0.3, 0.6) }
    // MiniMax
    else if m.contains("minimax") || m.contains("m2.7") { (0.5, 2.0) }
    // Local / Ollama — free
    else if m.contains("ollama") { (0.0, 0.0) }
    // Unknown — conservative estimate
    else { (1.0, 3.0) }
}

/// Calculate cost in USD from token counts and model name.
pub fn calculate_cost(model: &str, prompt_tokens: usize, completion_tokens: usize, cached_tokens: usize) -> f64 {
    let (input_price, output_price) = cost_per_million(model);
    // Cached tokens are typically 90% cheaper (Anthropic) or free (OpenAI)
    let cached_price = input_price * 0.1;

    let input_cost = (prompt_tokens as f64 / 1_000_000.0) * input_price;
    let cached_cost = (cached_tokens as f64 / 1_000_000.0) * cached_price;
    let output_cost = (completion_tokens as f64 / 1_000_000.0) * output_price;

    input_cost + cached_cost + output_cost
}

/// Format cost as a human-readable string.
pub fn format_cost(cost: f64) -> String {
    if cost < 0.01 {
        format!("${:.4}", cost)
    } else {
        format!("${:.2}", cost)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_claude_sonnet_pricing() {
        let (i, o) = cost_per_million("claude-sonnet-4-20250514");
        assert_eq!(i, 3.0);
        assert_eq!(o, 15.0);
    }

    #[test]
    fn test_deepseek_pricing() {
        let (i, o) = cost_per_million("deepseek-chat");
        assert_eq!(i, 0.27);
        assert_eq!(o, 1.1);
    }

    #[test]
    fn test_gpt4o_pricing() {
        let (i, o) = cost_per_million("gpt-4o");
        assert_eq!(i, 2.5);
        assert_eq!(o, 10.0);
    }

    #[test]
    fn test_unknown_model() {
        let (i, o) = cost_per_million("some-unknown-model");
        assert_eq!(i, 1.0);
        assert_eq!(o, 3.0);
    }

    #[test]
    fn test_calculate_cost() {
        // 1000 prompt tokens + 500 completion tokens with deepseek
        let cost = calculate_cost("deepseek-chat", 1000, 500, 0);
        let expected = (1000.0 / 1_000_000.0) * 0.27 + (500.0 / 1_000_000.0) * 1.1;
        assert!((cost - expected).abs() < 1e-10);
    }

    #[test]
    fn test_calculate_cost_with_cache() {
        let cost = calculate_cost("claude-sonnet-4-20250514", 1000, 500, 800);
        // input: 1000 * 3.0/1M, cached: 800 * 0.3/1M, output: 500 * 15.0/1M
        let expected = 1000.0 * 3.0 / 1e6 + 800.0 * 0.3 / 1e6 + 500.0 * 15.0 / 1e6;
        assert!((cost - expected).abs() < 1e-10);
    }

    #[test]
    fn test_format_cost() {
        assert_eq!(format_cost(0.42), "$0.42");
        assert_eq!(format_cost(0.001), "$0.0010");
        assert_eq!(format_cost(12.5), "$12.50");
    }
}
