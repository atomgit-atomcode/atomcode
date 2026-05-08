//! `atomcode uninstall` subcommand entry point.
//!
//! Spec: docs/superpowers/specs/2026-05-08-uninstall-design.md
//! Plan: docs/superpowers/plans/2026-05-08-uninstall-feature.md

use atomcode_core::uninstall::{
    actions::{PlatformSelfDelete, SelfDeleteStrategy},
    paths::atomcode_dir,
    scan::scan,
    Decisions, ExecuteContext, Group, Outcome,
};
use atomcode_core::self_update::current_exe_path;

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

    let decision_mode = decision_mode(&args, tty);
    let decisions = match decision_mode {
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

    let ctx = build_context(&plan)?;

    let strategy: Box<dyn SelfDeleteStrategy> = Box::new(PlatformSelfDelete);
    let outcome = atomcode_core::uninstall::execute(&plan, final_decisions, strategy.as_ref(), Some(ctx))?;
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
    if args.purge { return DecisionMode::Flag(Decisions::PURGE); }
    if args.keep_data { return DecisionMode::Flag(Decisions::KEEP_DATA); }
    if args.yes { return DecisionMode::Flag(Decisions::DEFAULTS); }
    if args.dry_run { return DecisionMode::Flag(Decisions::DEFAULTS); }
    if !tty { return DecisionMode::AbortNoTty; }
    DecisionMode::Tty
}

// ----- Stubs filled in Task 9 -----

fn print_plan(_plan: &atomcode_core::uninstall::scan::Plan, _decisions: Decisions) {
    println!("DRY RUN — Task 9 will render the plan here.");
}

fn prompt_user(_plan: &atomcode_core::uninstall::scan::Plan) -> anyhow::Result<Option<Decisions>> {
    // Task 9 implements interactive prompts. For now, this is unreachable
    // because `decision_mode` returns `Tty` only when no flag is set, and
    // until Task 9 we should never get here without flags. Keep a safe default.
    Ok(Some(Decisions::DEFAULTS))
}

fn print_summary(_outcome: &Outcome) {
    println!("(summary printer is implemented in Task 9)");
}

fn build_context(_plan: &atomcode_core::uninstall::scan::Plan) -> anyhow::Result<ExecuteContext> {
    Ok(ExecuteContext::default())
}

#[allow(dead_code)]
fn _used_in_task_9(_g: Group) {}
