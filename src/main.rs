use std::env;
use std::path::PathBuf;
use std::process;

use anyhow::Result;
use nix_closure_fuser::{load_allowed_paths_from_file, mount_path_view, PathViewOptions};

fn main() {
    if let Err(err) = run() {
        eprintln!("error: {err:#}");
        print_usage();
        process::exit(1);
    }
}

fn run() -> Result<()> {
    let mut args = env::args_os().skip(1);
    let mut options = PathViewOptions::default();
    let mut paths_file: Option<PathBuf> = None;
    let mut positionals = Vec::new();

    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--passthrough") => options.enable_passthrough = true,
            Some("--no-exec") => options.no_exec = true,
            Some("--paths-file") => {
                let Some(value) = args.next() else {
                    anyhow::bail!("--paths-file requires a value");
                };
                paths_file = Some(PathBuf::from(value));
            }
            Some("--help") | Some("-h") => {
                print_usage();
                process::exit(0);
            }
            _ => positionals.push(PathBuf::from(arg)),
        }
    }

    if positionals.is_empty() {
        anyhow::bail!("missing mountpoint");
    }

    let mountpoint = positionals.remove(0);
    let mut allowed_paths = if let Some(path_file) = paths_file {
        load_allowed_paths_from_file(&path_file)?
    } else {
        Vec::new()
    };
    allowed_paths.extend(positionals);

    if allowed_paths.is_empty() {
        anyhow::bail!("provide at least one allowed path or use --paths-file");
    }

    mount_path_view(allowed_paths, &mountpoint, options)
}

fn print_usage() {
    eprintln!(
        "usage: nix-closure-fuser [--passthrough] [--no-exec] [--paths-file closure.txt] <mountpoint> [allowed-path ...]"
    );
}
