//! Report-only benchmark harness.
//!
//! Run through `scripts/bench.sh`, or directly:
//!
//! ```sh
//! cargo bench -p atomcode-capabilities --features bench --bench core -- \
//!     --iterations 5 --out target/bench/core
//! ```
//!
//! The harness measures medians itself. It has no benchmark-framework
//! dependency, so it cannot change `Cargo.lock`. The bench target is guarded by
//! `required-features = ["bench"]`, which keeps it out of the normal
//! `cargo check --workspace --all-targets` gate.
//!
//! What the numbers are:
//! - `sse_feed`: decoder throughput over synthetic SSE lines.
//! - `file_index_first_walk`: one complete gitignore-aware walk.
//! - `grep_cold` / `grep_warm`: the real grep tool over a fixed tree.
//! - `read_file_window`: a 200-line window read from an 8 MiB file. It still
//!   reads the whole file; that is the behaviour the metric tracks.
//! - `session_list`: `SessionManager::list` over 1000 event sessions.
//! - `session_resume`: opening one 20,000-event session.
//! - `tui_first_frame` / `tui_audit` (optional, `--e2e`): process wall time,
//!   including process startup. Noisy; compare only on the same machine.

use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use atomcode_capabilities::file_index::FileIndex;
use atomcode_capabilities::provider::bench as provider_bench;
use atomcode_capabilities::session::{SessionManager, SessionMeta, StorageOwner};
use atomcode_capabilities::tools::{GrepTool, ReadFileTool};
use atomcode_capabilities::ProductDirs;
use atomcode_kernel::session::{LoggedEvent, SessionEvent, SessionHeader, SESSION_FORMAT_VERSION};
use atomcode_kernel::tool::{ProgressSink, Tool, ToolContext};
use serde_json::json;
use tokio_util::sync::CancellationToken;

const FILES: usize = 2_000;
const FILE_BYTES: usize = 240;
const SESSIONS: usize = 1_000;
const RESUME_EVENTS: usize = 20_000;
const BIG_FILE_MIB: usize = 8;
const READ_WINDOW_OFFSET: usize = 50_000;
const READ_WINDOW_LIMIT: usize = 200;

struct Sample {
    name: &'static str,
    samples: Vec<f64>,
    note: String,
}

impl Sample {
    fn new(name: &'static str, samples: Vec<f64>, note: impl Into<String>) -> Self {
        Self {
            name,
            samples,
            note: note.into(),
        }
    }

    fn median(&self) -> f64 {
        median(&self.samples)
    }

    fn min(&self) -> f64 {
        self.samples.iter().copied().fold(f64::INFINITY, f64::min)
    }

    fn max(&self) -> f64 {
        self.samples
            .iter()
            .copied()
            .fold(f64::NEG_INFINITY, f64::max)
    }
}

fn main() {
    let options = Options::parse();
    let mut samples = Vec::new();

    samples.extend(bench_sse(options.iterations));
    samples.extend(bench_file_index(options.iterations));
    samples.extend(bench_grep(options.iterations));
    samples.extend(bench_read(options.iterations));
    samples.extend(bench_session_list(options.iterations));
    samples.extend(bench_session_resume(options.iterations));
    if options.e2e {
        samples.extend(bench_e2e(options.iterations));
    }

    let markdown = render_markdown(&samples);
    print!("{markdown}");
    if let Some(base) = &options.out {
        if let Err(error) = write_reports(base, &samples, &markdown) {
            eprintln!("failed to write reports to {}: {error}", base.display());
            std::process::exit(1);
        }
    }
}

// ---------------------------------------------------------------------------
// Options
// ---------------------------------------------------------------------------

struct Options {
    iterations: usize,
    out: Option<PathBuf>,
    e2e: bool,
}

impl Options {
    fn parse() -> Self {
        let mut options = Self {
            iterations: 5,
            out: None,
            e2e: std::env::var_os("BENCH_ATOMCODE_BIN").is_some_and(|path| !path.is_empty()),
        };
        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--iterations" | "-n" => {
                    let value = args.next().expect("--iterations needs a value");
                    options.iterations = value.parse().expect("--iterations must be a number");
                }
                "--out" | "-o" => {
                    options.out = Some(PathBuf::from(args.next().expect("--out needs a path")));
                }
                "--e2e" => options.e2e = true,
                // Cargo passes this to a `harness = false` bench target.
                "--bench" | "--test" | "--nocapture" => {}
                other => panic!("unknown argument: {other}"),
            }
        }
        assert!(options.iterations > 0, "--iterations must be at least 1");
        options
    }
}

// ---------------------------------------------------------------------------
// Metrics
// ---------------------------------------------------------------------------

fn bench_sse(iterations: usize) -> Vec<Sample> {
    const LINES: usize = 20_000;
    const PER_CHUNK: usize = 512;
    let _ = provider_bench::sse_feed(LINES, PER_CHUNK);
    let samples = measure(iterations, || provider_bench::sse_feed(LINES, PER_CHUNK));
    vec![Sample::new(
        "sse_feed",
        samples,
        format!("{LINES} lines, {PER_CHUNK} per chunk; one decoder"),
    )]
}

fn bench_file_index(iterations: usize) -> Vec<Sample> {
    let fixture = TempTree::new("index").expect("create index fixture");
    make_tree(&fixture.root, FILES, FILE_BYTES).expect("write index fixture");

    let samples = measure(iterations, || {
        let index = FileIndex::new(fixture.root.clone(), ".atomcode");
        index.filter_blocking("", "")
    });
    vec![Sample::new(
        "file_index_first_walk",
        samples,
        format!("{FILES} files; one complete walk per sample"),
    )]
}

fn bench_grep(iterations: usize) -> Vec<Sample> {
    let fixture = TempTree::new("grep").expect("create grep fixture");
    make_tree(&fixture.root, FILES, FILE_BYTES).expect("write grep fixture");
    let dirs = ProductDirs::new(fixture.user.clone(), ".atomcode");
    let tool = GrepTool::new(dirs);
    let ctx = context(&fixture.root);
    let args = r#"{"pattern":"needle","path":"."}"#;

    let runtime = runtime();
    let cold = vec![runtime.block_on(async {
        let start = Instant::now();
        let result = tool.execute(args, &ctx).await;
        black_box((result.content.len(), result.is_error));
        start.elapsed().as_secs_f64() * 1000.0
    })];

    let warm = measure(iterations, || {
        runtime.block_on(async {
            let result = tool.execute(args, &ctx).await;
            black_box((result.content.len(), result.is_error));
        })
    });

    vec![
        Sample::new("grep_cold", cold, "first run over the fixture tree"),
        Sample::new("grep_warm", warm, "page cache warm"),
    ]
}

fn bench_read(iterations: usize) -> Vec<Sample> {
    let fixture = TempTree::new("read").expect("create read fixture");
    let path = fixture.root.join("big.txt");
    make_big_file(&path, BIG_FILE_MIB).expect("write big file");
    let tool = ReadFileTool::new(false);
    let ctx = context(&fixture.root);
    let args = format!(
        r#"{{"file_path":"{}","offset":{READ_WINDOW_OFFSET},"limit":{READ_WINDOW_LIMIT}}}"#,
        path.to_string_lossy()
    );

    let runtime = runtime();
    // Warm the page cache once; the metric is the decode/scan cost, not the
    // first cold disk read.
    runtime.block_on(async {
        let _ = tool.execute(&args, &ctx).await;
    });
    let samples = measure(iterations, || {
        runtime.block_on(async {
            let result = tool.execute(&args, &ctx).await;
            black_box((result.content.len(), result.is_error));
        })
    });

    vec![Sample::new(
        "read_file_window",
        samples,
        format!(
            "{READ_WINDOW_LIMIT} lines at offset {READ_WINDOW_OFFSET} from a {BIG_FILE_MIB} MiB file; whole file is decoded"
        ),
    )]
}

fn bench_session_list(iterations: usize) -> Vec<Sample> {
    let fixture = TempTree::new("sessions-list").expect("create session fixture");
    let manager = SessionManager::with_root(fixture.root.join("sessions"));
    for index in 0..SESSIONS {
        create_session(&manager, &fixture.root, &format!("list-{index:06}"), 0);
    }

    let _ = manager.list();
    let samples = measure(iterations, || manager.list());
    vec![Sample::new(
        "session_list",
        samples,
        format!("{SESSIONS} event sessions; reads every index file"),
    )]
}

fn bench_session_resume(iterations: usize) -> Vec<Sample> {
    let fixture = TempTree::new("sessions-resume").expect("create resume fixture");
    let manager = SessionManager::with_root(fixture.root.join("sessions"));
    let id = "resume-bench";
    let lease = create_session(&manager, &fixture.root, id, RESUME_EVENTS);

    // Warm the log once, then reopen it. Each reopen re-reads and re-projects
    // the whole log; that is the path P3 is meant to improve.
    let _ = manager.open_for_resume(&lease);
    let samples = measure(iterations, || manager.open_for_resume(&lease));

    vec![Sample::new(
        "session_resume",
        samples,
        format!("{RESUME_EVENTS} events; reads and projects the whole log"),
    )]
}

fn bench_e2e(iterations: usize) -> Vec<Sample> {
    let Some(bin) = std::env::var_os("BENCH_ATOMCODE_BIN") else {
        return Vec::new();
    };
    let bin = PathBuf::from(bin);
    if !bin.is_file() {
        eprintln!("skipping --e2e: {} is not a file", bin.display());
        return Vec::new();
    }
    // Keep the audit/demo processes away from the developer's real product tree.
    let home = tempfile::Builder::new()
        .prefix("atomcode-bench-home-")
        .tempdir()
        .expect("create e2e home");

    let mut samples = Vec::new();
    for (name, args, note) in [
        (
            "tui_first_frame",
            vec!["--tui", "--demo"],
            "process wall time; includes startup; no config or model",
        ),
        (
            "tui_audit",
            vec!["--tui", "--audit"],
            "process wall time; full screen composition; no tty",
        ),
    ] {
        // Warm the binary and page cache once. A cold first run measured 6.9 s
        // for a debug binary whose pages were not resident; that is not a useful
        // signal for a code change.
        let _ = Command::new(&bin)
            .args(&args)
            .env("ATOMCODE_HOME", home.path())
            .output()
            .expect("warm atomcode");
        let runs = measure(iterations, || {
            let start = Instant::now();
            let output = Command::new(&bin)
                .args(&args)
                .env("ATOMCODE_HOME", home.path())
                .output()
                .expect("spawn atomcode");
            black_box((output.status.success(), output.stdout.len()));
            start.elapsed().as_secs_f64() * 1000.0
        });
        samples.push(Sample::new(
            name,
            runs,
            format!("{note}; warm binary and page cache"),
        ));
    }
    samples
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

struct TempTree {
    _dir: tempfile::TempDir,
    root: PathBuf,
    user: PathBuf,
}

impl TempTree {
    fn new(label: &str) -> std::io::Result<Self> {
        let dir = tempfile::Builder::new()
            .prefix(&format!("atomcode-bench-{label}-"))
            .tempdir()?;
        let root = dir.path().join("work");
        let user = dir.path().join("user");
        std::fs::create_dir_all(&root)?;
        std::fs::create_dir_all(&user)?;
        Ok(Self {
            _dir: dir,
            root,
            user,
        })
    }
}

fn make_tree(root: &Path, files: usize, bytes: usize) -> std::io::Result<()> {
    for index in 0..files {
        let dir = root.join(format!("d{:03}", index % 50));
        std::fs::create_dir_all(&dir)?;
        let body = format!(
            "{} {} {}\n",
            index,
            if index % 7 == 0 { "needle" } else { "haystack" },
            "x".repeat(bytes.saturating_sub(24))
        );
        std::fs::write(dir.join(format!("f{index:05}.txt")), body)?;
    }
    Ok(())
}

fn make_big_file(path: &Path, mib: usize) -> std::io::Result<()> {
    let target = mib * 1024 * 1024;
    let mut body = String::with_capacity(target + 128);
    let line = "abcdefghijklmnopqrstuvwxyz0123456789abcdefghijklmnopqrstuvwxyz0123456789\n";
    while body.len() < target {
        body.push_str(line);
    }
    std::fs::write(path, body)
}

fn create_session(
    manager: &SessionManager,
    root: &Path,
    id: &str,
    events: usize,
) -> atomcode_capabilities::session::SessionLease {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;
    let lease = manager.acquire_lease(id).expect("acquire lease");
    let header = SessionHeader {
        version: SESSION_FORMAT_VERSION,
        id: id.to_string(),
        created_at: now as u64,
        cwd: Some(root.to_string_lossy().into_owned()),
        parent: None,
        inherited: 0,
        context: None,
        member: None,
    };
    let mut meta = SessionMeta::new(id, root.to_string_lossy(), now);
    meta.owner = StorageOwner::Native;
    meta.format_version = SESSION_FORMAT_VERSION;
    manager
        .create_event_session(&lease, &header, &meta)
        .expect("create event session");

    if events > 0 {
        let logged: Vec<_> = (0..events)
            .map(|index| {
                let turn = (index / 5) as u64 + 1;
                let event = match index % 5 {
                    0 => SessionEvent::TurnStart { turn },
                    1 => SessionEvent::UserMessage {
                        turn,
                        text: format!("question {index}"),
                        images: Vec::new(),
                    },
                    2 => SessionEvent::StepStart { turn, step: 1 },
                    3 => SessionEvent::AssistantMessage {
                        turn,
                        round: 1,
                        text: format!("answer {index}"),
                        reasoning: String::new(),
                        tool_calls: Vec::new(),
                        reasoning_blocks: Vec::new(),
                        meta: None,
                    },
                    _ => SessionEvent::StepEnd {
                        turn,
                        step: 1,
                        tool_calls: 0,
                    },
                };
                LoggedEvent {
                    seq: index as u64 + 1,
                    at: now as u64 + index as u64,
                    event,
                }
            })
            .collect();
        manager
            .append_events(&lease, &logged)
            .expect("append events");
    }
    lease
}

fn context(root: &Path) -> ToolContext {
    ToolContext {
        working_dir: root.to_path_buf(),
        cancel: CancellationToken::new(),
        progress: ProgressSink::noop(),
        requester: None,
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime")
}

// ---------------------------------------------------------------------------
// Measurement / report
// ---------------------------------------------------------------------------

fn measure<T>(iterations: usize, mut work: impl FnMut() -> T) -> Vec<f64> {
    let mut samples = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let start = Instant::now();
        black_box(work());
        samples.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    samples
}

fn median(values: &[f64]) -> f64 {
    if values.is_empty() {
        return f64::NAN;
    }
    let mut sorted: Vec<f64> = values.to_vec();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let middle = sorted.len() / 2;
    if sorted.len() % 2 == 0 {
        (sorted[middle - 1] + sorted[middle]) / 2.0
    } else {
        sorted[middle]
    }
}

fn render_markdown(samples: &[Sample]) -> String {
    let mut out = String::new();
    out.push_str("# AtomCode benchmark\n\n");
    out.push_str(&format!("- git: `{}`\n", git_sha()));
    out.push_str(&format!("- rustc: `{}`\n", rustc_version()));
    out.push_str(&format!(
        "- platform: {} {}\n",
        std::env::consts::OS,
        std::env::consts::ARCH
    ));
    out.push_str(&format!(
        "- profile: {}\n\n",
        if cfg!(debug_assertions) {
            "debug (relative comparisons only)"
        } else {
            "release"
        }
    ));
    out.push_str("| metric | median ms | min ms | max ms | n | note |\n");
    out.push_str("|---|---:|---:|---:|---:|---|\n");
    for sample in samples {
        out.push_str(&format!(
            "| {} | {:.3} | {:.3} | {:.3} | {} | {} |\n",
            sample.name,
            sample.median(),
            sample.min(),
            sample.max(),
            sample.samples.len(),
            sample.note.replace('|', "\\|")
        ));
    }
    out
}

fn write_reports(
    base: &Path,
    samples: &[Sample],
    markdown: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(parent) = base.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json_path = base.with_extension("json");
    let markdown_path = base.with_extension("md");
    let json = json!({
        "generated_at_unix": SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
        "git_sha": git_sha(),
        "rustc": rustc_version(),
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "profile": if cfg!(debug_assertions) { "debug" } else { "release" },
        "results": samples.iter().map(|sample| json!({
            "name": sample.name,
            "unit": "ms",
            "median": sample.median(),
            "min": sample.min(),
            "max": sample.max(),
            "samples": &sample.samples,
            "note": &sample.note,
        })).collect::<Vec<_>>(),
    });
    std::fs::write(&json_path, serde_json::to_string_pretty(&json)?)?;
    std::fs::write(&markdown_path, markdown)?;
    Ok(())
}

fn git_sha() -> String {
    Command::new("git")
        .args(["rev-parse", "--short=12", "HEAD"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|sha| !sha.is_empty())
        .unwrap_or_else(|| "unknown".into())
}

fn rustc_version() -> String {
    Command::new("rustc")
        .arg("--version")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|version| !version.is_empty())
        .unwrap_or_else(|| "unknown".into())
}
