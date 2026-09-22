//! `coding_overlay` 铺出来的那一层，必须能被加载器读进去。
//!
//! 这个文件的存在是一条教训：这个函数把值**拼进 TOML 模板**，而拼错的方式不止
//! 一种。曾经修过一轮（把 `{:?}` 换成序列化器），判据只验了序列化器自己的输出
//! —— 而真正出问题的是**替换那一步**：占位符在引号内、替换时把序列化器给的引号
//! 切掉，于是只有当序列化器恰好选双引号时才碰巧正确。
//!
//! `toml::Value::String(..).to_string()` 并不总是双引号：值里含 `"` 或反斜杠、
//! 且不含单引号时，它选**字面量**字符串（`'/tmp/we"ird'`）。切掉那对单引号，
//! 剩下的原样塞进 `"…"` —— 等于没转义。
//!
//! 所以这里的判据走**完整链路**：造一个刁钻路径 → `coding_overlay` → 加载器解析
//! → 逐字段比对值。任何在中间被改写、转义错、或被静默丢掉的字符都会让它红。

use std::path::Path;

use atomcode_plexus::{Layer, Op};

/// 会让「拼进 TOML」出错的字符，逐类一个。
///
/// 单引号那条是刻意的对照：序列化器对它的处理**恰好**让旧代码看起来是对的，
/// 所以它必须留在样本里 —— 一个只在「碰巧正确」的输入上通过的实现，会因为这个
/// 样本存在而无法蒙混。
const AWKWARD: &[(&str, &str)] = &[
    ("普通路径", "/tmp/plain"),
    ("双引号", "/tmp/we\"ird"),
    ("反斜杠", "/tmp/back\\slash"),
    ("单引号", "/tmp/it's-here"),
    ("DEL 控制字符", "/tmp/del\u{7f}x"),
    ("换行", "/tmp/two\nlines"),
    ("制表", "/tmp/tab\there"),
    ("CJK", "/tmp/中文目录"),
    ("空格与井号", "/tmp/a b #c"),
    ("等号与花括号", "/tmp/{a}=b"),
];

/// 逐层找出某一行（`id` 省略时 loader 用 `name`）。
fn config_of(layer: &Layer, name: &str) -> serde_json::Value {
    let mut found: Option<serde_json::Value> = None;
    for op in &layer.ops {
        if let Op::Insert(entries) = op {
            for e in entries {
                let id = if e.id.is_empty() { &e.name } else { &e.id };
                if id == name {
                    found = Some(e.config.clone());
                }
            }
        }
    }
    found.unwrap_or_else(|| panic!("`{name}` is not in the layer this overlay builds"))
}

#[test]
fn the_overlay_loads_and_keeps_every_value_verbatim() {
    for (what, dir) in AWKWARD {
        let working_dir = Path::new(dir);
        let artifacts = working_dir.join("artifacts");
        let overlay = atomcode_coding::on_harness::coding_overlay(
            working_dir,
            &artifacts,
            atomcode_coding::on_harness::Presence::Attended,
            "a-model",
        );

        // The real consumer. A layer that does not parse is a tree that does not
        // start — which is how this showed up in the first place, as
        // `TOML parse error at line 213` pointing into a generated file nobody
        // had ever looked at.
        let layer = Layer::from_toml(&overlay)
            .unwrap_or_else(|e| panic!("{what}: the overlay does not load: {e}"));

        // And the values survived the trip. Three placeholders, three chances to
        // have escaped the wrong thing — `working_dir` appears in more than one
        // row, so every row that carries it is checked rather than the first.
        let rows = [
            "tool-open-file-workspace",
            "tool-write-approval",
            "tool-bash-workspace",
        ];
        for row in rows {
            let cfg = config_of(&layer, row);
            assert_eq!(
                cfg.get("working_dir").and_then(|v| v.as_str()),
                Some(*dir),
                "{what}: `{row}` came back with a different working_dir\n  overlay:\n{overlay}"
            );
        }

        let cfg = config_of(&layer, "tool-output-artifact");
        assert_eq!(
            cfg.get("dir").and_then(|v| v.as_str()),
            Some(artifacts.to_string_lossy().as_ref()),
            "{what}: `tool-output-artifact` came back with a different dir"
        );

        let cfg = config_of(&layer, "persona-atomcode");
        assert_eq!(
            cfg.get("model").and_then(|v| v.as_str()),
            Some("a-model"),
            "{what}: the persona came back with a different model"
        );
    }
}

/// 转义不是「全都加一层」，布尔仍是布尔。
///
/// `{force_verify}` 走的是同一条替换路径，但它是 TOML **布尔** —— 一旦它也被
/// 当成字符串去转义，`force = "true"` 会让 `verify-cadence` 的配置多出一个引号，
/// 而那是另一种坏法：解析得过，值却是错的类型。
#[test]
fn the_boolean_placeholder_stays_a_boolean() {
    for (presence, want) in [
        (atomcode_coding::on_harness::Presence::Attended, false),
        (atomcode_coding::on_harness::Presence::Headless, true),
    ] {
        let overlay = atomcode_coding::on_harness::coding_overlay(
            Path::new("/tmp/x"),
            Path::new("/tmp/x/artifacts"),
            presence,
            "m",
        );
        let layer = Layer::from_toml(&overlay).expect("the overlay loads");
        let cfg = config_of(&layer, "verify-cadence");
        assert_eq!(
            cfg.get("force").and_then(|v| v.as_bool()),
            Some(want),
            "`force` must be a TOML boolean, not a quoted string"
        );
    }
}

/// 加载器的另一个入口也要过 —— `mount_swappable` 拼的是别的层。
///
/// 同一个函数里还有第三条路径（`scoped`: `agent-loop.working_dir`、
/// `llm.provider_id`），它走的是 `toml_string` 的完整输出。这里顺手钉住它，因为
/// 两条路径用的是同一个值、却有各自的拼法，而**修好一条不等于修好另一条**正是
/// 上一轮的教训。
#[test]
fn the_scoped_layer_loads_too() {
    for (what, dir) in AWKWARD {
        let overlay = format!(
            "[[patch]]\nid = \"agent-loop\"\nconfig = {{ working_dir = {} }}\n",
            atomcode_harness::bundle::toml_string(dir)
        );
        let layer = Layer::from_toml(&overlay)
            .unwrap_or_else(|e| panic!("{what}: the scoped layer does not load: {e}"));
        let mut got = None;
        for op in &layer.ops {
            if let Op::Patch { id, config, .. } = op {
                if id == "agent-loop" {
                    got = config
                        .as_ref()
                        .and_then(|c| c.get("working_dir"))
                        .and_then(|v| v.as_str())
                        .map(str::to_string);
                }
            }
        }
        assert_eq!(got.as_deref(), Some(*dir), "{what}: working_dir changed");
    }
}

/// 轮次预算属于这份共享清单，不属于某一个宿主。
///
/// 起因是一次修了一半:映射先写在 `product.rs`(全屏 TUI 那个宿主),于是
/// `mount_swappable`(拿着 `AgentHandle` 的宿主)仍然停在 `infra` 的 24 ——
/// 同一个 coding agent,两个宿主两种行为,而**谁也不报错**。
///
/// 所以判据落在 `CODING_DEFAULTS` 上:两个宿主的层序里都有它,谁都不能绕开。
#[test]
fn the_shared_list_carries_the_round_budget() {
    let layer = Layer::from_toml(atomcode_coding::on_harness::CODING_DEFAULTS)
        .expect("CODING_DEFAULTS must parse on its own — every host loads it");

    let got = layer.ops.iter().find_map(|op| match op {
        Op::Patch { id, config, .. } if id == "round-cap" => {
            config.as_ref().and_then(|c| c.get("max_rounds")).cloned()
        }
        _ => None,
    });
    assert_eq!(
        got,
        Some(serde_json::json!(0)),
        "`CODING_DEFAULTS` must state the round budget, or every host inherits \
         `infra`'s 24 and this assembly stops being the agent the engine runs"
    );
}
