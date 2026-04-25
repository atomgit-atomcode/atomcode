//! Fixissue workflow: pull an issue, check it's assigned to the current
//! user, and produce a prompt that the agent can act on.

use anyhow::{anyhow, Result};

use crate::auth;

use super::client::Client;
use super::models::{Comment, Issue};
use super::url::{detect_cwd_atomgit_repo, IssueRef, RepoRef};

/// Label applied to the issue after a successful fixissue run.
pub const FIXED_LABEL: &str = "fixed";

/// Result of prepping a fix-issue run. Either "go, here's the prompt" or
/// "skip, here's why" — callers typically just print the reason and exit 0.
pub enum Prepared {
    Run {
        prompt: String,
        issue_title: String,
        issue_number: u64,
        /// Preserved so the CLI driver can post back a comment + label
        /// when the agent finishes successfully — without re-parsing the URL.
        issue_ref: IssueRef,
    },
    Skip {
        reason: String,
    },
}

/// Post-run side effects on the issue: comment with the agent's repair
/// summary and add the `fixed` label. Both are best-effort from the
/// caller's perspective — on error we return it so the CLI can print
/// a clear message, but the local fix has already been applied.
///
/// Ordering: comment first (the big artifact the user cares about), label
/// second. If the comment succeeds but label add fails, the user at least
/// has the repair record on the issue.
pub fn post_completion(issue_ref: &IssueRef, summary: &str) -> anyhow::Result<()> {
    let client = Client::from_stored_auth()?;
    client.post_issue_comment(issue_ref, summary)?;
    client.add_issue_label(issue_ref, FIXED_LABEL)?;
    Ok(())
}

/// Given an issue URL, resolve it via the AtomGit API and — if it's
/// assigned to the logged-in user — build a prompt that briefs the agent
/// on what to fix.
pub fn prepare(issue_url: &str, working_dir: &std::path::Path) -> Result<Prepared> {
    let r = IssueRef::parse(issue_url)?;

    // Validate cwd's git origin matches the issue's repo so the agent
    // doesn't accidentally patch a different codebase. Failure modes:
    //   * cwd not a git repo / no `origin` / origin on another host:
    //     treat as "can't validate" and proceed (user may intentionally
    //     be reading an issue from outside its repo — still useful for
    //     analysis, plus the prompt carries the repo URL).
    //   * origin IS on atomgit.com but owner/repo don't match:
    //     Skip — this is almost certainly a mistake (wrong cwd).
    let issue_repo = RepoRef::from(&r);
    let mut cwd_hint: Option<RepoRef> = None;
    match detect_cwd_atomgit_repo(working_dir) {
        Ok(Some(cwd_repo)) => {
            if !cwd_repo.matches(&issue_repo) {
                return Ok(Prepared::Skip {
                    reason: format!(
                        "cwd points to {}/{} but the issue is in {}/{}. Skipping — cd to the matching repo first (or pass the right URL).",
                        cwd_repo.owner, cwd_repo.repo, issue_repo.owner, issue_repo.repo
                    ),
                });
            }
            cwd_hint = Some(cwd_repo);
        }
        Ok(None) => {
            // Not a git repo or non-atomgit origin. Fall through.
        }
        Err(_) => {
            // `git` not installed / not on PATH. Same handling as "can't validate".
        }
    }

    let me = auth::current_user()
        .ok_or_else(|| anyhow!("not logged in — run `atomcode login` first"))?;

    let client = Client::from_stored_auth()?;
    let issue = client.get_issue(&r)?;
    // Keep the binding above alive only while it's used; suppress unused-var
    // warning for the cwd-validation-passed case.
    let _ = cwd_hint;

    if !issue.is_assigned_to(&me.username) {
        return Ok(Prepared::Skip {
            reason: format!(
                "issue #{} is assigned to {}, not you ({}). Skipping — fixissue only runs on issues assigned to the current user.",
                issue.number,
                issue.assignee_list(),
                me.username
            ),
        });
    }

    let comments = client.get_issue_comments(&r);
    let prompt = build_prompt(&issue, &comments, working_dir, &r);
    Ok(Prepared::Run {
        prompt,
        issue_title: issue.title.clone(),
        issue_number: issue.number,
        issue_ref: r,
    })
}

fn build_prompt(
    issue: &Issue,
    comments: &[Comment],
    working_dir: &std::path::Path,
    r: &IssueRef,
) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "请分析并修复以下 AtomGit issue，直接在当前本地项目里改代码（不要 commit 或 push，改完即可）。\n\n"
    ));
    out.push_str("---\n");
    out.push_str(&format!("## Issue #{}: {}\n", issue.number, issue.title));
    if let Some(url) = &issue.html_url {
        out.push_str(&format!("URL: {}\n", url));
    } else {
        out.push_str(&format!(
            "URL: https://atomgit.com/{}/{}/issues/{}\n",
            r.owner, r.repo, r.number
        ));
    }
    out.push_str(&format!("状态: {}\n", issue.state));
    if !issue.labels.is_empty() {
        let labels: Vec<&str> = issue.labels.iter().map(|l| l.name.as_str()).collect();
        out.push_str(&format!("标签: {}\n", labels.join(", ")));
    }
    if let Some(reporter) = &issue.user {
        out.push_str(&format!("报告人: {}\n", reporter.login));
    }
    out.push_str(&format!("工作目录: {}\n", working_dir.display()));
    out.push_str("\n### 正文\n\n");
    out.push_str(issue.body.as_deref().unwrap_or("(空)"));
    out.push('\n');

    let with_body: Vec<&Comment> = comments
        .iter()
        .filter(|c| {
            c.body
                .as_deref()
                .map(|b| !b.trim().is_empty())
                .unwrap_or(false)
        })
        .collect();
    if !with_body.is_empty() {
        out.push_str(&format!("\n### 评论 ({})\n", with_body.len()));
        for (i, c) in with_body.iter().enumerate() {
            let author = c
                .user
                .as_ref()
                .map(|u| u.login.as_str())
                .unwrap_or("unknown");
            let body = c.body.as_deref().unwrap_or("");
            out.push_str(&format!("\n**#{} — @{}:**\n{}\n", i + 1, author, body));
        }
    }

    out.push_str("\n---\n\n");
    out.push_str(
        "请按以下步骤处理：\n\
         1. 先总结 issue 的核心问题（一段话）；\n\
         2. 定位代码中相关文件、给出修复方案；\n\
         3. 直接在本地项目里实施修复（可调用工具编辑文件、跑测试）；\n\
         4. 修复完成后用 `## 修复摘要` 为标题，写一段简洁摘要，包含：\n\
            - 改动的文件清单\n\
            - 核心修复逻辑说明\n\
            - 是否通过编译 / 测试\n\
         \n⚠️ 不要执行 git commit / push，只改本地文件。\n\
         \n📝 重要：你在这轮对话里的所有文本回复会被自动发布到上述 issue 的评论区作为修复记录，\
         修复完成后，issue 会被自动打上 `fixed` 标签。请确保输出的内容适合作为正式评论。\n",
    );
    out
}
