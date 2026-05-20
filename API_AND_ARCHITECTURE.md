# Nix Closure Fuser API And Architecture

## Purpose

`nix-closure-fuser` is a read-only FUSE filesystem that exposes a filtered view of a selected set of absolute host paths.

The mounted tree preserves the original absolute path layout. For example, if `/nix/store/aaa-package` is allowed, it appears inside the mount as `MOUNTPOINT/nix/store/aaa-package`.

The implementation is focused on these behaviors:

- expose exact allowed files
- expose allowed directories and all descendants
- expose symlinks as symlinks
- hide unrelated sibling paths
- support normal userspace `read()`
- optionally attempt Linux FUSE fd passthrough and fall back cleanly

## Public API

The crate currently exposes three main public items from [src/lib.rs](/home/deng/nix-fuse/src/lib.rs).

### `PathViewOptions`

```rust
pub struct PathViewOptions {
    pub enable_passthrough: bool,
    pub no_exec: bool,
}
```

Purpose:

- `enable_passthrough`: enable phase 6 fd-passthrough attempts in `open()`
- `no_exec`: add `MountOption::NoExec` to the mount options

Notes:

- `enable_passthrough = true` does not guarantee passthrough will be used
- if passthrough setup fails, the filesystem falls back to ordinary userspace reads
- `no_exec` should stay `false` if you want to execute binaries from the mounted closure

### `PathViewFs`

`PathViewFs` is the core filesystem object implementing `fuser::Filesystem`.

Constructors:

```rust
pub fn new(allowed_paths: Vec<PathBuf>) -> io::Result<Self>
pub fn with_options(
    allowed_paths: Vec<PathBuf>,
    options: PathViewOptions,
) -> io::Result<Self>
```

Behavior:

- every input path must be absolute
- parent directories are synthesized as virtual directories
- real nodes preserve the same absolute path shape in the mounted view
- directory descendants are materialized lazily

Useful public helpers:

```rust
pub fn attr_for_node(&self, ino: INodeNo) -> io::Result<FileAttr>
pub fn list_dir(&self, ino: INodeNo) -> io::Result<Vec<DirEntry>>
```

These are primarily helpful for internal verification and tests.

### `DirEntry`

```rust
pub struct DirEntry {
    pub ino: INodeNo,
    pub name: OsString,
    pub kind: FileType,
}
```

Purpose:

- represents one directory entry in the filtered view
- used by `list_dir()` and then emitted by `readdir()`

### `mount_path_view`

```rust
pub fn mount_path_view(
    allowed_paths: Vec<PathBuf>,
    mountpoint: &Path,
    options: PathViewOptions,
) -> anyhow::Result<()>
```

Purpose:

- build a `PathViewFs`
- configure read-only FUSE mount options
- mount the filesystem using `fuser::mount2`

Important behavior:

- this call blocks until the mount is unmounted
- mount options always include `RO`, `NoSuid`, `NoDev`, and `FSName("nix-closure-view")`
- `NoExec` is added only when requested

### `load_allowed_paths_from_file`

```rust
pub fn load_allowed_paths_from_file(path: &Path) -> anyhow::Result<Vec<PathBuf>>
```

Purpose:

- load newline-separated absolute paths from a text file
- this is the phase 5 workflow entry point for closure files such as `closure.txt`

Validation:

- blank lines are ignored
- every non-empty line must be an absolute path

## CLI Wrapper

The binary entry point is [src/main.rs](/home/deng/nix-fuse/src/main.rs).

Usage:

```bash
nix-closure-fuser [--passthrough] [--no-exec] [--paths-file closure.txt] <mountpoint> [allowed-path ...]
```

Supported options:

- `--paths-file <file>`: load allowed paths from a newline-separated file
- `--passthrough`: enable optional fd-passthrough attempts
- `--no-exec`: mount with `NoExec`

Input rules:

- the first positional argument is the mountpoint
- remaining positional arguments are allowed absolute paths
- you must provide at least one allowed path either directly or through `--paths-file`

## FUSE API Implemented

The filesystem implements these callbacks in [src/lib.rs](/home/deng/nix-fuse/src/lib.rs):

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

### Behavior Summary

#### `lookup`

- resolves a child name under a parent inode
- checks prebuilt children first
- lazily materializes descendants only when the parent is an allowed real directory
- returns `ENOENT` for hidden siblings

#### `getattr`

- returns synthetic directory attributes for virtual directories
- uses `symlink_metadata` for real nodes so symlinks remain symlinks

#### `readlink`

- returns the original symlink target bytes from the backing filesystem
- does not rewrite absolute symlink targets

#### `opendir` / `readdir` / `releasedir`

- directory open uses a dummy handle
- `readdir` injects `.` and `..`
- virtual directories list only prebuilt children
- real allowed directories scan the backing directory and lazily materialize visible descendants

#### `open` / `read` / `release`

- only read-only opens are allowed
- regular file I/O uses a stored file handle
- reads use `FileExt::read_at`
- optional passthrough is attempted in `open()`
- `BackingId` is retained in `OpenFile` until `release()`

#### `access`

- visible nodes pass read and execute style checks
- write checks return `EROFS`

#### `statfs`

- returns simple synthetic filesystem statistics

## Internal Architecture

## High-Level Flow

```text
allowed paths
    ->
PathViewFs::new / with_options
    ->
build stable inode table + parent/child indexes
    ->
mount_path_view
    ->
kernel FUSE requests
    ->
lookup/getattr/readdir/open/read/readlink/access/statfs
```

## Core Structures

### `PathViewFs`

`PathViewFs` owns all mutable filesystem state:

- `tree: RwLock<TreeState)`
- `next_ino: AtomicU64`
- `open_files: Mutex<HashMap<FileHandle, OpenFile>>`
- `next_fh: AtomicU64`
- `options: PathViewOptions`

Responsibilities:

- maintain inode stability
- enforce filtered visibility
- manage open file handles
- coordinate optional passthrough state

### `TreeState`

`TreeState` is the in-memory path graph:

- `nodes: HashMap<INodeNo, Node>`
- `children: HashMap<INodeNo, BTreeMap<OsString, INodeNo>>`
- `path_index: HashMap<PathBuf, INodeNo>`

Purpose:

- `nodes`: inode to node metadata
- `children`: parent inode to sorted child map
- `path_index`: virtual path to inode lookup

### `Node`

Two node classes are used:

```text
VirtualDir
Real
```

`VirtualDir`:

- synthetic directory created to connect absolute path prefixes
- example: `/`, `/nix`, `/nix/store`

`Real`:

- backed by an actual host path
- stores both `virtual_path` and `real_path`
- `allow_descendants = true` means lazy subtree expansion is allowed

### `OpenFile`

`OpenFile` stores:

- `file: File`
- `backing_id: Option<BackingId>`

Purpose:

- support ordinary userspace file reads
- keep passthrough backing registrations alive until `release()`

## Visibility Model

The implementation follows this rule set:

- allowed file: expose exactly that file
- allowed directory: expose that directory and descendants
- allowed symlink: expose the symlink itself only
- unlisted sibling path: hidden

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

The filesystem does not pre-scan entire allowed directory trees.

Resolution path:

1. `lookup(parent, name)` checks `children[parent]`
2. if present, return the known inode
3. otherwise, if `parent` is a real directory with `allow_descendants = true`, inspect `real_path.join(name)`
4. if the child exists, create a new `Real` node and cache it
5. otherwise, return `ENOENT`

This keeps startup cost low for large Nix closures.

## Architecture Diagram

```text
                         +----------------------+
                         |  src/main.rs         |
                         |  minimal CLI wrapper |
                         +----------+-----------+
                                    |
                                    v
                         +----------------------+
                         | load_allowed_paths_  |
                         | from_file()          |
                         +----------+-----------+
                                    |
                                    v
                         +----------------------+
                         | mount_path_view()    |
                         | mount2 + RO options  |
                         +----------+-----------+
                                    |
                                    v
                    +--------------------------------------+
                    | PathViewFs                           |
                    |--------------------------------------|
                    | tree: RwLock<TreeState>             |
                    | next_ino: AtomicU64                 |
                    | open_files: Mutex<HashMap<...>>     |
                    | next_fh: AtomicU64                  |
                    | options: PathViewOptions            |
                    +----------------+---------------------+
                                     |
                    +----------------+----------------+
                    |                                 |
                    v                                 v
         +-------------------------+      +--------------------------+
         | TreeState               |      | OpenFile table           |
         |-------------------------|      |--------------------------|
         | nodes                   |      | file: File               |
         | children                |      | backing_id: BackingId?   |
         | path_index              |      +--------------------------+
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

## Build Commands

Use these commands from the repository root:

```bash
cargo check
```

```bash
cargo build
```

If you want test-only validation for the tree logic:

```bash
cargo test
```

## Manual Test Commands

The filesystem is intended to be validated manually after build.

### 1. Prepare a mountpoint

```bash
mkdir -p filtered-mnt
```

### 2. Run with direct allowed paths

Example with a Nix store path:

```bash
cargo run -- filtered-mnt /nix/store/aaa-package
```

Example with two store paths:

```bash
cargo run -- filtered-mnt /nix/store/aaa-package /nix/store/bbb-package
```

Example with a regular file:

```bash
cargo run -- filtered-mnt /absolute/path/to/file.txt
```

### 3. Run with a closure file

Generate the closure file:

```bash
nix-store -qR ./result > closure.txt
```

Mount from that file:

```bash
cargo run -- --paths-file closure.txt filtered-mnt
```

### 4. Run with passthrough enabled

```bash
cargo run -- --passthrough filtered-mnt /nix/store/aaa-package
```

Expected behavior:

- if the kernel and privileges support passthrough, `open_backing` can be used
- if not, normal userspace reads should still work

### 5. Run with `NoExec`

```bash
cargo run -- --no-exec filtered-mnt /nix/store/aaa-package
```

## Functional Test Commands

Run these commands in another shell while the mount is active.

### Directory visibility

```bash
ls filtered-mnt
```

```bash
ls filtered-mnt/nix
```

```bash
ls filtered-mnt/nix/store
```

Expected result:

- only allowed store roots should appear under `filtered-mnt/nix/store`

### File reads

```bash
cat filtered-mnt/nix/store/aaa-package/some-file
```

Expected result:

- file contents should match the backing file

### Symlink reads

```bash
readlink filtered-mnt/nix/store/aaa-package/some-link
```

Expected result:

- the symlink target should match the original backing symlink target

### Forbidden sibling check

```bash
ls filtered-mnt/nix/store/ccc-package
```

Expected result:

- `No such file or directory`

### File input test

```bash
ls filtered-mnt/absolute/path/to
```

```bash
cat filtered-mnt/absolute/path/to/file.txt
```

Expected result:

- the exact file is visible
- unrelated siblings are not visible unless explicitly allowed

### Read-only enforcement

These commands should fail:

```bash
touch filtered-mnt/nix/store/aaa-package/new-file
```

```bash
mkdir filtered-mnt/nix/store/aaa-package/new-dir
```

Expected result:

- write operations should fail with a read-only filesystem style error

## Unmount Command

Use whichever unmount command is available in your environment:

```bash
fusermount -u filtered-mnt
```

or:

```bash
umount filtered-mnt
```

## Current Limitations

- no write support
- no mkdir, create, unlink, rename, or setattr operations
- no mount namespace integration
- no symlink target rewriting
- no hardening against mutable-path race conditions
- passthrough support depends on kernel support and privilege

## Source Map

- library implementation: [src/lib.rs](/home/deng/nix-fuse/src/lib.rs)
- CLI wrapper: [src/main.rs](/home/deng/nix-fuse/src/main.rs)
- design reference: [nix_closure_fuser_path_view_design_nix_dep.md](/home/deng/nix-fuse/nix_closure_fuser_path_view_design_nix_dep.md)
