use std::process::Command;

fn help(args: &[&str]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_wayback-pg"))
        .args(args)
        .output()
        .expect("run wayback");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("UTF-8 help output")
}

#[test]
fn exposes_only_data_ingest_commands() {
    let output = help(&["--help"]);

    let commands = output
        .split("Commands:\n")
        .nth(1)
        .and_then(|section| section.split("\n\nOptions:").next())
        .expect("commands section");
    assert!(commands.contains("update"));
    assert!(commands.contains("backfill"));
    for removed in [
        "rebuild", "serve", "search", "validate", "export", "convert", "fix",
    ] {
        assert!(
            !commands
                .lines()
                .any(|line| line.trim_start().starts_with(&format!("{removed} "))),
            "obsolete command {removed:?} must not remain in help:\n{commands}"
        );
    }
}

#[test]
fn update_is_incremental_and_backfill_requires_a_range() {
    let update_help = help(&["update", "--help"]);
    assert!(update_help.contains("--pg-config"));
    assert!(update_help.contains("--until"));
    assert!(update_help.contains("--tal"));
    assert!(!update_help.contains("--from"));
    assert!(!update_help.contains("--source"));

    let backfill_help = help(&["backfill", "--help"]);
    assert!(backfill_help.contains("--pg-config"));
    assert!(backfill_help.contains("--from"));
    assert!(backfill_help.contains("--until"));
}

#[test]
fn both_commands_expose_the_data_type_selector() {
    for command in ["update", "backfill"] {
        let command_help = help(&[command, "--help"]);
        assert!(
            command_help.contains("--types"),
            "{command} must expose --types"
        );
        assert!(
            command_help.contains("[default: roa,aspa]"),
            "{command} must ingest both families by default:\n{command_help}"
        );
    }

    // An unknown family is rejected by the parser rather than silently ignored.
    let output = Command::new(env!("CARGO_BIN_EXE_wayback-pg"))
        .args([
            "update",
            "--pg-config",
            "host=/nonexistent",
            "--types",
            "routeviews",
        ])
        .output()
        .expect("run wayback");
    assert!(!output.status.success(), "unknown --types value must fail");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("invalid value"),
        "unexpected stderr: {stderr}"
    );
}
