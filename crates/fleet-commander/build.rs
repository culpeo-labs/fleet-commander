//! Generates the table of embedded `fleet-agent` binaries consumed by
//! `src/embedded_agent.rs`.
//!
//! With the `embed-agent` feature on (release builds only), it stages the
//! per-arch static-musl binaries — whose paths CI passes via the
//! `FLEET_AGENT_X86_64` / `FLEET_AGENT_AARCH64` env vars — into `OUT_DIR` and
//! emits `include_bytes!` entries for each. With the feature off it emits an
//! empty table so the include always compiles and a plain `cargo build` needs
//! no musl toolchains.

use std::{env, fs, path::PathBuf};

/// (arch slug, env var holding the path to that arch's musl binary).
const ARCHES: &[(&str, &str)] = &[
    ("x86_64", "FLEET_AGENT_X86_64"),
    ("aarch64", "FLEET_AGENT_AARCH64"),
];

fn main() {
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_EMBED_AGENT");

    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR not set"));
    let generated = out_dir.join("embedded_agents.rs");

    if env::var_os("CARGO_FEATURE_EMBED_AGENT").is_none() {
        fs::write(
            &generated,
            "pub static EMBEDDED_AGENTS: &[(&str, &[u8])] = &[];\n",
        )
        .expect("write empty embedded_agents.rs");
        return;
    }

    let mut entries = String::new();
    for (slug, env_key) in ARCHES {
        println!("cargo:rerun-if-env-changed={env_key}");
        let src = env::var_os(env_key).unwrap_or_else(|| {
            panic!(
                "`embed-agent` feature is enabled but {env_key} is unset \
                 (expected a path to the {slug} static-musl fleet-agent binary)"
            )
        });
        let src = PathBuf::from(src);
        println!("cargo:rerun-if-changed={}", src.display());

        if src.is_relative() {
            panic!(
                "{env_key}={} is a relative path; build scripts run with CWD set \
                 to the crate dir, so it must be absolute (e.g. \
                 \"$GITHUB_WORKSPACE/target/.../fleet-agent\")",
                src.display()
            );
        }
        if !src.exists() {
            panic!(
                "{env_key} points at {}, which does not exist — build the {slug} \
                 static-musl fleet-agent binary before the commander",
                src.display()
            );
        }

        // Stage into OUT_DIR so `include_bytes!` references a stable path.
        let staged = out_dir.join(format!("fleet-agent-{slug}"));
        fs::copy(&src, &staged)
            .unwrap_or_else(|e| panic!("staging {} -> {}: {e}", src.display(), staged.display()));

        entries.push_str(&format!(
            "    ({slug:?}, include_bytes!({:?})),\n",
            staged.to_str().expect("OUT_DIR path is valid UTF-8")
        ));
    }

    let body = format!("pub static EMBEDDED_AGENTS: &[(&str, &[u8])] = &[\n{entries}];\n");
    fs::write(&generated, body).expect("write embedded_agents.rs");
}
