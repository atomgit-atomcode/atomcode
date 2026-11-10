# Parser attribution and local implementation comparison

An Apple linker-map build of the phase-one candidate measures 22,970,268 bytes of live grammar C symbols (21.91 MiB). Kotlin accounts for 5,781,516 bytes and C# for 5,327,905 bytes. These are linked symbol contributions, excluding dead-stripped symbols, not archive file sizes. Alignment/shared pools/Rust wrappers are excluded, and removing a language would not necessarily shrink the executable by exactly that amount. See [the parser measurement](measurements/2026-11-10-parser-size.json).

Regenerating the same Kotlin grammar JSON with upstream Tree-sitter CLI 0.26.7, ABI 14, keeps 10,180 states and 6,938 large states. Its C source grows from 33,716,905 to 33,776,249 bytes. There is no demonstrated table-size gain; the regenerated parser is not adopted.

The local comparison checkouts were OpenCode `4423f42836`, Codex `806d9732c9`, oh-my-pi `4585e9da4a`, and DeepSeek Harness `687f282e86`. These are observations of those checkouts, not a claim about every released version:

| Project | Kotlin / C# loading | Important distinction |
|---|---|---|
| OpenCode | `packages/tui/src/parsers-config.ts` supplies external WASM URLs to OpenTUI's parser registration | The inspected entry is syntax highlighting. Download/cache internals live in the OpenTUI dependency, which is not installed in this checkout. |
| Codex | TUI highlighting uses a bundled Syntect/two-face syntax database initialized through `OnceLock` | No Kotlin/C# Tree-sitter AST grammar dependencies were found; native Tree-sitter is used for shell parsing. Highlighting is not an AST-navigation replacement. |
| oh-my-pi | Kotlin and C# are external `.wasm.zst` grammars, downloaded on first use, checked by size/SHA-256 and atomically cached | Rust loads each language once; Tree-sitter copies tables to native memory and lexing uses Wasmtime. On unavailable downloads, callers skip those language files and report that fact. Common languages remain statically linked. |
| DeepSeek Harness | Shiki Kotlin/C# highlighting grammars use dynamic imports; precise navigation uses configured external LSP server processes | Shiki grammars and LSP services are separate from native Tree-sitter AST tables. |

AtomCode currently has neither a WebAssembly engine nor this resource-install contract. Tree-sitter's `wasm` feature adds Wasmtime/Cranelift, so moving just two grammars to WASM cannot be assumed to produce a net saving. Cold offline use would also change under a first-download model. Keep Bash safety parsing local and built-in. Before adopting a language-resource design, compare the entire executable plus bundled/cache resources, initialization latency and offline behavior, rather than treating highlighting and AST navigation as interchangeable.
