# Just work state, still under review and lack of test cases.

# nix-closure-fuser

`nix-closure-fuser` mounts a read-only FUSE view of selected absolute paths.

It is intended for Nix closures: give it a list of store paths, and the mount exposes only those paths and their descendants while preserving the normal absolute path layout.

Example:

```text
input:
  /nix/store/aaa-package
  /nix/store/bbb-package

mount output:
  filtered-mnt/nix/store/aaa-package
  filtered-mnt/nix/store/bbb-package
```

Unlisted sibling paths are hidden.

## Usage

```bash
nix-closure-fuser [--daemonize] [--daemon-output log.txt] [--passthrough] [--no-exec] [--paths-file closure.txt | --paths-stdin] <mountpoint> [allowed-path ...]
```

Allowed paths must be absolute.

### Direct paths

```bash
nix-closure-fuser filtered-mnt /nix/store/aaa-package /nix/store/bbb-package
```

### Closure file

```bash
nix-closure-fuser --paths-file closure.txt filtered-mnt
```

`closure.txt` should contain one absolute path per line.

### Stdin

```bash
nix-store -qR "$DEVENV_PROFILE" | nix-closure-fuser --paths-stdin filtered-mnt
```

### Daemon mode

```bash
nix-store -qR "$DEVENV_PROFILE" | nix-closure-fuser --daemonize --paths-stdin filtered-mnt
```

Daemon mode mounts first, then forks. The parent prints the child PID to stdout, writes readable status to stderr, and exits immediately while the child inherits the mounted FUSE session and serves the runtime.

The child redirects stdout and stderr to `./nix-closure-fuser.log` by default. Use `--daemon-output` to choose another file:

```bash
nix-store -qR "$DEVENV_PROFILE" | nix-closure-fuser --daemonize --daemon-output fuser.log --paths-stdin filtered-mnt
```

### Passthrough

```bash
nix-store -qR "$DEVENV_PROFILE" | nix-closure-fuser --passthrough --paths-stdin filtered-mnt
```

Passthrough is attempted first. If passthrough open fails, the daemon logs the real error and falls back to normal userspace FUSE reads.

### NoExec

```bash
nix-closure-fuser --no-exec filtered-mnt /nix/store/aaa-package
```

## Implemented Functionality

- Read-only FUSE filesystem.
- Preserves absolute path shape inside the mount.
- Exposes exact allowed regular files.
- Exposes allowed directories and their descendants.
- Exposes symlinks as symlinks.
- Supports `readlink`.
- Hides unlisted sibling paths.
- Lazily materializes directory children.
- Supports path input from positional args, `--paths-file`, and `--paths-stdin`.
- Supports `--daemonize` by forking a child FUSE runtime and printing its PID to stdout.
- Supports child stdout/stderr redirection with `--daemon-output`, defaulting to `./nix-closure-fuser.log`.
- Supports normal userspace file reads.
- Optionally attempts Linux FUSE passthrough.
- Falls back to userspace reads if passthrough open fails.
- Logs passthrough init and open diagnostics.
- Rejects write access as read-only.
- Provides simple `statfs` support.

Implemented FUSE callbacks:

- `init`
- `lookup`
- `getattr`
- `readlink`
- `opendir`
- `readdir`
- `releasedir`
- `open`
- `read`
- `release`
- `access`
- `statfs`

## Not Implemented

- Writes.
- `create`, `mkdir`, `unlink`, `rmdir`, `rename`.
- Symlink or hardlink creation.
- `setattr`, xattrs, fallocate, copy file range.
- Symlink target rewriting.
- Mount namespace setup inside the Rust program.
- Automatic clean unmount on `Ctrl-C`.
