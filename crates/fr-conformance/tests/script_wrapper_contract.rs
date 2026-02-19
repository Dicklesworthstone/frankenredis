#![forbid(unsafe_code)]

use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy)]
struct WrapperSpec {
    script_rel: &'static str,
    expected_cmd_line: &'static str,
}

const WRAPPER_SPECS: [WrapperSpec; 6] = [
    WrapperSpec {
        script_rel: "scripts/run_live_oracle_diff.sh",
        expected_cmd_line: "cmd=(cargo run -p fr-conformance --bin live_oracle_orchestrator -- \"$@\")",
    },
    WrapperSpec {
        script_rel: "scripts/run_adversarial_triage.sh",
        expected_cmd_line: "cmd=(cargo run -p fr-conformance --bin adversarial_triage_orchestrator -- \"$@\")",
    },
    WrapperSpec {
        script_rel: "scripts/run_raptorq_artifact_gate.sh",
        expected_cmd_line: "cmd=(cargo run -p fr-conformance --bin raptorq_artifact_orchestrator -- \"$@\")",
    },
    WrapperSpec {
        script_rel: "scripts/check_coverage_flake_budget.sh",
        expected_cmd_line: "cmd=(cargo run -p fr-conformance --bin live_oracle_budget_orchestrator -- \"$1\")",
    },
    WrapperSpec {
        script_rel: "scripts/benchmark_round1.sh",
        expected_cmd_line: "cmd=(cargo run -p fr-conformance --bin conformance_benchmark_runner -- --round round1 \"$@\")",
    },
    WrapperSpec {
        script_rel: "scripts/benchmark_round2.sh",
        expected_cmd_line: "cmd=(cargo run -p fr-conformance --bin conformance_benchmark_runner -- --round round2 \"$@\")",
    },
];

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crate dir has parent")
        .parent()
        .expect("workspace dir has parent")
        .to_path_buf()
}

fn script_contents(script_rel: &str) -> String {
    let path = repo_root().join(script_rel);
    fs::read_to_string(&path).expect("failed to read script wrapper")
}

#[test]
fn wrappers_delegate_to_expected_binaries() {
    for spec in WRAPPER_SPECS {
        let contents = script_contents(spec.script_rel);
        assert!(
            contents.contains(spec.expected_cmd_line),
            "wrapper {} missing expected command line {}",
            spec.script_rel,
            spec.expected_cmd_line
        );
        assert!(
            contents.contains("\"${cmd[@]}\""),
            "wrapper {} must execute delegated command array",
            spec.script_rel
        );
    }
}

#[test]
fn wrappers_do_not_embed_legacy_orchestration_logic() {
    let banned_snippets = [
        "~/.local/bin/rch exec --",
        "hyperfine \\",
        "strace -c -o",
        "while (($# > 0)); do",
    ];

    for spec in WRAPPER_SPECS {
        let contents = script_contents(spec.script_rel);
        for banned in banned_snippets {
            assert!(
                !contents.contains(banned),
                "wrapper {} should stay thin; found banned snippet: {}",
                spec.script_rel,
                banned
            );
        }
    }
}
