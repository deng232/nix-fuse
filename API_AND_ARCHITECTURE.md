# Nix Closure Fuser API And Architecture

## Purpose

`nix-closure-fuser` is a read-only FUSE filesystem that exposes a filtered view of selected absolute host paths.

The mounted tree preserves the original absolute path layout. For example, if `/nix/store/aaa-package` is allowed, it appears inside the mount as `MOUNTPOINT/nix/store/aaa-package`.

Current behavior:

- exact allowed files are visible
- allowed directories are visible with their descendants
- symlinks are exposed as symlinks
- unrelated sibling paths are hidden
- writes are rejected
- normal userspace reads are used unless `--passthrough` is requested
- with `--passthrough`, passthrough is attempted first; failures are logged by the daemon and then userspace reads are used

## Public API

The library API is implemented in [src/lib.rs](/home/deng/nix-fuse/src/lib.rs).

### `PathViewOptions`

```rust
pub struct PathViewOptions {
    pub enable_passthrough: bool,
    pub no_exec: bool,
}
```

Fields:

- `enable_passthrough`: requests Linux FUSE fd passthrough during `init()` and `open()`
- `no_exec`: adds `MountOption::NoExec` to the FUSE mount

If `enable_passthrough` is true and `open_backing()` fails, the daemon prints the real error to stderr and falls back to ordinary userspace reads.

### `PathViewFs`

```rust
pub struct PathViewFs { ... }
```

`PathViewFs` is the core filesystem object and implements `fuser::Filesystem`.

Constructors:

```rust
pub fn new(allowed_paths: Vec<PathBuf>) -> io::Result<Self>

pub fn with_options(
    allowed_paths: Vec<PathBuf>,
    options: PathViewOptions,
) -> io::Result<Self>
```

Rules enforced at construction:

- every allowed path must be absolute
- parent path prefixes are created as virtual directories
- real path nodes preserve absolute host path shape inside the mount
- directory roots get `allow_descendants = true`
- regular files and symlinks are exact entries only

Public helpers:

```rust
pub fn attr_for_node(&self, ino: INodeNo) -> io::Result<FileAttr>
pub fn list_dir(&self, ino: INodeNo) -> io::Result<Vec<DirEntry>>
```

These are mostly useful for tests and internal inspection.

### `DirEntry`

```rust
pub struct DirEntry {
    pub ino: INodeNo,
    pub name: OsString,
    pub kind: FileType,
}
```

`DirEntry` represents one visible directory entry returned by `list_dir()` and emitted by `readdir()`.

### `mount_path_view`

```rust
pub fn mount_path_view(
    allowed_paths: Vec<PathBuf>,
    mountpoint: &Path,
    options: PathViewOptions,
) -> anyhow::Result<()>
```

This function:

- creates `PathViewFs`
- applies FUSE mount options
- calls `fuser::mount2`

Mount options always include:

- `MountOption::RO`
- `MountOption::NoSuid`
- `MountOption::NoDev`
- `MountOption::FSName("nix-closure-view")`

`MountOption::NoExec` is added only when `PathViewOptions::no_exec` is true.

`fuser::mount2` blocks until the filesystem is unmounted.

### `load_allowed_paths_from_file`

```rust
pub fn load_allowed_paths_from_file(path: &Path) -> anyhow::Result<Vec<PathBuf>>
```

This loads newline-separated absolute paths from a closure file.

Rules:

- blank lines are ignored
- every non-empty line must be absolute
- relative paths are rejected with an error

### `parse_allowed_paths`

```rust
pub fn parse_allowed_paths(contents: &str, source_name: &str) -> anyhow::Result<Vec<PathBuf>>
```

This parses newline-separated absolute paths from any string source.

It is used by both:

- `load_allowed_paths_from_file`
- the CLI `--paths-stdin` mode

## CLI

The binary wrapper is implemented in [src/main.rs](/home/deng/nix-fuse/src/main.rs).

Usage:

```bash
nix-closure-fuser [--passthrough] [--no-exec] [--paths-file closure.txt | --paths-stdin] <mountpoint> [allowed-path ...]
```

Options:

- `--paths-file <file>`: load allowed paths from a newline-separated file
- `--paths-stdin`: read newline-separated allowed paths from standard input
- `--passthrough`: request FUSE fd passthrough with logged userspace-read fallback
- `--no-exec`: mount with `NoExec`
- `--help`: print usage

Input model:

- first positional argument is the mountpoint
- remaining positional arguments are allowed absolute paths
- at least one allowed path must be provided directly, through `--paths-file`, or through `--paths-stdin`
- `--paths-file` and `--paths-stdin` are mutually exclusive

## FUSE Callbacks

The implemented callbacks are:

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

### `init`

`init()` now performs explicit passthrough setup diagnostics.

It logs:

- whether passthrough was requested
- whether `NoExec` is enabled
- the effective capability mask from `/proc/self/status`
- whether `CAP_SYS_ADMIN` is present
- the kernel-advertised FUSE capabilities from `KernelConfig::capabilities()`

When passthrough is requested, it also:

- calls `config.add_capabilities(InitFlags::FUSE_PASSTHROUGH)`
- logs whether the request succeeded
- calls `config.set_max_stack_depth(1)`
- logs the previous stack depth returned by `fuser`

Expected successful startup lines look like:

```text
init: passthrough_requested=true, no_exec=false
init: capability check: CapEff=0x0000000000200000, CAP_SYS_ADMIN=true
init: kernel capabilities: ...
init: requested FUSE_PASSTHROUGH capability successfully
init: attempting passthrough setup with max_stack_depth=1
init: set_max_stack_depth(1) succeeded, negotiated=0
```

The `negotiated=0` text is the previous stack depth returned by `fuser`, not a failure indicator.

### `lookup`

`lookup(parent, name)` is the visibility gate.

It:

- checks prebuilt children first
- lazily materializes descendants under allowed real directories
- returns `ENOENT` for hidden siblings

### `getattr`

`getattr(ino)` returns attributes for a visible inode.

It:

- returns synthetic `0o555` directory attributes for virtual directories
- uses `symlink_metadata` for real nodes
- preserves symlink file type instead of following symlinks

### `readlink`

`readlink(ino)` returns the backing symlink target bytes.

The target is not rewritten. Absolute symlinks still point to their original absolute target.

### `opendir`, `readdir`, `releasedir`

Directory behavior:

- `opendir` uses a dummy file handle
- `readdir` injects `.` and `..`
- virtual directories list prebuilt children only
- real allowed directories scan the backing directory and materialize visible children lazily
- `releasedir` returns success

### `open`, `read`, `release`

`open()` rejects non-read-only access with `EROFS`.

Without `--passthrough`:

- the backing file is opened with `File::open`
- an `OpenFile` is stored with `backing_id = None`
- `reply.opened(...)` is returned
- `read()` serves bytes with `FileExt::read_at`

With `--passthrough`:

- `open()` calls `reply.open_backing(&file)`
- on success, the returned `BackingId` is stored in `OpenFile`
- `reply.opened_passthrough(...)` is returned
- on failure, the daemon logs the real error and falls back to `reply.opened(...)`
- fallback reads are served by `read()` with `FileExt::read_at`

`release()` removes the `OpenFile`, which also drops any stored `BackingId`.

### `access`

`access()` verifies that the inode is visible.

Write checks return `EROFS`.

### `statfs`

`statfs()` returns simple synthetic filesystem statistics so common tools can complete their checks.

## Internal Architecture

## High-Level Flow

```text
CLI or library caller
    |
    v
allowed absolute paths
    |
    v
PathViewFs::new / PathViewFs::with_options
    |
    v
build virtual parents + real allowed roots
    |
    v
fuser::mount2
    |
    v
FUSE init
    |
    +-- optional FUSE_PASSTHROUGH negotiation
    +-- optional max_stack_depth setup
    +-- capability diagnostics
    |
    v
lookup / getattr / readdir / readlink / open / read
```

## Core Structures

### `PathViewFs`

`PathViewFs` owns the filesystem state:

- `tree: RwLock<TreeState>`
- `next_ino: AtomicU64`
- `open_files: Mutex<HashMap<FileHandle, OpenFile>>`
- `next_fh: AtomicU64`
- `options: PathViewOptions`

Responsibilities:

- maintain stable inodes
- enforce visibility rules
- manage open file handles
- keep passthrough `BackingId` values alive until release

### `TreeState`

`TreeState` is the inode and path index:

- `nodes: HashMap<INodeNo, Node>`
- `children: HashMap<INodeNo, BTreeMap<OsString, INodeNo>>`
- `path_index: HashMap<PathBuf, INodeNo>`

`BTreeMap` is used for child maps so directory listings are deterministic.

### `Node`

```text
Node::VirtualDir
Node::Real
```

`VirtualDir`:

- synthetic path prefix
- examples: `/`, `/home`, `/home/deng`, `/nix`, `/nix/store`

`Real`:

- backed by an actual host path
- stores `virtual_path`, `real_path`, `parent`, and `allow_descendants`

### `OpenFile`

`OpenFile` stores:

- `file: File`
- `backing_id: Option<BackingId>`

`backing_id` is `Some(...)` only after successful passthrough registration.

## Architecture Diagram

```text
                 +-------------------------------+
                 | src/main.rs                    |
                 | CLI args -> PathViewOptions    |
                 +---------------+---------------+
                                 |
                                 v
                 +-------------------------------+
                 | load_allowed_paths_from_file   |
                 | optional closure.txt loader    |
                 +---------------+---------------+
                                 |
                                 v
                 +-------------------------------+
                 | mount_path_view                |
                 | RO/NoSuid/NoDev mount options  |
                 +---------------+---------------+
                                 |
                                 v
        +------------------------------------------------+
        | PathViewFs                                     |
        |------------------------------------------------|
        | tree: RwLock<TreeState>                       |
        | open_files: Mutex<HashMap<FileHandle, ...>>    |
        | options: PathViewOptions                       |
        +-------------------------+----------------------+
                                  |
              +-------------------+-------------------+
              |                                       |
              v                                       v
  +-------------------------+           +-----------------------------+
  | TreeState               |           | OpenFile table              |
  |-------------------------|           |-----------------------------|
  | nodes                   |           | userspace File              |
  | children                |           | optional BackingId          |
  | path_index              |           +-----------------------------+
  +------------+------------+
               |
               v
     +----------------------+
     | Node                 |
     |----------------------|
     | VirtualDir           |
     | Real                 |
     +----------------------+
```

## Visibility Model

Allowed path behavior:

- regular file: expose exactly that file
- directory: expose that directory and descendants
- symlink: expose the symlink itself
- unrelated sibling: hide it

Example:

```text
allowed:
  /nix/store/aaa

visible:
  /nix
  /nix/store
  /nix/store/aaa
  /nix/store/aaa/...

hidden:
  /nix/store/bbb
```

## Lazy Materialization

The filesystem avoids pre-scanning large trees.

Resolution order:

1. check `children[parent][name]`
2. return cached inode if found
3. if parent is a real directory with `allow_descendants = true`, inspect `real_path.join(name)`
4. if the child exists, create and cache a `Real` node
5. otherwise return `ENOENT`

## Passthrough Requirements

Passthrough mode depends on kernel and runtime support.

Required conditions:

- kernel has `CONFIG_FUSE_PASSTHROUGH=y`
- kernel advertises `FUSE_PASSTHROUGH`
- daemon requests `InitFlags::FUSE_PASSTHROUGH`
- daemon configures a valid `max_stack_depth`
- daemon has the required privilege, typically `CAP_SYS_ADMIN`
- backing filesystem stack depth is compatible

Useful checks:

```bash
zgrep -H '^CONFIG_FUSE_PASSTHROUGH=' /proc/config.gz
```

```bash
cat /proc/$(pidof nix-closure-fuser)/status | grep CapEff
```

```bash
stat -f -c %T /absolute/backing/file
```

```bash
findmnt -T /absolute/backing/file
```

## Build Commands

From the repository root:

```bash
cargo check
```

```bash
cargo build
```

```bash
cargo test
```

## Manual Test Commands

### Prepare a mountpoint

```bash
mkdir -p filtered-mnt
```

### Mount a single file

```bash
./target/debug/nix-closure-fuser filtered-mnt "$(pwd)/API_AND_ARCHITECTURE.md"
```

In another shell:

```bash
ls filtered-mnt"$(pwd)"/API_AND_ARCHITECTURE.md
```

```bash
cat filtered-mnt"$(pwd)"/API_AND_ARCHITECTURE.md
```

### Mount a Nix store path

```bash
./target/debug/nix-closure-fuser filtered-mnt /nix/store/aaa-package
```

In another shell:

```bash
ls filtered-mnt/nix/store
```

```bash
ls filtered-mnt/nix/store/aaa-package
```

### Mount from a closure file

Generate closure paths externally:

```bash
nix-store -qR ./result > closure.txt
```

Mount them:

```bash
./target/debug/nix-closure-fuser --paths-file closure.txt filtered-mnt
```

### Mount from stdin

This is the direct pipe workflow for a Nix closure:

```bash
nix-store -qR "$DEVENV_PROFILE" | ./target/debug/nix-closure-fuser --paths-stdin filtered-mnt
```

The input must contain one absolute path per line. Blank lines are ignored.

### Test passthrough

```bash
./target/debug/nix-closure-fuser --passthrough filtered-mnt "$(pwd)/API_AND_ARCHITECTURE.md"
```

Expected successful init diagnostics include:

```text
init: requested FUSE_PASSTHROUGH capability successfully
init: set_max_stack_depth(1) succeeded
```

In another shell:

```bash
cat filtered-mnt"$(pwd)"/API_AND_ARCHITECTURE.md
```

If passthrough open fails, stderr prints:

```text
passthrough open failed for <path>, falling back to userspace read: <real os error>
```

The caller should still be able to read the file through normal FUSE userspace I/O after this fallback.

### Test hidden siblings

If only `/nix/store/aaa-package` was allowed:

```bash
ls filtered-mnt/nix/store/bbb-package
```

Expected result:

```text
No such file or directory
```

### Test symlinks

```bash
readlink filtered-mnt/nix/store/aaa-package/some-link
```

Expected result:

- the same symlink target as the backing filesystem

### Test read-only behavior

These should fail:

```bash
touch filtered-mnt/nix/store/aaa-package/new-file
```

```bash
mkdir filtered-mnt/nix/store/aaa-package/new-dir
```

Expected result:

- read-only filesystem or permission-style error

## Unmounting

Clean unmount:

```bash
fusermount -u filtered-mnt
```

If the mount is stale after an interrupted daemon:

```bash
fusermount -uz filtered-mnt
```

Alternative:

```bash
umount filtered-mnt
```

Lazy unmount fallback:

```bash
umount -l filtered-mnt
```

## Current Limitations

- no write support
- no create, mkdir, unlink, rename, symlink, link, or setattr support
- no automatic signal handler for clean `Ctrl-C` unmount
- no mount namespace setup
- no symlink target rewriting
- no race hardening for mutable arbitrary host paths
- passthrough stack depth is currently fixed at `1`

## Source Map

- library implementation: [src/lib.rs](/home/deng/nix-fuse/src/lib.rs)
- CLI wrapper: [src/main.rs](/home/deng/nix-fuse/src/main.rs)
- design reference: [nix_closure_fuser_path_view_design_nix_dep.md](/home/deng/nix-fuse/nix_closure_fuser_path_view_design_nix_dep.md)
