use std::process::Command;
fn main() {
    let output = Command::new("cargo")
        .args(&["run", "-p", "fr-conformance", "--bin", "live_oracle_orchestrator", "--", "--filter", "core_function"])
        .output()
        .unwrap();
    println!("{}", String::from_utf8_lossy(&output.stdout));
    eprintln!("{}", String::from_utf8_lossy(&output.stderr));
}
