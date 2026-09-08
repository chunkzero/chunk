use std::{fs, path::Path};

fn main() {
    let baseline = Path::new("generated-baseline");
    println!("cargo:rerun-if-changed=generated-baseline");
    let model = fs::read_to_string(baseline.join("src/model.rs")).unwrap();
    for (flag, enabled) in [
        ("caller_engine", baseline.join("src/engine.rs").is_file()),
        ("snapshot_host", model.contains("fn get(")),
        ("invocation_context", model.contains("pub timestamp:")),
    ] {
        println!("cargo:rustc-check-cfg=cfg({flag})");
        if enabled {
            println!("cargo:rustc-cfg={flag}");
        }
    }
    let revision = fs::read_to_string(baseline.join("REVISION")).unwrap();
    println!("cargo:rustc-env=BENCH_BASELINE_REVISION={}", revision.trim());
}
