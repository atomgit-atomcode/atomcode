# AtomCode Review — Supported Languages & File Types

This document lists the built-in review rules that `atomcode-review` automatically injects into the reviewer system prompt based on the changed files in a diff.

Rules are matched against the **lowercased base name** of each changed file. Specific file names are checked before generic extension patterns, and the first match wins.

You can override any built-in rule at runtime without rebuilding by placing a file named `<rule>.md` in a directory and passing `--rules-dir <dir>` to the review command.

## Programming Languages

| Language         | Rule       | File Patterns                                       |
| ---------------- | ---------- | -------------------------------------------------- |
| ArkTS            | `arkts`    | `*.ets`                                            |
| C                | `c`        | `*.c`, `*.h`                                       |
| C++              | `cpp`      | `*.cc`, `*.cpp`, `*.cxx`, `*.hpp`                  |
| C#               | `csharp`   | `*.cs`                                             |
| Cangjie (仓颉)   | `cangjie`  | `*.cj`                                             |
| Clojure          | `clojure`  | `*.clj`, `*.cljs`, `*.cljc`                        |
| Dart             | `dart`     | `*.dart`                                           |
| Elixir           | `elixir`   | `*.ex`, `*.exs`                                    |
| Erlang           | `erlang`   | `*.erl`, `*.hrl`                                   |
| Go               | `go`       | `*.go`                                             |
| Groovy           | `groovy`   | `*.groovy`                                         |
| Haskell          | `haskell`  | `*.hs`                                             |
| Java             | `java`     | `*.java`                                           |
| Kotlin           | `kotlin`   | `*.kt`, `*.kts`                                    |
| Lua              | `lua`      | `*.lua`                                            |
| Objective-C      | `objc`     | `*.m`, `*.mm`                                      |
| Perl             | `perl`     | `*.pl`, `*.pm`                                     |
| PHP              | `php`      | `*.php`                                            |
| Python           | `python`   | `*.py`                                             |
| R                | `r`        | `*.r`                                              |
| Ruby             | `ruby`     | `*.rb`                                             |
| Rust             | `rust`     | `*.rs`                                             |
| Scala            | `scala`    | `*.scala`                                          |
| Shell            | `shell`    | `*.sh`, `*.bash`                                   |
| Solidity         | `solidity` | `*.sol`                                            |
| SQL              | `sql`      | `*.sql`                                            |
| Swift            | `swift`    | `*.swift`                                          |
| TypeScript / Web | `ts`       | `*.js`, `*.jsx`, `*.ts`, `*.tsx`, `*.mjs`, `*.vue` |

## Web & Markup

| Category | Rule       | File Patterns                         |
| -------- | ---------- | ------------------------------------- |
| HTML     | `html`     | `*.html`, `*.htm`                     |
| CSS      | `css`      | `*.css`, `*.scss`, `*.sass`, `*.less` |
| GraphQL  | `graphql`  | `*.graphql`, `*.gql`                  |
| Markdown | `markdown` | `readme*.md`, `*.md`, `*.markdown`    |

## Build Tools & Dependency Manifests

| Tool / File          | Rule           | File Patterns                                                             |
| -------------------- | -------------- | ------------------------------------------------------------------------- |
| Gradle build file    | `build_gradle` | `build.gradle`                                                            |
| Maven POM            | `pom_xml`      | `pom.xml`                                                                 |
| npm package manifest | `package_json` | `package.json`                                                            |
| CMake                | `cmake`        | `cmakelists.txt`, `*.cmake`                                               |
| Makefile             | `makefile`     | `makefile`, `gnumakefile`, `*.mk`                                         |
| Dockerfile           | `dockerfile`   | `dockerfile`, `dockerfile.*`, `*.dockerfile`                              |
| Python dependencies  | `python_deps`  | `requirements*.txt`, `pyproject.toml`, `setup.py`, `setup.cfg`, `pipfile` |

## Data, Config & Infrastructure

| Category          | Rule             | File Patterns               |
| ----------------- | ---------------- | --------------------------- |
| JSON              | `json`           | `*.json`, `*.json5`         |
| YAML              | `yaml`           | `*.yaml`, `*.yml`           |
| TOML              | `toml`           | `*.toml`                    |
| XML               | `xml`            | `*.xml`                     |
| Properties        | `properties`     | `*.properties`              |
| Protobuf          | `protobuf`       | `*.proto`                   |
| Terraform         | `terraform`      | `*.tf`, `*.tfvars`          |
| MyBatis / DAO XML | `mapper_dao_xml` | `*mapper*.xml`, `*dao*.xml` |

## How Matching Works

1. The diff is parsed for `+++ b/<path>` lines to discover changed files.
2. Each file's base name is lowercased and checked against the matcher table.
3. The first matching rule is selected for that file.
4. Files are grouped by rule; each rule document is rendered once, with the matching file names listed in scope.

Example: a diff touching both `src/main/java/Foo.java` and `web/App.vue` will inject the `java` and `ts` rules, each applied only to its respective files.

## Runtime Customization

To tune a rule without rebuilding:

```bash
mkdir -p ./my-rules
cp crates/atomcode-review/rules/cangjie.md ./my-rules/cangjie.md
# edit ./my-rules/cangjie.md
atomcode review --rules-dir ./my-rules
```

Only the rules present in the directory are overridden; all other rules continue to use the built-in versions.

## Adding a New Language

To add a new built-in language rule:

1. Create `crates/atomcode-review/rules/<name>.md` with the review checklist.
2. Register the file pattern in `crates/atomcode-review/src/rules.rs` under `MATCHERS`.
3. Register the rule document under `RULE_DOCS` in the same file.
4. Run `cargo test -p atomcode-review` to verify the matcher/doc consistency tests pass.
