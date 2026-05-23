use std::io::{self, Read};
use std::path::PathBuf;
use std::process;
use std::{env, fs};

use anyhow::Result;
use nix_closure_fuser::{
    load_allowed_paths_from_file, mount_path_view, mount_path_view_daemonized, parse_allowed_paths,
    PathViewOptions,
};

const USAGE: &str = "usage: nix-closure-fuser [--daemonize] [--daemon-output log.txt] [--passthrough] [--no-exec] [--paths-file closure.txt | --paths-stdin] <mountpoint> [allowed-path ...]";

fn main() {
    if let Err(err) = run() {
        eprintln!("error: {err:#}");
        process::exit(1);
    }
}

fn run() -> Result<()> {
    let mut args = env::args_os().skip(1);
    let mut options = PathViewOptions::default();
    let mut paths_file: Option<PathBuf> = None;
    let mut daemon_output: Option<PathBuf> = None;
    let mut read_paths_stdin = false;
    let mut daemonize = false;
    let mut positionals = Vec::new();

    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--passthrough") => options.enable_passthrough = true,
            Some("--no-exec") => options.no_exec = true,
            Some("--daemonize") => daemonize = true,
            Some("--paths-stdin") => read_paths_stdin = true,
            Some("--daemon-output") => {
                let Some(value) = args.next() else {
                    anyhow::bail!("--daemon-output requires a value\n\n{USAGE}");
                };
                daemon_output = Some(PathBuf::from(value));
            }
            Some("--paths-file") => {
                let Some(value) = args.next() else {
                    anyhow::bail!("--paths-file requires a value\n\n{USAGE}");
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

    if paths_file.is_some() && read_paths_stdin {
        anyhow::bail!("--paths-file and --paths-stdin cannot be used together\n\n{USAGE}");
    }

    if positionals.is_empty() {
        anyhow::bail!("missing mountpoint\n\n{USAGE}");
    }

    let mountpoint = positionals.remove(0);
    let mountpoint = fs::canonicalize(mountpoint)?;
    let mut allowed_paths = if let Some(path_file) = paths_file {
        load_allowed_paths_from_file(&path_file)?
    } else if read_paths_stdin {
        let mut stdin_contents = String::new();
        io::stdin().read_to_string(&mut stdin_contents)?;
        parse_allowed_paths(&stdin_contents, "stdin")?
    } else {
        Vec::new()
    };
    allowed_paths.extend(positionals);

    if allowed_paths.is_empty() {
        anyhow::bail!("provide at least one allowed path, --paths-file, or --paths-stdin\n\n{USAGE}");
    }

    if daemonize {
        let daemon_output =
            daemon_output.unwrap_or(env::current_dir()?.join("nix-closure-fuser.log"));
        mount_path_view_daemonized(allowed_paths, &mountpoint, options, &daemon_output)
    } else if daemon_output.is_some() {
        anyhow::bail!("--daemon-output requires --daemonize\n\n{USAGE}");
    } else {
        mount_path_view(allowed_paths, &mountpoint, options)
    }
}

fn print_usage() {
    eprintln!("{USAGE}");
}
