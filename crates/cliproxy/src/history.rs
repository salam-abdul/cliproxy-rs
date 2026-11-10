//! `cliproxy history delete`: removes selected rows from the model-traffic history
//! (`observability.history`).

use std::path::PathBuf;

use anyhow::Context;
use clap::{Parser, Subcommand};
use cpa_server::request_logging::history::configured_dir;
use cpa_server::request_logging::history::delete::{Selector, delete, parse_time};

#[derive(Parser)]
#[command(name = "history", about = "Manage the model-traffic history (observability.history)")]
struct HistoryArgs {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Delete rows from the history. Needs at least one selector.
    Delete(DeleteArgs),
}

#[derive(clap::Args)]
struct DeleteArgs {
    /// The config file that sets observability.history.dir
    #[arg(long, default_value = "config.yaml")]
    config: PathBuf,
    /// Rows that started at or after this time: YYYY-MM-DD (UTC) or RFC 3339
    #[arg(long)]
    from: Option<String>,
    /// Rows that started before this time: YYYY-MM-DD (UTC) or RFC 3339
    #[arg(long)]
    to: Option<String>,
    /// Rows of this session
    #[arg(long)]
    session: Option<String>,
    /// Rows of this account (an email, or a credential ID)
    #[arg(long)]
    account: Option<String>,
    /// Rows for this upstream model or the model the client asked for
    #[arg(long)]
    model: Option<String>,
    /// Report what would be deleted without deleting anything
    #[arg(long)]
    dry_run: bool,
}

/// Runs `cliproxy history ...` (`args` starts with `history`); returns the exit code.
pub(crate) fn run(args: impl IntoIterator<Item = String>) -> i32 {
    let args = match HistoryArgs::try_parse_from(args) {
        Ok(args) => args,
        Err(error) => {
            let _ = error.print();
            return error.exit_code();
        }
    };
    match execute(args) {
        Ok(report) => {
            println!("{report}");
            0
        }
        Err(error) => {
            eprintln!("history: {error:#}");
            1
        }
    }
}

fn execute(args: HistoryArgs) -> anyhow::Result<String> {
    let Command::Delete(args) = args.command;
    // Parsed, not loaded: loading rewrites a plaintext management key into the file.
    let text = std::fs::read_to_string(&args.config).with_context(|| format!("read {}", args.config.display()))?;
    let config = cpa_core::config::Config::parse(&text).with_context(|| format!("parse {}", args.config.display()))?;
    let dir = configured_dir(&config)
        .context("history is not configured: set observability.history.dir to an absolute path")?;
    let time = |value: &Option<String>| value.as_deref().map(parse_time).transpose();
    let selector = Selector {
        from: time(&args.from)?,
        to: time(&args.to)?,
        session: args.session,
        account: args.account,
        model: args.model,
    };
    let report = delete(&dir, &selector, args.dry_run)?;
    Ok(if args.dry_run {
        format!("dry run: {report}")
    } else {
        report.to_string()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Dir(std::path::PathBuf);
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn config(yaml: &str) -> (Dir, String) {
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = Dir(std::env::temp_dir().join(format!("cliproxy-history-{}-{n}", std::process::id())));
        std::fs::create_dir_all(&dir.0).unwrap();
        let path = dir.0.join("config.yaml");
        std::fs::write(
            &path,
            yaml.replace("{dir}", &dir.0.join("history").display().to_string()),
        )
        .unwrap();
        let path = path.display().to_string();
        (dir, path)
    }

    fn run(args: &[&str]) -> anyhow::Result<String> {
        let args = HistoryArgs::try_parse_from(args.iter().map(|a| a.to_string()))?;
        execute(args)
    }

    #[test]
    fn history_delete_refuses_without_a_selector() {
        let (_dir, path) = config("observability: {history: {enable: true, dir: '{dir}'}}\n");
        let error = run(&["history", "delete", "--config", &path]).unwrap_err();
        assert!(format!("{error:#}").contains("--session"), "{error:#}");
    }

    #[test]
    fn history_delete_requires_a_configured_dir() {
        let (_dir, path) = config("observability: {history: {enable: true}}\n");
        let error = run(&["history", "delete", "--config", &path, "--session", "s"]).unwrap_err();
        assert!(format!("{error:#}").contains("observability.history.dir"), "{error:#}");
    }

    #[test]
    fn history_delete_works_while_recording_is_off() {
        let (_dir, path) = config("observability: {history: {enable: false, dir: '{dir}'}}\n");
        let report = run(&[
            "history",
            "delete",
            "--config",
            &path,
            "--from",
            "2026-01-01",
            "--dry-run",
        ])
        .unwrap();
        assert!(report.starts_with("dry run: 0 row(s)"), "{report}");
    }

    #[test]
    fn history_delete_rejects_a_bad_time() {
        let (_dir, path) = config("observability: {history: {enable: true, dir: '{dir}'}}\n");
        let error = run(&["history", "delete", "--config", &path, "--to", "soon"]).unwrap_err();
        assert!(format!("{error:#}").contains("soon"), "{error:#}");
    }

    #[test]
    fn history_delete_leaves_the_config_file_unchanged() {
        let (_dir, path) = config("management: {secret-key: plain}\nobservability: {history: {dir: '{dir}'}}\n");
        let before = std::fs::read(&path).unwrap();
        run(&["history", "delete", "--config", &path, "--session", "s"]).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }
}
