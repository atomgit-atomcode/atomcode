//! Immutable, session-owned patch imports. Workspace I/O uses the same world as edit_file.
use super::output_artifact::{ArtifactStore, ArtifactStoreProvider};
use super::unified_patch;
use super::{err, ok, resolve_path};
use crate::world::FileSystem;
use async_trait::async_trait;
use atomcode_kernel::tool::{RiskLevel, Tool, ToolContext, ToolResult};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Serialize, Deserialize)]
struct Target {
    path: String,
    before: Vec<u8>,
    after: Vec<u8>,
}
#[derive(Serialize, Deserialize)]
struct Record {
    version: u8,
    root: PathBuf,
    patch: Vec<u8>,
    targets: Vec<Target>,
}

pub struct ImportPatchTool {
    world: Arc<dyn FileSystem>,
    store: ArtifactStoreProvider,
    sensitive: super::SensitivePaths,
}
pub struct ApplyPatchTool {
    world: Arc<dyn FileSystem>,
    store: ArtifactStoreProvider,
}
impl ImportPatchTool {
    pub fn new(
        world: Arc<dyn FileSystem>,
        store: ArtifactStoreProvider,
        sensitive: super::SensitivePaths,
    ) -> Self {
        Self {
            world,
            store,
            sensitive,
        }
    }
}
impl ApplyPatchTool {
    pub fn new(world: Arc<dyn FileSystem>, store: ArtifactStoreProvider) -> Self {
        Self { world, store }
    }
}
fn store(provider: &ArtifactStoreProvider) -> Result<Arc<ArtifactStore>, String> {
    provider().ok_or_else(|| "input artifacts are unavailable for this session".into())
}
/// ENOTSUP (`os error 45` on macOS, `EOPNOTSUPP` on Linux) means the workspace volume
/// cannot do what the patch needs. Say so, and say what to do instead — the bare errno
/// told the reader nothing.
fn with_mount_hint(msg: String) -> String {
    let lower = msg.to_ascii_lowercase();
    if lower.contains("os error 45") || lower.contains("operation not supported") {
        format!(
            "{msg} (the workspace volume does not support the file operation this patch \
needs — retry from a local volume, or edit the file with edit_file)"
        )
    } else {
        msg
    }
}

/// Name the stage and the path an error came from: a bare `os error 45` did not say
/// which call failed.
fn stage_err(stage: &str, rel: &str, e: impl std::fmt::Display) -> String {
    with_mount_hint(format!("{rel}: {stage} failed: {e}"))
}

async fn root(world: &dyn FileSystem, cwd: &Path) -> Result<PathBuf, String> {
    world
        .canonicalize(cwd)
        .await
        .map_err(|e| stage_err("canonicalize working directory", &cwd.display().to_string(), e))
}
async fn target_path(
    world: &dyn FileSystem,
    root: &Path,
    relative: &str,
) -> Result<PathBuf, String> {
    let path = root.join(relative);
    let canonical = world
        .canonicalize(&path)
        .await
        .map_err(|e| stage_err("canonicalize target", relative, e))?;
    if canonical != path || !canonical.starts_with(root) {
        return Err(format!("{relative}: symlink or path escape refused"));
    }
    Ok(path)
}
fn cancelled(ctx: &ToolContext) -> Result<(), String> {
    if ctx.cancel.is_cancelled() {
        Err("cancelled before workspace commit".into())
    } else {
        Ok(())
    }
}

#[async_trait]
impl Tool for ImportPatchTool {
    fn name(&self) -> &str {
        "import_patch"
    }
    fn description(&self) -> &str {
        "Import an explicitly provided UTF-8 unified-diff file without re-emitting its body. Returns a session-owned immutable artifact ID, full SHA-256, and file_paths manifest. Supports exact updates to existing regular text files; additions, deletions, renames, binary patches and mode changes are refused. Use apply_patch with the returned ID and manifest. Never reconstruct a user's large patch with write_file or shell heredocs."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        json!({"type":"object","properties":{"file_path":{"type":"string"}},"required":["file_path"],"additionalProperties":false})
    }
    fn read_only_hint(&self) -> bool {
        true
    }
    async fn execute(&self, args: &str, ctx: &ToolContext) -> ToolResult {
        match self.import(args, ctx).await {
            Ok(value) => ok(value),
            Err(e) => err(format!("import_patch: {e}")),
        }
    }
}
impl ImportPatchTool {
    async fn import(&self, args: &str, ctx: &ToolContext) -> Result<String, String> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Args {
            file_path: String,
        }
        let args: Args = serde_json::from_str(args).map_err(|e| e.to_string())?;
        cancelled(ctx)?;
        let path = resolve_path(&args.file_path, &ctx.working_dir);
        if self
            .world
            .info(&path)
            .await
            .map_err(|e| stage_err("stat patch file", &args.file_path, e))?
            .len
            > 8 * 1024 * 1024
        {
            return Err("patch exceeds 8 MiB import limit".into());
        }
        let bytes = self
            .world
            .read_bytes(&path)
            .await
            .map_err(|e| stage_err("read patch file", &args.file_path, e))?;
        self.import_bytes(bytes, ctx).await
    }

    /// Explicit host input: bytes came directly from a file picker or upload,
    /// not model output. Same validation and artifact namespace as import_patch.
    pub async fn import_bytes(&self, bytes: Vec<u8>, ctx: &ToolContext) -> Result<String, String> {
        let store = store(&self.store)?;
        cancelled(ctx)?;
        if bytes.len() > 8 * 1024 * 1024 {
            return Err("patch exceeds 8 MiB import limit".into());
        }
        if bytes.contains(&0) {
            return Err("binary patch content is unsupported".into());
        }
        let patch = std::str::from_utf8(&bytes).map_err(|_| "patch must be UTF-8")?;
        let parsed = unified_patch::parse(patch)?;
        if parsed.len() > 256 {
            return Err("patch exceeds 256-file limit".into());
        }
        let root = root(&*self.world, &ctx.working_dir).await?;
        let mut targets = Vec::new();
        let mut total = 0usize;
        for file in &parsed {
            cancelled(ctx)?;
            ctx.progress
                .emit(format!("Preflighting patch target: {}", file.path));
            let path = target_path(&*self.world, &root, &file.path).await?;
            if self
                .sensitive
                .references_in(
                    &json!({"file_path":file.path}).to_string(),
                    &root,
                    &*self.world,
                )
                .await
            {
                return Err(format!("{}: sensitive targets cannot be captured in patch artifacts; use explicitly approved per-file edits", file.path));
            }
            let info = self
                .world
                .info(&path)
                .await
                .map_err(|e| stage_err("stat", &file.path, e))?;
            if info.len > 8 * 1024 * 1024 {
                return Err(format!("{}: file exceeds 8 MiB limit", file.path));
            }
            let before = self
                .world
                .read_bytes(&path)
                .await
                .map_err(|e| stage_err("read", &file.path, e))?;
            if before.contains(&0) {
                return Err(format!("{}: binary targets are unsupported", file.path));
            }
            let text = std::str::from_utf8(&before)
                .map_err(|_| format!("{}: only UTF-8 targets are supported", file.path))?;
            let after = unified_patch::apply(file, text)?.into_bytes();
            total = total
                .saturating_add(before.len())
                .saturating_add(after.len());
            if total > 16 * 1024 * 1024 {
                return Err("combined before/after images exceed 16 MiB limit".into());
            }
            targets.push(Target {
                path: file.path.clone(),
                before,
                after,
            });
        }
        let digest = format!("{:x}", Sha256::digest(&bytes));
        let manifest: Vec<_> = targets.iter().map(|t| t.path.clone()).collect();
        let record = Record {
            version: 1,
            root,
            patch: bytes,
            targets,
        };
        let record = serde_json::to_vec(&record).map_err(|e| e.to_string())?;
        if record.len() > 64 * 1024 * 1024 {
            return Err("encoded input record exceeds 64 MiB limit".into());
        }
        let id = store
            .put_input(&record)
            .map_err(|e| stage_err("store input record", "artifact store", e))?;
        Ok(json!({"artifact_id":id,"patch_sha256":digest,"file_paths":manifest,"files":manifest.len(),"hunks":parsed.iter().map(|f| f.hunks.len()).sum::<usize>(),"status":"imported_and_preflighted","next":"apply_patch with this artifact_id and exact file_paths; never output the patch body"}).to_string())
    }
}

#[async_trait]
impl Tool for ApplyPatchTool {
    fn name(&self) -> &str {
        "apply_patch"
    }
    fn description(&self) -> &str {
        "Apply a previously imported immutable patch by artifact_id and its exact file_paths manifest. Do not include patch text. All target contents must still match import-time bytes; otherwise no writes occur. dry_run validates without writes. Already-applied targets are detected on retry; mixed states are refused. Writes use the execution world's filesystem and attempt rollback on failure, reporting uncertain files rather than claiming OS-level atomicity."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        json!({"type":"object","properties":{"artifact_id":{"type":"string"},"file_paths":{"type":"array","items":{"type":"string"},"minItems":1,"maxItems":256},"dry_run":{"type":"boolean"}},"required":["artifact_id","file_paths"],"additionalProperties":false})
    }
    fn risk(&self, _: &str) -> RiskLevel {
        RiskLevel::Risky
    }
    async fn execute(&self, args: &str, ctx: &ToolContext) -> ToolResult {
        match self.apply(args, ctx).await {
            Ok(value) => ok(value),
            Err(e) => err(format!("apply_patch: {e}")),
        }
    }
}
impl ApplyPatchTool {
    async fn apply(&self, args: &str, ctx: &ToolContext) -> Result<String, String> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Args {
            artifact_id: String,
            file_paths: Vec<String>,
            #[serde(default)]
            dry_run: bool,
        }
        let args: Args = serde_json::from_str(args).map_err(|e| e.to_string())?;
        let store = store(&self.store)?;
        let record: Record = serde_json::from_slice(
            &store
                .get_input(&args.artifact_id)
                .map_err(|e| stage_err("read input record", &args.artifact_id, e))?,
        )
        .map_err(|e| e.to_string())?;
        let root = root(&*self.world, &ctx.working_dir).await?;
        if record.version != 1 || record.root != root {
            return Err("artifact belongs to a different workspace or version".into());
        }
        if args.file_paths
            != record
                .targets
                .iter()
                .map(|t| t.path.clone())
                .collect::<Vec<_>>()
        {
            return Err(
                "file_paths does not match the immutable artifact manifest; no files written"
                    .into(),
            );
        }
        // Verify stored plans against the original bytes as well as the record hash.
        let patch = unified_patch::parse(
            std::str::from_utf8(&record.patch).map_err(|_| "invalid patch encoding")?,
        )?;
        if patch.len() != record.targets.len() {
            return Err("invalid artifact plan".into());
        }
        for (file, target) in patch.iter().zip(&record.targets) {
            if file.path != target.path
                || unified_patch::apply(
                    file,
                    std::str::from_utf8(&target.before).map_err(|_| "invalid source encoding")?,
                )?
                .as_bytes()
                    != target.after
            {
                return Err("artifact plan does not match patch".into());
            }
        }
        let mut paths = Vec::new();
        for target in &record.targets {
            paths.push(target_path(&*self.world, &root, &target.path).await?);
        }
        // Same locks as edit_file, sorted to avoid multi-file lock-order deadlocks.
        let mut lock_paths = paths.clone();
        lock_paths.sort();
        lock_paths.dedup();
        if lock_paths.len() != paths.len() {
            return Err("duplicate canonical targets".into());
        }
        let locks: Vec<_> = lock_paths
            .iter()
            .map(|p| super::edit::edit_path_lock(p))
            .collect();
        let mut guards = Vec::new();
        for lock in &locks {
            guards.push(tokio::select! { guard = lock.lock() => guard, _ = ctx.cancel.cancelled() => return Err("cancelled before commit".into()) });
        }
        let mut all_after = true;
        let mut all_before = true;
        let mut conflicts = Vec::new();
        let mut already_applied = Vec::new();
        let mut unchanged = Vec::new();
        for (target, path) in record.targets.iter().zip(&paths) {
            cancelled(ctx)?;
            let current = self
                .world
                .read_bytes(path)
                .await
                .map_err(|e| e.to_string())?;
            all_before &= current == target.before;
            all_after &= current == target.after;
            if current == target.after && current != target.before {
                already_applied.push(target.path.clone());
            } else if current == target.before {
                unchanged.push(target.path.clone());
            }
            if current != target.before && current != target.after {
                let diagnostic = patch
                    .iter()
                    .find(|file| file.path == target.path)
                    .and_then(|file| {
                        std::str::from_utf8(&current)
                            .ok()
                            .and_then(|text| unified_patch::apply(file, text).err())
                    })
                    .unwrap_or_else(|| {
                        format!("{}: contents differ from import-time source", target.path)
                    });
                conflicts.push(diagnostic);
            }
        }
        if all_after {
            return Ok(
                json!({"status":"already_applied","file_paths":args.file_paths}).to_string(),
            );
        }
        if !all_before {
            return Err(json!({"status":"conflict","diagnostics":conflicts,"already_applied":already_applied,"unchanged":unchanged,"file_paths":args.file_paths,"written":[],"message":"targets changed or patch is only partly applied; no files written. Review and re-import; never blindly retry"}).to_string());
        }
        if args.dry_run {
            return Ok(json!({"status":"validated","file_paths":args.file_paths}).to_string());
        }
        for (i, (target, path)) in record.targets.iter().zip(&paths).enumerate() {
            ctx.progress.emit(format!(
                "Applying patch file {}/{}: {}",
                i + 1,
                paths.len(),
                target.path
            ));
            let result = async {
                cancelled(ctx)?;
                target_path(&*self.world, &root, &target.path).await?;
                if self
                    .world
                    .read_bytes(path)
                    .await
                    .map_err(|e| e.to_string())?
                    != target.before
                {
                    return Err("file changed before write".into());
                }
                self.world
                    .write_bytes(path, &target.after)
                    .await
                    .map_err(|e| e.to_string())?;
                if self
                    .world
                    .read_bytes(path)
                    .await
                    .map_err(|e| e.to_string())?
                    != target.after
                {
                    return Err("post-write verification failed".into());
                }
                Ok::<_, String>(())
            }
            .await;
            if let Err(error) = result {
                let mut restored = Vec::new();
                let mut uncertain = Vec::new();
                // Include the failed write: it may have changed bytes before failing.
                for j in (0..=i).rev() {
                    let target = &record.targets[j];
                    let path = &paths[j];
                    if target_path(&*self.world, &root, &target.path)
                        .await
                        .is_err()
                    {
                        uncertain.push(target.path.clone());
                        continue;
                    }
                    let current = self.world.read_bytes(path).await;
                    if current.as_ref().is_ok_and(|b| b == &target.before) {
                        restored.push(target.path.clone());
                        continue;
                    }
                    if current.as_ref().is_ok_and(|b| b == &target.after)
                        && self.world.write_bytes(path, &target.before).await.is_ok()
                        && self
                            .world
                            .read_bytes(path)
                            .await
                            .is_ok_and(|b| b == target.before)
                    {
                        restored.push(target.path.clone());
                    } else {
                        uncertain.push(target.path.clone());
                    }
                }
                return Err(json!({"status":"failed","file":target.path,"error":error,"restored_or_unchanged":restored,"uncertain":uncertain,"not_attempted":record.targets[i+1..].iter().map(|t| &t.path).collect::<Vec<_>>(),"retry":"verify current states first; never blindly reapply"}).to_string());
            }
        }
        Ok(json!({"status":"applied","file_paths":args.file_paths,"patch_sha256":format!("{:x}",Sha256::digest(&record.patch))}).to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::LocalFs;
    use atomcode_kernel::tool::ProgressSink;
    /// A workspace on an SMB/NFS mount fails `canonicalize`/`stat` with ENOTSUP and the
    /// bare errno told the reader nothing. The hint must name the volume problem and a
    /// way forward; unrelated errors must pass through untouched.
    #[test]
    fn mount_unsupported_error_gets_an_actionable_hint() {
        let msg = with_mount_hint("Operation not supported (os error 45)".into());
        assert!(msg.contains("local volume"), "{msg}");
        assert!(msg.contains("edit_file"), "{msg}");
        assert_eq!(with_mount_hint("No such file".into()), "No such file");
    }

    fn context(dir: &Path) -> ToolContext {
        ToolContext {
            working_dir: dir.to_path_buf(),
            cancel: tokio_util::sync::CancellationToken::new(),
            progress: ProgressSink::noop(),
            requester: None,
        }
    }
    #[tokio::test]
    async fn large_patch_import_apply_conflict_manifest_and_retry() {
        let dir = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let store = Arc::new(ArtifactStore::new(storage.path()));
        let provider: ArtifactStoreProvider = {
            let store = store.clone();
            Arc::new(move || Some(store.clone()))
        };
        let world = Arc::new(LocalFs::new(dir.path()));
        let importer = ImportPatchTool::new(
            world.clone(),
            provider.clone(),
            super::super::sensitive_path::test_guard(),
        );
        let applier = ApplyPatchTool::new(world, provider);
        let ctx = context(dir.path());
        let mut patch = String::new();
        let mut before = Vec::new();
        let mut after = Vec::new();
        for file in 0..5 {
            let old = (0..400)
                .map(|n| format!("old-{file}-{n}\n"))
                .collect::<String>();
            let new = (0..400)
                .map(|n| format!("new-{file}-{n}\n"))
                .collect::<String>();
            std::fs::write(dir.path().join(format!("f{file}")), &old).unwrap();
            patch.push_str(&format!("--- a/f{file}\n+++ b/f{file}\n"));
            for hunk in 0..4 {
                patch.push_str(&format!(
                    "@@ -{},100 +{},100 @@\n",
                    hunk * 100 + 1,
                    hunk * 100 + 1
                ));
                for n in hunk * 100..(hunk + 1) * 100 {
                    patch.push_str(&format!("-old-{file}-{n}\n"));
                }
                for n in hunk * 100..(hunk + 1) * 100 {
                    patch.push_str(&format!("+new-{file}-{n}\n"));
                }
            }
            before.push(old);
            after.push(new);
        }
        std::fs::write(dir.path().join("input.patch"), &patch).unwrap();
        let imported = importer
            .execute(r#"{"file_path":"input.patch"}"#, &ctx)
            .await;
        assert!(!imported.is_error, "{}", imported.content);
        let result: serde_json::Value = serde_json::from_str(&imported.content).unwrap();
        assert_eq!(result["hunks"], 20);
        assert_eq!(
            result["patch_sha256"],
            format!("{:x}", Sha256::digest(patch.as_bytes()))
        );
        let args = json!({"artifact_id":result["artifact_id"],"file_paths":result["file_paths"]});
        assert!(
            args.to_string().len() < 1024,
            "tool payload independent of patch size"
        );
        let bad = json!({"artifact_id":result["artifact_id"],"file_paths":["f0"]});
        assert!(applier.execute(&bad.to_string(), &ctx).await.is_error);
        std::fs::write(dir.path().join("f4"), "concurrent change").unwrap();
        let failure = applier.execute(&args.to_string(), &ctx).await;
        assert!(failure.is_error);
        for (i, text) in before.iter().enumerate().take(4) {
            assert_eq!(
                std::fs::read_to_string(dir.path().join(format!("f{i}"))).unwrap(),
                *text
            );
        }
        std::fs::write(dir.path().join("f4"), &before[4]).unwrap();
        std::fs::write(dir.path().join("f0"), &after[0]).unwrap();
        let mixed = applier.execute(&args.to_string(), &ctx).await;
        assert!(mixed.is_error);
        assert!(
            mixed.content.contains(r#""already_applied":["f0"]"#),
            "{}",
            mixed.content
        );
        std::fs::write(dir.path().join("f0"), &before[0]).unwrap();
        let mut dry = args.clone();
        dry["dry_run"] = true.into();
        assert!(!applier.execute(&dry.to_string(), &ctx).await.is_error);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f0")).unwrap(),
            before[0]
        );
        let applied = applier.execute(&args.to_string(), &ctx).await;
        assert!(!applied.is_error, "{}", applied.content);
        for (i, text) in after.iter().enumerate() {
            assert_eq!(
                std::fs::read_to_string(dir.path().join(format!("f{i}"))).unwrap(),
                *text
            );
        }
        let again = applier.execute(&args.to_string(), &ctx).await;
        assert!(!again.is_error);
        assert!(again.content.contains("already_applied"));
        let id = result["artifact_id"].as_str().unwrap();
        std::fs::write(storage.path().join("inputs").join(id), "tampered").unwrap();
        assert!(applier.execute(&args.to_string(), &ctx).await.is_error);
    }
}

#[cfg(test)]
mod failure_tests {
    use super::*;
    use crate::world::{DirEntry, FileInfo, FsError, LocalFs};
    struct Fails {
        inner: LocalFs,
        partial: bool,
        /// When set, `info` fails the way an SMB/NFS mount does (ENOTSUP).
        no_stat: bool,
    }
    #[async_trait]
    impl FileSystem for Fails {
        fn root(&self) -> PathBuf {
            self.inner.root()
        }
        fn describe(&self) -> String {
            "failing fixture".into()
        }
        async fn read_bytes(&self, p: &Path) -> Result<Vec<u8>, FsError> {
            self.inner.read_bytes(p).await
        }
        async fn write_bytes(&self, p: &Path, b: &[u8]) -> Result<(), FsError> {
            if p.file_name().is_some_and(|n| n == "b") {
                if self.partial {
                    self.inner.write_bytes(p, b"partial").await?;
                }
                return Err(FsError::denied("injected write failure"));
            }
            self.inner.write_bytes(p, b).await
        }
        async fn create_dir_all(&self, p: &Path) -> Result<(), FsError> {
            self.inner.create_dir_all(p).await
        }
        async fn list(&self, p: &Path) -> Result<Vec<DirEntry>, FsError> {
            self.inner.list(p).await
        }
        async fn info(&self, p: &Path) -> Result<FileInfo, FsError> {
            if self.no_stat {
                return Err(FsError::denied("Operation not supported (os error 45)"));
            }
            self.inner.info(p).await
        }
        async fn canonicalize(&self, p: &Path) -> Result<PathBuf, FsError> {
            self.inner.canonicalize(p).await
        }
    }
    #[tokio::test]
    async fn write_failures_restore_verified_targets_and_report_partial_writes_as_uncertain() {
        for partial in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let storage = tempfile::tempdir().unwrap();
            let store = Arc::new(ArtifactStore::new(storage.path()));
            let provider: ArtifactStoreProvider = Arc::new(move || Some(store.clone()));
            let world = Arc::new(Fails {
                inner: LocalFs::new(dir.path()),
                partial,
                no_stat: false,
            });
            for name in ["a", "b"] {
                std::fs::write(dir.path().join(name), "old\n").unwrap();
            }
            std::fs::write(dir.path().join("p"), "--- a/a\n+++ b/a\n@@ -1 +1 @@\n-old\n+new\n--- a/b\n+++ b/b\n@@ -1 +1 @@\n-old\n+new\n").unwrap();
            let ctx = ToolContext {
                working_dir: dir.path().to_path_buf(),
                cancel: tokio_util::sync::CancellationToken::new(),
                progress: Default::default(),
                requester: None,
            };
            let imported = ImportPatchTool::new(
                world.clone(),
                provider.clone(),
                super::super::sensitive_path::test_guard(),
            )
            .execute(r#"{"file_path":"p"}"#, &ctx)
            .await;
            assert!(!imported.is_error, "{}", imported.content);
            let imported: serde_json::Value = serde_json::from_str(&imported.content).unwrap();
            let args =
                json!({"artifact_id":imported["artifact_id"],"file_paths":imported["file_paths"]})
                    .to_string();
            let result = ApplyPatchTool::new(world, provider)
                .execute(&args, &ctx)
                .await;
            assert!(result.is_error);
            assert!(result.content.contains("injected write failure"));
            assert_eq!(
                std::fs::read_to_string(dir.path().join("a")).unwrap(),
                "old\n"
            );
            assert_eq!(
                std::fs::read_to_string(dir.path().join("b")).unwrap(),
                if partial { "partial" } else { "old\n" }
            );
            if partial {
                assert!(
                    result.content.contains(r#""uncertain":["b"]"#),
                    "{}",
                    result.content
                );
            }
        }
    }
    #[tokio::test]
    async fn sensitive_targets_are_not_cached_as_readable_input_records() {
        let dir = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".env"), "secret\n").unwrap();
        std::fs::write(
            dir.path().join("p"),
            "--- a/.env\n+++ b/.env\n@@ -1 +1 @@\n-secret\n+replacement\n",
        )
        .unwrap();
        let store = Arc::new(ArtifactStore::new(storage.path()));
        let provider: ArtifactStoreProvider = Arc::new(move || Some(store.clone()));
        let ctx = ToolContext {
            working_dir: dir.path().to_path_buf(),
            cancel: Default::default(),
            progress: Default::default(),
            requester: None,
        };
        let tool = ImportPatchTool::new(
            Arc::new(LocalFs::new(dir.path())),
            provider,
            super::super::sensitive_path::test_guard(),
        );
        let result = tool.execute(r#"{"file_path":"p"}"#, &ctx).await;
        assert!(result.is_error);
        assert!(result.content.contains("sensitive targets"));
        assert_eq!(std::fs::read_dir(storage.path()).unwrap().count(), 0);
        assert_eq!(
            std::fs::read_to_string(dir.path().join(".env")).unwrap(),
            "secret\n"
        );
    }

    /// The live ENOTSUP came from HERE, not from stat/read: writing the input record to
    /// the artifact store. It was a bare `.to_string()`, so the reader saw only the errno.
    #[tokio::test]
    async fn import_names_the_stage_when_the_artifact_store_cannot_write() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a"), "old\n").unwrap();
        std::fs::write(
            dir.path().join("p"),
            "--- a/a\n+++ b/a\n@@ -1 +1 @@\n-old\n+new\n",
        )
        .unwrap();
        // A regular file where the store's directory must be → every write fails.
        let blocked = dir.path().join("blocked");
        std::fs::write(&blocked, b"not a dir").unwrap();
        let store = Arc::new(ArtifactStore::new(blocked.join("inputs")));
        let provider: ArtifactStoreProvider = Arc::new(move || Some(store.clone()));
        let ctx = ToolContext {
            working_dir: dir.path().to_path_buf(),
            cancel: Default::default(),
            progress: Default::default(),
            requester: None,
        };
        let tool = ImportPatchTool::new(
            Arc::new(LocalFs::new(dir.path())),
            provider,
            super::super::sensitive_path::test_guard(),
        );
        let result = tool.execute(r#"{"file_path":"p"}"#, &ctx).await;
        assert!(result.is_error, "{}", result.content);
        assert!(
            result.content.contains("store input record"),
            "must name the failing stage: {}",
            result.content
        );
    }

    /// The smoke run got a bare `import_patch: Operation not supported (os error 45)`:
    /// no stage, no hint. Import must say which step failed and why the volume refuses.
    #[tokio::test]
    async fn import_names_the_failing_stage_and_hints_on_an_unsupported_volume() {
        let dir = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("p"),
            "--- a/a\n+++ b/a\n@@ -1 +1 @@\n-old\n+new\n",
        )
        .unwrap();
        let store = Arc::new(ArtifactStore::new(storage.path()));
        let provider: ArtifactStoreProvider = Arc::new(move || Some(store.clone()));
        let ctx = ToolContext {
            working_dir: dir.path().to_path_buf(),
            cancel: Default::default(),
            progress: Default::default(),
            requester: None,
        };
        let tool = ImportPatchTool::new(
            Arc::new(Fails {
                inner: LocalFs::new(dir.path()),
                partial: false,
                no_stat: true,
            }),
            provider,
            super::super::sensitive_path::test_guard(),
        );
        let result = tool.execute(r#"{"file_path":"p"}"#, &ctx).await;
        assert!(result.is_error, "{}", result.content);
        assert!(
            result.content.contains("stat patch file"),
            "must name the failing stage: {}",
            result.content
        );
        assert!(
            result.content.contains("local volume"),
            "must hint at the unsupported volume: {}",
            result.content
        );
    }
}
