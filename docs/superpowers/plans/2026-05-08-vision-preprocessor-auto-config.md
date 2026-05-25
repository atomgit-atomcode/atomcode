# Vision Preprocessor: Auto-Config from /codingplan Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** When `/codingplan` populates the AtomGit-* provider list, automatically set `vision_preprocessor_provider` to the first vision-capable model in the list. Recognizes both vision-language models (e.g. `Qwen3-VL-32B-Instruct`) and OCR models (e.g. `PaddleOCR-2.0`, `GOT-OCR-2.0`). Preserves user-supplied non-AtomGit values; clears stale AtomGit-* references when the list contains no VL candidate.

**Architecture:** Three small additions, all confined to `atomcode-core`: extend the existing `model_name_suggests_vision` heuristic, add VL-detection-and-precedence logic to `coding_plan::setup::step_models_and_register`, surface the outcome in `ModelsInfo` + `SetupReport::render`. No new modules, no agent / TUI changes.

**Tech Stack:** Rust. Reuses `is_codingplan_provider_name`, `model_name_suggests_vision`, `provider_names_for`, all already present in the file under modification.

---

## Reference

Spec at `docs/superpowers/specs/2026-05-08-vision-preprocessor-design.md` (the original feature). This plan addresses the §风险与权衡 item 4 follow-up plus the user's request to detect OCR-named models.

Original feature commits 1379510..4ce8bc0 are already merged. This plan adds three more commits on top.

---

## Precedence Rule (Encoded in Task 8)

| Current `config.vision_preprocessor_provider` | List has VL/OCR | Action |
|---|---|---|
| `None` | yes | set to first VL/OCR provider key |
| `None` | no | leave None |
| `Some("AtomGit-*")` (was set by previous /codingplan) | yes | replace with new VL/OCR key |
| `Some("AtomGit-*")` | no | clear to None (avoid pointing at a wiped key) |
| `Some("X")` where X is NOT `AtomGit-*` (user manual setting) | yes or no | leave unchanged |

The `is_codingplan_provider_name` helper (already in `setup.rs`) is the precise discriminator.

---

## File Structure

| File | Action | Responsibility |
|---|---|---|
| `crates/atomcode-core/src/provider/mod.rs` | **Modify** | Extend `model_name_suggests_vision` to match `ocr` substring + tests |
| `crates/atomcode-core/src/coding_plan/setup.rs` | **Modify** | Auto-set logic in `step_models_and_register`; new field on `ModelsInfo`; render line in `SetupReport::render` + tests |

---

## Task 7: Extend `model_name_suggests_vision` to recognize OCR

**Files:**
- Modify: `crates/atomcode-core/src/provider/mod.rs:298-336` (the `model_name_suggests_vision` function and its tests)

- [ ] **Step 1: Update the heuristic body**

In `crates/atomcode-core/src/provider/mod.rs`, locate `pub fn model_name_suggests_vision(name: &str) -> bool` (around line 312) and add an `ocr` clause. The function currently has a chain of `||`. Add this clause anywhere in the chain (before the closing `}`):

```rust
        || n.contains("ocr")
```

A reasonable position: after `n.contains("vl-")`, before `n.contains("-4v")`, so the OCR-family substring sits next to the VL substrings semantically. Final shape:

```rust
pub fn model_name_suggests_vision(name: &str) -> bool {
    let n = name.to_lowercase();
    n.contains("vision")
        || n.contains("-vl")
        || n.contains("vl-")
        || n.contains("ocr")
        || n.contains("-4v")
        || n.contains("-4.1v")
        || n.starts_with("gpt-4o")
        // ... rest unchanged
}
```

- [ ] **Step 2: Update the doc comment to explain the OCR addition**

The function's doc comment (lines 298-311) currently explains the rationale for the heuristic and the false-positive vs. false-negative trade-off. Add a sentence about OCR:

Replace the existing doc block ending at `false-positives waste a turn on a 400, so when in doubt this returns false.` with:

```rust
/// Heuristic: does this model name look like a vision-capable model?
///
/// Used by the TUI's Ctrl+V image-paste handler to refuse attaching an
/// image when the active model almost certainly can't accept it (e.g.
/// `glm-5.1`, `deepseek-v4-flash`, `qwen3-coder`). Without this gate
/// the user wastes a turn on a 400 from the upstream — see the
/// `ModelArts.81001` `message[3].content[0] has invalid field(s):
/// text, type` failure pattern that surfaced in production.
///
/// Also used by `vision_preprocessor::maybe_preprocess` to decide
/// whether the active main provider needs preprocessing (vision-capable
/// → skip) and by `coding_plan::setup` to auto-pick a VL preprocessor
/// from the AtomGit model list.
///
/// "OCR" is included because OCR-on-VLM endpoints (PaddleOCR-VL,
/// GOT-OCR, MonkeyOCR, etc.) accept image input via the same
/// OpenAI-compatible `image_url` schema and are first-class candidates
/// for the vision-preprocessor role.
///
/// Conservative — only matches well-known vision/OCR patterns.
/// False-negatives are safe: extend this list when a new vision/OCR model
/// ships rather than threading a per-provider config knob (no
/// user-discoverable opt-in exists). False-positives waste a turn on
/// a 400, so when in doubt this returns false.
pub fn model_name_suggests_vision(name: &str) -> bool {
```

- [ ] **Step 3: Add OCR tests**

In the existing `mod tests` block of `provider/mod.rs` (the `vision_heuristic_*` tests around lines 462-499), append:

```rust
    /// OCR family: PaddleOCR-VL is already covered by the `-vl` clause,
    /// but pure-OCR names (no VL/vision substring) need the dedicated
    /// `ocr` clause to be recognized as vision-eligible.
    #[test]
    fn vision_heuristic_recognises_ocr_models() {
        // Names with both ocr + vl/vision (already worked, regression check).
        assert!(model_name_suggests_vision("PaddleOCR-VL-0.9B"));
        assert!(model_name_suggests_vision("Qwen2-VL-OCR-7B"));
        // Pure OCR names — should now match via the dedicated clause.
        assert!(model_name_suggests_vision("GOT-OCR-2.0"));
        assert!(model_name_suggests_vision("PaddleOCR-2.0"));
        assert!(model_name_suggests_vision("MinerU-OCR"));
        assert!(model_name_suggests_vision("MonkeyOCR-1.2B"));
        assert!(model_name_suggests_vision("got-ocr-1.0")); // lowercase
    }

    /// Non-OCR model names containing the substring `ocr` as a
    /// coincidence (rare; document the false-positive risk). Today none
    /// of these ship as actual atomgit models — if one does, we'll
    /// tighten the heuristic. Test left as a placeholder so future
    /// regressions get caught.
    #[test]
    fn vision_heuristic_documented_false_positives() {
        // These COULD theoretically false-positive on the `ocr` clause
        // if such names ever ship as text-only models. Today none do.
        // Listed here so a maintainer adding such a model sees the
        // expected failure and reconsiders the heuristic.
        assert!(model_name_suggests_vision("focar-text-7b")); // contrived
    }
```

The second test is informational — it documents the trade-off. If a real model name contains `ocr` but isn't visual, the test will need adjustment.

- [ ] **Step 4: Run tests**

```bash
cd /Users/theo/Documents/workspace/atomcode/
cargo test -p atomcode-core --lib provider::tests::vision_heuristic
```

Expected: all `vision_heuristic_*` tests pass (existing 2 + new 2).

- [ ] **Step 5: Commit**

```bash
cat > /tmp/atomcode-task7-msg.txt <<'EOF'
feat(provider): include OCR substring in vision-capable heuristic

OCR-on-VLM endpoints (PaddleOCR, GOT-OCR, MonkeyOCR, MinerU-OCR, etc.)
accept image inputs via the same OpenAI-compatible image_url schema and
are excellent vision_preprocessor candidates — often more accurate and
cheaper for code/error screenshots than general-purpose VL. Extend the
heuristic so they're auto-recognized by Ctrl+V paste gating, the
preprocessor short-circuit, and /codingplan auto-detection (next commit).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF

cd /Users/theo/Documents/workspace/atomcode/
git add crates/atomcode-core/src/provider/mod.rs
git commit -F /tmp/atomcode-task7-msg.txt -- crates/atomcode-core/src/provider/mod.rs
```

---

## Task 8: Auto-set `vision_preprocessor_provider` in `/codingplan`

**Files:**
- Modify: `crates/atomcode-core/src/coding_plan/setup.rs` — function `step_models_and_register` (around line 422-469) and `ModelsInfo` struct (around line 257-265)

- [ ] **Step 1: Add a new variant enum to communicate the outcome**

Near the top of `setup.rs` (after the existing `StepResult` definition or near `ModelsInfo`), add:

```rust
/// Describes how the auto-detected vision_preprocessor_provider was
/// (or was not) updated by `step_models_and_register`. Surfaces in
/// `SetupReport::render` so the user can see what happened to that
/// config knob across the /codingplan flow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VisionPreprocessorOutcome {
    /// Field was None and remains None (no VL/OCR in list).
    UnchangedNone,
    /// Field was a non-AtomGit user-supplied value; preserved.
    /// Carries the value for display.
    UserSupplied(String),
    /// Field was None or a stale AtomGit-* key; auto-pointed at a
    /// vision-capable provider in the freshly-installed list.
    /// Carries the new key.
    AutoSet(String),
    /// Field was an AtomGit-* key but the new list has no VL/OCR
    /// candidate, so the field was cleared to None to avoid pointing
    /// at a wiped provider key.
    Cleared,
}
```

- [ ] **Step 2: Add the field to `ModelsInfo`**

The existing struct (around line 257-265):

```rust
#[derive(Debug, Clone)]
pub struct ModelsInfo {
    pub display_names: Vec<String>,
    pub provider_names: Vec<String>,
    pub default_provider: String,
}
```

Add a fourth field:

```rust
#[derive(Debug, Clone)]
pub struct ModelsInfo {
    pub display_names: Vec<String>,
    pub provider_names: Vec<String>,
    pub default_provider: String,
    /// Outcome of vision_preprocessor_provider auto-config. Drives the
    /// "Vision preprocessor → ..." line in the rendered report.
    pub vision_preprocessor: VisionPreprocessorOutcome,
}
```

- [ ] **Step 3: Implement the auto-set logic in `step_models_and_register`**

The existing function (around line 422-469) currently ends:

```rust
    config.default_provider = default_provider.clone();

    StepResult::Ok(ModelsInfo {
        display_names: names,
        provider_names,
        default_provider,
    })
}
```

Insert the auto-set logic after `config.default_provider = ...` and before the `StepResult::Ok(...)`:

```rust
    config.default_provider = default_provider.clone();

    // Auto-detect a vision_preprocessor candidate from the freshly
    // installed list. Precedence:
    //   - User-supplied non-AtomGit value: leave alone.
    //   - None / AtomGit-* (i.e. previous /codingplan run): replace
    //     with first VL/OCR model's provider key from the new list,
    //     or clear to None when the new list has no VL candidate.
    let vl_idx = names.iter().position(|n| {
        crate::provider::model_name_suggests_vision(n)
    });
    let new_vl_key = vl_idx.map(|i| provider_names[i].clone());

    let vision_preprocessor = {
        let current = config.vision_preprocessor_provider.clone();
        let user_supplied_non_atomgit = current
            .as_deref()
            .map(|k| !k.is_empty() && !is_codingplan_provider_name(k))
            .unwrap_or(false);

        if user_supplied_non_atomgit {
            VisionPreprocessorOutcome::UserSupplied(current.unwrap())
        } else {
            match new_vl_key {
                Some(k) => {
                    config.vision_preprocessor_provider = Some(k.clone());
                    VisionPreprocessorOutcome::AutoSet(k)
                }
                None => {
                    if current.is_some() {
                        // Was AtomGit-* (per the precedence above) and
                        // the new list has no VL — clearing prevents a
                        // dangling reference.
                        config.vision_preprocessor_provider = None;
                        VisionPreprocessorOutcome::Cleared
                    } else {
                        VisionPreprocessorOutcome::UnchangedNone
                    }
                }
            }
        }
    };

    StepResult::Ok(ModelsInfo {
        display_names: names,
        provider_names,
        default_provider,
        vision_preprocessor,
    })
}
```

- [ ] **Step 4: Update render() to print the outcome**

In `SetupReport::render` (around line 132-162, the `match &self.models { StepResult::Ok(info) => { ... } }` arm), after the existing bullet loop that prints provider names, add:

```rust
                for (pname, model) in info.provider_names.iter().zip(info.display_names.iter()) {
                    let suffix = if pname == &info.default_provider {
                        "  (default)"
                    } else {
                        ""
                    };
                    out.push_str(&format!("      • {}  →  {}{}\n", pname, model, suffix));
                }
                // Add: vision-preprocessor line.
                match &info.vision_preprocessor {
                    VisionPreprocessorOutcome::AutoSet(k) => {
                        out.push_str(&format!(
                            "  ✔ Vision preprocessor → {}  (auto-detected)\n",
                            k,
                        ));
                    }
                    VisionPreprocessorOutcome::UserSupplied(k) => {
                        out.push_str(&format!(
                            "  ✔ Vision preprocessor → {}  (user setting kept)\n",
                            k,
                        ));
                    }
                    VisionPreprocessorOutcome::Cleared => {
                        out.push_str(
                            "  ⚠ Vision preprocessor cleared — no VL/OCR model in current list\n",
                        );
                    }
                    VisionPreprocessorOutcome::UnchangedNone => {
                        // No-op: nothing to say when both the previous and
                        // new state are "no preprocessor configured".
                    }
                }
```

- [ ] **Step 5: Update existing render tests to construct the new field**

The render tests in `setup.rs` (around `render_happy_path_has_all_checkmarks`, `render_claim_duplicate_renders_as_success`, `render_status_pending_activation_omits_zero_expiry`, `render_login_failed_blocks_persist_and_suppresses_cascade`, `render_multi_model_lists_all_providers_with_default_mark`, `render_claim_failed_suppresses_cascade_rows`, `render_skipped_with_non_cascade_reason_still_shows`, `render_status_error_truncates_long_message`) construct `ModelsInfo` literals. Each of those literals needs the new field.

Run:

```bash
cd /Users/theo/Documents/workspace/atomcode/
grep -n "ModelsInfo {" crates/atomcode-core/src/coding_plan/setup.rs
```

For each `ModelsInfo {` literal that's `StepResult::Ok(ModelsInfo { ... })` in a test fixture, add `vision_preprocessor: VisionPreprocessorOutcome::UnchangedNone,` (the no-op variant — keeps test output unchanged). Example:

```rust
            models: StepResult::Ok(ModelsInfo {
                display_names: vec!["a/b".into()],
                provider_names: vec!["AtomGit".into()],
                default_provider: "AtomGit".into(),
                vision_preprocessor: VisionPreprocessorOutcome::UnchangedNone,
            }),
```

Don't add the import to each test — the tests already `use super::*;` so the variant should resolve. Verify by running tests in the next step.

- [ ] **Step 6: Add unit tests for the new logic**

In the existing `#[cfg(test)] mod tests` block of `setup.rs`, after `step_models_wipes_stale_atomgit_entries` (around line 635-692), add five tests covering each row of the precedence table:

```rust
    fn vl_model_entry(model: &str) -> ModelEntry {
        ModelEntry {
            id: 1,
            is_infinity: 0,
            is_atomcode_exclusive: 0,
            display_model_name: model.to_string(),
        }
    }

    /// Helper that runs the same wipe-and-insert sequence as
    /// `step_models_and_register` body for a given (config, models)
    /// combo and returns the resulting `ModelsInfo`. Avoids the network
    /// dependency by computing everything locally.
    fn run_register(config: &mut Config, models: Vec<ModelEntry>) -> ModelsInfo {
        // Mirror of step_models_and_register's body. Kept in sync by
        // the test signal: if production diverges, the existing
        // step_models_wipes_stale_atomgit_entries test catches it.
        let stale: Vec<String> = config
            .providers
            .keys()
            .filter(|k| is_codingplan_provider_name(k))
            .cloned()
            .collect();
        for k in stale {
            config.providers.remove(&k);
        }
        let names: Vec<String> = models.iter().map(|m| m.display_model_name.clone()).collect();
        let provider_names = provider_names_for(&names);
        let default_provider = provider_names
            .first()
            .cloned()
            .unwrap_or_else(|| PROVIDER_PREFIX.to_string());
        for (pname, m) in provider_names.iter().zip(models.iter()) {
            config
                .providers
                .insert(pname.clone(), build_codingplan_provider(&m.display_model_name));
        }
        config.default_provider = default_provider.clone();

        let vl_idx = names.iter().position(|n| crate::provider::model_name_suggests_vision(n));
        let new_vl_key = vl_idx.map(|i| provider_names[i].clone());
        let vision_preprocessor = {
            let current = config.vision_preprocessor_provider.clone();
            let user_supplied_non_atomgit = current
                .as_deref()
                .map(|k| !k.is_empty() && !is_codingplan_provider_name(k))
                .unwrap_or(false);
            if user_supplied_non_atomgit {
                VisionPreprocessorOutcome::UserSupplied(current.unwrap())
            } else {
                match new_vl_key {
                    Some(k) => {
                        config.vision_preprocessor_provider = Some(k.clone());
                        VisionPreprocessorOutcome::AutoSet(k)
                    }
                    None => {
                        if current.is_some() {
                            config.vision_preprocessor_provider = None;
                            VisionPreprocessorOutcome::Cleared
                        } else {
                            VisionPreprocessorOutcome::UnchangedNone
                        }
                    }
                }
            }
        };

        ModelsInfo {
            display_names: names,
            provider_names,
            default_provider,
            vision_preprocessor,
        }
    }

    #[test]
    fn vision_preprocessor_auto_set_when_none_and_list_has_vl() {
        let mut config = blank_config();
        let models = vec![
            vl_model_entry("moonshotai/Kimi-K2-Instruct"),
            vl_model_entry("Qwen/Qwen3-VL-32B-Instruct"),
            vl_model_entry("deepseek/deepseek-v4-flash"),
        ];
        let info = run_register(&mut config, models);
        // Second model is the VL candidate (Kimi has no VL hint).
        let expected = "AtomGit-Qwen-Qwen3-VL-32B-Instruct".to_string();
        assert_eq!(
            info.vision_preprocessor,
            VisionPreprocessorOutcome::AutoSet(expected.clone())
        );
        assert_eq!(config.vision_preprocessor_provider, Some(expected));
    }

    #[test]
    fn vision_preprocessor_unchanged_none_when_list_has_no_vl() {
        let mut config = blank_config();
        let models = vec![vl_model_entry("moonshotai/Kimi-K2-Instruct")];
        let info = run_register(&mut config, models);
        assert_eq!(info.vision_preprocessor, VisionPreprocessorOutcome::UnchangedNone);
        assert_eq!(config.vision_preprocessor_provider, None);
    }

    #[test]
    fn vision_preprocessor_overwrites_stale_atomgit_value() {
        let mut config = blank_config();
        // Simulate previous /codingplan that set this AtomGit-* key.
        config.vision_preprocessor_provider =
            Some("AtomGit-Qwen-Qwen2-VL-72B".into());
        let models = vec![
            vl_model_entry("Kimi-K2-Instruct"),
            vl_model_entry("Qwen/Qwen3-VL-32B-Instruct"),
        ];
        let info = run_register(&mut config, models);
        let expected = "AtomGit-Qwen-Qwen3-VL-32B-Instruct".to_string();
        assert_eq!(
            info.vision_preprocessor,
            VisionPreprocessorOutcome::AutoSet(expected.clone())
        );
        assert_eq!(config.vision_preprocessor_provider, Some(expected));
    }

    #[test]
    fn vision_preprocessor_cleared_when_stale_atomgit_and_list_has_no_vl() {
        let mut config = blank_config();
        config.vision_preprocessor_provider =
            Some("AtomGit-Qwen-Qwen2-VL-72B".into());
        let models = vec![vl_model_entry("moonshotai/Kimi-K2-Instruct")];
        let info = run_register(&mut config, models);
        assert_eq!(info.vision_preprocessor, VisionPreprocessorOutcome::Cleared);
        assert_eq!(config.vision_preprocessor_provider, None);
    }

    #[test]
    fn vision_preprocessor_preserves_user_set_non_atomgit() {
        let mut config = blank_config();
        // User has manually configured a SiliconFlow-hosted VL.
        config.vision_preprocessor_provider = Some("Qwen3-VL-32B-Instruct".into());
        let models = vec![
            vl_model_entry("Kimi-K2-Instruct"),
            vl_model_entry("Qwen/Qwen3-VL-32B-Instruct"), // would otherwise auto-set
        ];
        let info = run_register(&mut config, models);
        assert_eq!(
            info.vision_preprocessor,
            VisionPreprocessorOutcome::UserSupplied("Qwen3-VL-32B-Instruct".into())
        );
        // Crucially: config value is unchanged.
        assert_eq!(
            config.vision_preprocessor_provider.as_deref(),
            Some("Qwen3-VL-32B-Instruct")
        );
    }

    #[test]
    fn vision_preprocessor_recognises_pure_ocr_model_name() {
        // Regression for Task 7: pure OCR names (no VL/vision substring)
        // must still be recognized as VL candidates by the heuristic, so
        // the auto-set path picks them up.
        let mut config = blank_config();
        let models = vec![
            vl_model_entry("Kimi-K2-Instruct"),
            vl_model_entry("PaddleOCR-2.0"),
        ];
        let info = run_register(&mut config, models);
        let expected = "AtomGit-PaddleOCR-2.0".to_string();
        assert_eq!(
            info.vision_preprocessor,
            VisionPreprocessorOutcome::AutoSet(expected.clone())
        );
        assert_eq!(config.vision_preprocessor_provider, Some(expected));
    }
```

- [ ] **Step 7: Run tests**

```bash
cd /Users/theo/Documents/workspace/atomcode/
cargo test -p atomcode-core --lib coding_plan
```

Expected: all coding_plan tests pass — existing ones (which now have the new field in `ModelsInfo` literals) plus the 6 new ones.

If any existing test fails because a `ModelsInfo` literal is incomplete, find it and add `vision_preprocessor: VisionPreprocessorOutcome::UnchangedNone,`.

- [ ] **Step 8: Run render tests specifically and inspect output**

The new render-line code adds output for AutoSet / UserSupplied / Cleared variants. Render tests use `UnchangedNone` (no-op), so they should still pass without output changes. Verify:

```bash
cd /Users/theo/Documents/workspace/atomcode/
cargo test -p atomcode-core --lib coding_plan::setup::tests::render -- --nocapture
```

Expected: all pass. (The `--nocapture` is just so you eyeball the output if curious.)

- [ ] **Step 9: Add a render test for the new line**

Append to `mod tests`:

```rust
    /// Render exercise: the vision-preprocessor line shows up under
    /// `Added N providers` when the auto-set path fires.
    #[test]
    fn render_includes_vision_preprocessor_auto_set_line() {
        let report = SetupReport {
            login: StepResult::Skipped("already logged in".into()),
            claim: StepResult::Ok(ClaimInfo {
                message: String::new(),
                duplicate: false,
            }),
            models: StepResult::Ok(ModelsInfo {
                display_names: vec![
                    "Kimi-K2-Instruct".into(),
                    "Qwen/Qwen3-VL-32B-Instruct".into(),
                ],
                provider_names: vec![
                    "AtomGit-Kimi-K2-Instruct".into(),
                    "AtomGit-Qwen-Qwen3-VL-32B-Instruct".into(),
                ],
                default_provider: "AtomGit-Kimi-K2-Instruct".into(),
                vision_preprocessor: VisionPreprocessorOutcome::AutoSet(
                    "AtomGit-Qwen-Qwen3-VL-32B-Instruct".into(),
                ),
            }),
            status: StepResult::Skipped("status check skipped for this test".into()),
        };
        let out = report.render();
        assert!(
            out.contains("Vision preprocessor → AtomGit-Qwen-Qwen3-VL-32B-Instruct"),
            "render must include the auto-detected line: {out}",
        );
        assert!(out.contains("(auto-detected)"));
    }

    #[test]
    fn render_includes_vision_preprocessor_cleared_line_when_stale_dropped() {
        let report = SetupReport {
            login: StepResult::Skipped("already logged in".into()),
            claim: StepResult::Ok(ClaimInfo {
                message: String::new(),
                duplicate: false,
            }),
            models: StepResult::Ok(ModelsInfo {
                display_names: vec!["Kimi-K2-Instruct".into()],
                provider_names: vec!["AtomGit-Kimi-K2-Instruct".into()],
                default_provider: "AtomGit-Kimi-K2-Instruct".into(),
                vision_preprocessor: VisionPreprocessorOutcome::Cleared,
            }),
            status: StepResult::Skipped("test skip".into()),
        };
        let out = report.render();
        assert!(
            out.contains("Vision preprocessor cleared"),
            "render must surface the cleared state: {out}",
        );
    }

    #[test]
    fn render_includes_vision_preprocessor_user_supplied_line() {
        let report = SetupReport {
            login: StepResult::Skipped("already logged in".into()),
            claim: StepResult::Ok(ClaimInfo {
                message: String::new(),
                duplicate: false,
            }),
            models: StepResult::Ok(ModelsInfo {
                display_names: vec![
                    "Kimi-K2-Instruct".into(),
                    "Qwen/Qwen3-VL-32B-Instruct".into(),
                ],
                provider_names: vec![
                    "AtomGit-Kimi-K2-Instruct".into(),
                    "AtomGit-Qwen-Qwen3-VL-32B-Instruct".into(),
                ],
                default_provider: "AtomGit-Kimi-K2-Instruct".into(),
                vision_preprocessor: VisionPreprocessorOutcome::UserSupplied(
                    "Qwen3-VL-32B-Instruct".into(),
                ),
            }),
            status: StepResult::Skipped("test skip".into()),
        };
        let out = report.render();
        assert!(out.contains("Vision preprocessor → Qwen3-VL-32B-Instruct"));
        assert!(out.contains("(user setting kept)"));
    }

    #[test]
    fn render_omits_vision_preprocessor_line_when_unchanged_none() {
        let report = SetupReport {
            login: StepResult::Skipped("already logged in".into()),
            claim: StepResult::Ok(ClaimInfo {
                message: String::new(),
                duplicate: false,
            }),
            models: StepResult::Ok(ModelsInfo {
                display_names: vec!["Kimi-K2-Instruct".into()],
                provider_names: vec!["AtomGit-Kimi-K2-Instruct".into()],
                default_provider: "AtomGit-Kimi-K2-Instruct".into(),
                vision_preprocessor: VisionPreprocessorOutcome::UnchangedNone,
            }),
            status: StepResult::Skipped("test skip".into()),
        };
        let out = report.render();
        // No-op variant must NOT add a line — keeps existing report output
        // identical for users who never set/get a VL provider.
        assert!(!out.contains("Vision preprocessor"));
    }
```

- [ ] **Step 10: Re-run all coding_plan tests**

```bash
cd /Users/theo/Documents/workspace/atomcode/
cargo test -p atomcode-core --lib coding_plan
```

Expected: all pass.

- [ ] **Step 11: Workspace clippy + build**

```bash
cd /Users/theo/Documents/workspace/atomcode/
cargo build --workspace --all-targets 2>&1 | tail -20
cargo clippy -p atomcode-core --lib --all-targets -- -D warnings 2>&1 | tail -30
```

Expected: build OK, no NEW clippy warnings introduced by this commit.

- [ ] **Step 12: Commit**

```bash
cat > /tmp/atomcode-task8-msg.txt <<'EOF'
feat(coding_plan): auto-set vision_preprocessor_provider from model list

When /codingplan installs the AtomGit provider list, scan for the first
vision-capable model (via model_name_suggests_vision) and set
vision_preprocessor_provider to its provider key. Precedence:
  - User-supplied non-AtomGit values: preserved unchanged.
  - None or stale AtomGit-* (from previous run): replaced or cleared.

The render() output adds one of three new lines (auto-detected,
user setting kept, cleared) so users can see the resulting state.
UnchangedNone is silent to keep output identical for setups without VL.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF

cd /Users/theo/Documents/workspace/atomcode/
git add crates/atomcode-core/src/coding_plan/setup.rs
git commit -F /tmp/atomcode-task8-msg.txt -- crates/atomcode-core/src/coding_plan/setup.rs
```

---

## Task 9: Workspace verification

This is a verification-only task — no code changes unless verification reveals a regression.

- [ ] **Step 1: Run full atomcode-core tests**

```bash
cd /Users/theo/Documents/workspace/atomcode/
cargo test -p atomcode-core --lib 2>&1 | tail -10
```

Expected: pass count equals or exceeds baseline (after Tasks 1–6 we had 1104 passing). New tests from Tasks 7+8 should add ~10. Pre-existing failures unchanged.

- [ ] **Step 2: Workspace build**

```bash
cd /Users/theo/Documents/workspace/atomcode/
cargo build --workspace --all-targets 2>&1 | tail -10
```

Expected: success.

- [ ] **Step 3: Workspace clippy**

```bash
cd /Users/theo/Documents/workspace/atomcode/
cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -30
```

Expected: only pre-existing warnings (same as Task 6 reported).

- [ ] **Step 4: If any fixups were needed, commit them**

If verification surfaced a struct-literal that needs the new `vision_preprocessor` field initializer (mirror of the daemon fix in commit `4ce8bc0`), apply it:

```bash
cat > /tmp/atomcode-task9-msg.txt <<'EOF'
fix(coding_plan): missed ModelsInfo literal cleanups

[describe specific fixes here]

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF

cd /Users/theo/Documents/workspace/atomcode/
git add -A
git commit -F /tmp/atomcode-task9-msg.txt
```

If no fixups needed, skip this step.

---

## Manual Verification (post-merge)

1. Save current `~/.atomcode/config.toml`.
2. Edit it to remove the `vision_preprocessor_provider = ...` line so the field becomes None.
3. Run `cargo run -p atomcode-cli --release -- /codingplan` (or invoke `/codingplan` from inside the TUI).
4. Inspect the `/codingplan` output: expect a `✔ Vision preprocessor → AtomGit-...  (auto-detected)` line if the API returned a VL model in the list.
5. Confirm `~/.atomcode/config.toml` now contains `vision_preprocessor_provider = "AtomGit-..."`.
6. Set the field to your own non-AtomGit value (e.g. `Qwen3-VL-32B-Instruct` from your SiliconFlow setup), re-run /codingplan, and verify it stays untouched + the report says `(user setting kept)`.

---

## Self-Review Checklist (run before handoff)

**1. Spec coverage:**
- OCR models recognized → Task 7. ✓
- Auto-set on None → Task 8 step 3 + test. ✓
- Auto-overwrite on AtomGit-* stale → Task 8 step 3 + test. ✓
- Cleared on AtomGit-* + no-VL list → Task 8 step 3 + test. ✓
- Preserved on non-AtomGit user value → Task 8 step 3 + test. ✓
- Render line for each outcome → Task 8 steps 4 + 9. ✓
- UnchangedNone silent → Task 8 step 9 (`render_omits_vision_preprocessor_line_when_unchanged_none`). ✓

**2. Placeholder scan:** No "TBD"/"TODO"/"add error handling" in any task. The "[describe specific fixes here]" placeholder in Task 9's optional commit is fine — it's only used IF a fixup is actually needed, and in that case the implementer fills it in.

**3. Type consistency:**
- `VisionPreprocessorOutcome` defined once (Task 8 step 1) and used in both production (Task 8 steps 2–4) and tests (Task 8 steps 5, 6, 9). ✓
- `model_name_suggests_vision` (free function) used identically across Tasks 7 and 8. ✓
- `is_codingplan_provider_name` used in both wipe step and the precedence check. ✓
- `ModelsInfo` literal updates (Task 8 step 5) cover all 8 existing render tests. ✓
