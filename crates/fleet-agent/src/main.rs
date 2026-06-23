//! `fleet-agent` entry point.
//!
//! Usage: `fleet-agent serve --root <path>` — reads JSON-RPC requests from
//! stdin and writes responses to stdout, framed with `Content-Length`.
//!
//! Args are hand-parsed (no `clap`) to keep the injected binary's
//! dependency footprint and cold-start as small as possible.

use std::io::{self, BufReader};
use std::path::PathBuf;
use std::process::ExitCode;

use fleet_agent::Server;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("fleet-agent: {msg}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &[String]) -> Result<(), String> {
    let mut iter = args.iter();
    let command = iter.next().map(String::as_str);
    match command {
        Some("serve") => {
            let root = parse_root(iter)?;
            let server = Server::new(root);
            let stdin = io::stdin();
            let stdout = io::stdout();
            let mut reader = BufReader::new(stdin.lock());
            let mut writer = stdout.lock();
            server
                .serve(&mut reader, &mut writer)
                .map_err(|e| format!("serve loop failed: {e}"))
        }
        Some("--help") | Some("-h") | None => {
            eprintln!("usage: fleet-agent serve --root <path>");
            Ok(())
        }
        Some(other) => Err(format!("unknown command: {other}")),
    }
}

/// Parse the remaining args looking for `--root <path>`, defaulting to the
/// current working directory when omitted.
fn parse_root<'a>(mut iter: impl Iterator<Item = &'a String>) -> Result<PathBuf, String> {
    let mut root: Option<PathBuf> = None;
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--root" => {
                let value = iter.next().ok_or("--root requires a value")?;
                root = Some(PathBuf::from(value));
            }
            other if other.starts_with("--root=") => {
                root = Some(PathBuf::from(&other["--root=".len()..]));
            }
            other => return Err(format!("unexpected argument: {other}")),
        }
    }
    match root {
        Some(r) => Ok(r),
        None => std::env::current_dir().map_err(|e| format!("cannot read cwd: {e}")),
    }
}
