//! `atomcode uninstall` subcommand entry point.
//!
//! Spec: docs/superpowers/specs/2026-05-08-uninstall-design.md
//! Plan: docs/superpowers/plans/2026-05-08-uninstall-feature.md

use atomcode_core::self_update::current_exe_path;
use super::{
    actions::{PlatformSelfDelete, SelfDeleteStrategy},
    paths::atomcode_dir,
    scan::scan,
    Decisions, ExecuteContext, Group, Outcome,
};

pub struct Args {
    pub yes: bool,
    pub purge: bool,
    pub keep_data: bool,
    pub dry_run: bool,
}

const EXIT_USER_DECLINED: u8 = 1;
const EXIT_BAD_ARGS: u8 = 2;
const EXIT_PARTIAL_FAIL: u8 = 3;
// EXIT_FATAL: u8 = 4 — bubbled up via anyhow::Error and the caller's process exit code.

pub fn run(args: Args) -> anyhow::Result<()> {
    use is_terminal::IsTerminal;
    let tty = std::io::stdin().is_terminal();

    if args.purge && args.keep_data {
        eprintln!("atomcode uninstall: --purge conflicts with --keep-data");
        std::process::exit(EXIT_BAD_ARGS as i32);
    }

    let mode = decision_mode(&args, tty);
    let tty_mode = matches!(mode, DecisionMode::Tty);
    let decisions = match mode {
        DecisionMode::Tty => None,
        DecisionMode::Flag(d) => Some(d),
        DecisionMode::AbortNoTty => {
            eprintln!(
                "atomcode uninstall: refusing to run interactively without a TTY.\n\
                 Pass one of: --yes (use defaults), --purge (delete everything),\n\
                              --keep-data (binary only), --dry-run."
            );
            std::process::exit(EXIT_BAD_ARGS as i32);
        }
    };

    let exe = current_exe_path()?;
    let data_dir = atomcode_dir();
    let plan = scan(&exe, &data_dir)?;

    if args.dry_run {
        print_plan(&plan, decisions.unwrap_or(Decisions::DEFAULTS));
        return Ok(());
    }

    let final_decisions = match decisions {
        Some(d) => d,
        None => match prompt_user(&plan)? {
            Some(d) => d,
            None => std::process::exit(EXIT_USER_DECLINED as i32),
        },
    };

    if !final_decisions.binary {
        eprintln!("atomcode uninstall: cannot uninstall without removing binary; aborted.");
        std::process::exit(EXIT_USER_DECLINED as i32);
    }

    if tty_mode {
        if !confirm_and_kill_running_processes()? {
            eprintln!("aborted: running processes were not terminated.");
            std::process::exit(EXIT_USER_DECLINED as i32);
        }
    }

    let ctx = build_context(&plan)?;

    let strategy: Box<dyn SelfDeleteStrategy> = Box::new(PlatformSelfDelete);
    let outcome =
        super::execute(&plan, final_decisions, strategy.as_ref(), Some(ctx))?;
    print_summary(&outcome);

    if !outcome.failed.is_empty() {
        std::process::exit(EXIT_PARTIAL_FAIL as i32);
    }
    Ok(())
}

enum DecisionMode {
    Tty,
    Flag(Decisions),
    AbortNoTty,
}

fn decision_mode(args: &Args, tty: bool) -> DecisionMode {
    if args.purge {
        return DecisionMode::Flag(Decisions::PURGE);
    }
    if args.keep_data {
        return DecisionMode::Flag(Decisions::KEEP_DATA);
    }
    if args.yes {
        return DecisionMode::Flag(Decisions::DEFAULTS);
    }
    if args.dry_run {
        return DecisionMode::Flag(Decisions::DEFAULTS);
    }
    if !tty {
        return DecisionMode::AbortNoTty;
    }
    DecisionMode::Tty
}

fn confirm_and_kill_running_processes() -> anyhow::Result<bool> {
    use super::actions::{kill_process, list_atomcode_processes};
    use std::io::{BufRead, Write};

    let procs = list_atomcode_processes();
    if procs.is_empty() {
        return Ok(true);
    }

    println!("\nFound {} running atomcode process(es):", procs.len());
    for p in &procs {
        println!("  pid {}  {}", p.pid, p.name);
    }
    print!("Kill them and continue? [y/N]: ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    if !line.trim().eq_ignore_ascii_case("y") {
        return Ok(false);
    }
    for p in procs {
        if let Err(e) = kill_process(p.pid) {
            #[cfg(windows)]
            {
                eprintln!("could not kill pid {}: {}", p.pid, e);
                return Ok(false); // Windows: must succeed or rename will fail
            }
            #[cfg(not(windows))]
            {
                eprintln!(
                    "warn: could not kill pid {}: {} (continuing — Unix unlink doesn't need it)",
                    p.pid, e
                );
            }
        }
    }
    Ok(true)
}

// ----- Task 9 implementations -----

fn print_plan(plan: &super::scan::Plan, decisions: Decisions) {
    println!("DRY RUN — no changes will be made.\n");

    print_group(
        plan,
        Group::Binary,
        "[Group 1] Binary + PATH edit",
        decisions.binary,
    );
    print_group(
        plan,
        Group::Credentials,
        "[Group 2] Credentials and global config",
        decisions.credentials,
    );
    print_group(
        plan,
        Group::State,
        "[Group 3] Local state and extensions",
        decisions.state,
    );
}

fn print_group(
    plan: &super::scan::Plan,
    g: Group,
    label: &str,
    will_remove: bool,
) {
    let items: Vec<_> = plan.items.iter().filter(|i| i.group == g).collect();
    if items.is_empty() {
        return;
    }
    println!(
        "{label}  [{}]",
        if will_remove { "WILL REMOVE" } else { "KEEP" }
    );
    for it in &items {
        let mark = if it.needs_privilege { " (sudo)" } else { "" };
        println!(
            "  {}  ({}){}",
            it.path.display(),
            human_size(it.size_bytes),
            mark
        );
    }
    println!();
}

fn human_size(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{} B", bytes)
    } else {
        format!("{:.1} {}", v, UNITS[u])
    }
}

fn prompt_user(plan: &super::scan::Plan) -> anyhow::Result<Option<Decisions>> {
    use std::io::{BufRead, Write};

    println!("This will uninstall AtomCode from your system.\n");

    let g1 = ask_group(
        plan,
        Group::Binary,
        "[Group 1] Remove binary and PATH edit?",
        true,
    )?;
    if !g1 {
        eprintln!("Group 1 declined; aborting (cannot keep binary while removing data).");
        return Ok(None);
    }
    let g2 = ask_group(
        plan,
        Group::Credentials,
        "[Group 2] Remove credentials and global config?",
        false,
    )?;
    let g3 = ask_group(
        plan,
        Group::State,
        "[Group 3] Remove local state and extensions?",
        true,
    )?;

    println!("\nSummary:");
    summarize_decision(plan, Group::Binary, true);
    summarize_decision(plan, Group::Credentials, g2);
    summarize_decision(plan, Group::State, g3);

    print!("\nContinue? [y/N]: ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    if !line.trim().eq_ignore_ascii_case("y") {
        return Ok(None);
    }
    Ok(Some(Decisions {
        binary: true,
        credentials: g2,
        state: g3,
    }))
}

fn ask_group(
    plan: &super::scan::Plan,
    g: Group,
    prompt: &str,
    default_yes: bool,
) -> anyhow::Result<bool> {
    use std::io::{BufRead, Write};
    let items: Vec<_> = plan.items.iter().filter(|i| i.group == g).collect();
    if items.is_empty() {
        return Ok(default_yes);
    }
    println!("\n{prompt}");
    for it in &items {
        let mark = if it.needs_privilege { " (sudo)" } else { "" };
        println!(
            "  {}  ({}){}",
            it.path.display(),
            human_size(it.size_bytes),
            mark
        );
    }
    let prompt_suffix = if default_yes { "[Y/n]" } else { "[y/N]" };
    print!("Proceed? {prompt_suffix}: ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    let answer = line.trim();
    Ok(match answer {
        "" => default_yes,
        s if s.eq_ignore_ascii_case("y") || s.eq_ignore_ascii_case("yes") => true,
        s if s.eq_ignore_ascii_case("n") || s.eq_ignore_ascii_case("no") => false,
        _ => default_yes,
    })
}

fn summarize_decision(plan: &super::scan::Plan, g: Group, will_remove: bool) {
    let count = plan.items.iter().filter(|i| i.group == g).count();
    if count == 0 {
        return;
    }
    let action = if will_remove { "Remove" } else { "Keep" };
    let label = match g {
        Group::Binary => "binary + PATH",
        Group::Credentials => "credentials",
        Group::State => "local state",
    };
    println!("  {action}: {count} items ({label})");
}

fn print_summary(outcome: &Outcome) {
    println!("\n──────────────────");
    if !outcome.removed.is_empty() {
        println!("Removed:");
        for p in &outcome.removed {
            println!("  {}", p.display());
        }
    }
    if !outcome.kept.is_empty() {
        println!("Kept (use --purge to remove later):");
        for p in &outcome.kept {
            println!("  {}", p.display());
        }
    }
    if !outcome.failed.is_empty() {
        println!("Failed:");
        for (p, e) in &outcome.failed {
            println!("  {}  ({})", p.display(), e);
        }
    }
    if !outcome.backups.is_empty() {
        println!("Backups:");
        for p in &outcome.backups {
            println!("  {}", p.display());
        }
    }
}

fn build_context(plan: &super::scan::Plan) -> anyhow::Result<ExecuteContext> {
    // `mut` is only used by the `#[cfg(unix)]` branch below — Windows
    // builds compile this as a never-mutated `Vec`. Suppress the lint
    // there rather than duplicate the let with cfg gates.
    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut rc_files = Vec::new();
    #[cfg(unix)]
    {
        let prefix = plan
            .binary_path
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        let rc = super::paths::unix_rc_paths();
        for path in [rc.zshrc, rc.bashrc] {
            if path.exists() {
                rc_files.push((path, prefix.clone()));
            }
        }
    }
    let _ = plan; // suppress unused-var warning on non-unix builds
    let ctx = ExecuteContext {
        rc_files,
        #[cfg(windows)]
        windows_install_dir_literal: plan
            .binary_path
            .parent()
            .map(|p| p.to_string_lossy().into_owned()),
        #[cfg(windows)]
        windows_install_dir_expanded: plan
            .binary_path
            .parent()
            .map(|p| p.to_string_lossy().into_owned()),
    };
    Ok(ctx)
}
