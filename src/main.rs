use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::Parser;
use manifestdiff::{diff_manifests, parse_manifest, worst_severity, Severity};

/// Diffs two Kubernetes Deployment manifests and flags risky or breaking changes.
#[derive(Parser)]
#[command(name = "manifestdiff", version, about)]
struct Cli {
    /// The "before" manifest.
    before: PathBuf,
    /// The "after" manifest.
    after: PathBuf,

    /// Print machine-readable JSON instead of the text report.
    #[arg(long)]
    json: bool,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(&cli) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("manifestdiff: {err:#}");
            ExitCode::from(2)
        }
    }
}

fn run(cli: &Cli) -> Result<ExitCode> {
    let before_text = std::fs::read_to_string(&cli.before)
        .with_context(|| format!("reading {}", cli.before.display()))?;
    let after_text = std::fs::read_to_string(&cli.after)
        .with_context(|| format!("reading {}", cli.after.display()))?;

    let before = parse_manifest(&before_text)
        .with_context(|| format!("parsing {}", cli.before.display()))?;
    let after =
        parse_manifest(&after_text).with_context(|| format!("parsing {}", cli.after.display()))?;

    let findings = diff_manifests(&before, &after);

    if cli.json {
        let items: Vec<_> = findings
            .iter()
            .map(|f| {
                serde_json::json!({
                    "severity": severity_label(f.severity),
                    "message": f.message,
                })
            })
            .collect();
        let out = serde_json::json!({
            "before": cli.before.display().to_string(),
            "after": cli.after.display().to_string(),
            "worst_severity": worst_severity(&findings).map(severity_label),
            "findings": items,
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
    } else {
        print_text_report(cli, &findings);
    }

    Ok(match worst_severity(&findings) {
        Some(Severity::Breaking) => ExitCode::from(2),
        Some(Severity::Risky) => ExitCode::from(1),
        Some(Severity::Info) | None => ExitCode::SUCCESS,
    })
}

fn severity_label(s: Severity) -> &'static str {
    match s {
        Severity::Info => "info",
        Severity::Risky => "risky",
        Severity::Breaking => "breaking",
    }
}

fn print_text_report(cli: &Cli, findings: &[manifestdiff::Finding]) {
    println!(
        "manifestdiff: {} -> {}\n",
        cli.before.display(),
        cli.after.display()
    );

    if findings.is_empty() {
        println!("no risky or breaking changes detected");
        return;
    }

    for (label, severity) in [
        ("BREAKING CHANGES", Severity::Breaking),
        ("RISKY CHANGES", Severity::Risky),
        ("INFO", Severity::Info),
    ] {
        let matching: Vec<_> = findings.iter().filter(|f| f.severity == severity).collect();
        if matching.is_empty() {
            continue;
        }
        println!("{label}:");
        for f in matching {
            println!("  - {}", f.message);
        }
        println!();
    }
}
