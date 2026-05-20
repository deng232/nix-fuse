# Nix Closure Path View with Rust `fuser`

## Purpose

Build a small Rust filesystem that mounts a **read-only filtered view** from a given list of real paths.

Primary target:

```text
given Nix closure paths
  -> create a mount
  -> mounted tree exposes only those paths and their descendants
  -> no unrelated host paths are visible
```

Example input:

```text
/nix/store/aaa-package
/nix/store/bbb-package
/home/me/some-file
```

Example mountpoint:

```text
./filtered-mnt
```

Example visible result:

```text
filtered-mnt/
  nix/
    store/
      aaa-package/
      bbb-package/
  home/
    me/
      some-file
```

This design intentionally starts with a **read-only userspace FUSE prototype**. Kernel fd passthrough is optional and should be added only after the normal `lookup`, `getattr`, `readdir`, `readlink`, `open`, and `read` path works.

## Background facts

`fuser` is a Rust FUSE library exposing the low-level `Filesystem` trait. Current `fuser` has typed APIs such as `INodeNo`, `FileHandle`, `OpenFlags`, and reply types like `ReplyEntry`, `ReplyAttr`, `ReplyDirectory`, `ReplyOpen`, and `ReplyData`.

`fuser` also has Linux FUSE fd-passthrough support through:

```rust
ReplyOpen::open_backing(fd) -> Result<BackingId>
ReplyOpen::opened_passthrough(fh, flags, &backing_id)
```

The `BackingId` must live long enough. Do not create it inside `open()` and immediately drop it. Store it in filesystem state and drop it during `release()`.

Kernel FUSE passthrough itself works by registering a backing file descriptor with the kernel, receiving a `backing_id`, then replying to `OPEN` with `FOPEN_PASSTHROUGH`. It is useful for file data I/O, but metadata/path operations still need the daemon.

Important limitation: kernel passthrough currently requires kernel support and privilege. For rootless use, assume passthrough may fail and keep normal userspace `read()` working.

Useful docs:

- `fuser::Filesystem`: https://docs.rs/fuser/latest/fuser/trait.Filesystem.html
- `fuser::ReplyOpen`: https://docs.rs/fuser/latest/fuser/struct.ReplyOpen.html
- `fuser::BackingId`: https://docs.rs/fuser/latest/fuser/struct.BackingId.html
- `fuser::KernelConfig`: https://docs.rs/fuser/latest/fuser/struct.KernelConfig.html
- Linux FUSE passthrough: https://docs.kernel.org/filesystems/fuse/fuse-passthrough.html
- Linux FUSE kernel interface overview: https://man7.org/linux/man-pages/man4/fuse.4.html

## Main design rule

Do **not** start by writing a general CLI or full container runtime.

Start with a library-like filesystem object:

```rust
let fs = PathViewFs::new(allowed_paths)?;
fuser::mount2(fs, mountpoint, &options)?;
```

The first success condition is:

```bash
ls ./filtered-mnt/nix/store
cat ./filtered-mnt/nix/store/<allowed-path>/some-file
readlink ./filtered-mnt/nix/store/<allowed-path>/some-link
```

## Non-goals for the first prototype

Do not implement these first:

```text
write
create
mkdir
mknod
unlink
rmdir
rename
symlink
link
setattr
setxattr
removexattr
fallocate
copy_file_range
full CLI flag parser
container namespace setup
OverlayFS integration
ComposeFS integration
```

The first filesystem is read-only.

## Visibility model

Input path type matters:

```text
input is regular file:
    expose exactly that file

input is directory:
    expose that directory and all descendants

input is symlink:
    expose the symlink itself first
    do not recursively expose the symlink target unless that target is also allowed
```

For Nix closure paths, most inputs are store path directories:

```text
/nix/store/<hash>-name
```

Those should be treated as allowed directory roots.

## Virtual path policy

### First version: preserve absolute path shape

Real path:

```text
/nix/store/aaa-package
```

Virtual path inside mount:

```text
/nix/store/aaa-package
```

Mounted at `./filtered-mnt`, this appears as:

```text
./filtered-mnt/nix/store/aaa-package
```

This is easiest to debug because the mounted tree mirrors host paths.

### Later option: store-root mode

For a Nix-only view, we may instead mount only store basenames:

```text
./filtered-mnt/aaa-package
./filtered-mnt/bbb-package
```

Do not start here. Absolute symlinks inside Nix store usually reference `/nix/store/...`, so preserving `/nix/store` shape is less surprising for early testing.

## Important symlink note

If the mountpoint is only `./filtered-mnt`, an absolute symlink like this:

```text
/nix/store/xxx/lib/libfoo.so
```

still points to host `/nix/store/xxx/lib/libfoo.so`, not to:

```text
./filtered-mnt/nix/store/xxx/lib/libfoo.so
```

For actual container use, the filtered mount should be placed inside a mount namespace so `/nix/store` resolves to the filtered view. Another option is to rewrite absolute symlink targets in `readlink()`, but that changes semantics and should not be done in the first prototype.

## Core data structures

Use stable inode numbers internally. The kernel will repeatedly ask about inodes, so we need a table.

```rust
use std::{
    collections::{BTreeMap, HashMap},
    ffi::{OsStr, OsString},
    fs::File,
    io,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex, RwLock,
    },
};

use fuser::{BackingId, FileHandle, FileType, INodeNo};

struct PathViewFs {
    nodes: RwLock<HashMap<INodeNo, Node>>,

    // parent inode -> child name -> child inode
    children: RwLock<HashMap<INodeNo, BTreeMap<OsString, INodeNo>>>,

    next_ino: AtomicU64,

    open_files: Mutex<HashMap<FileHandle, OpenFile>>,
    next_fh: AtomicU64,
}

enum Node {
    // Synthetic directories created so that the real allowed path has parents.
    // Example: /, /nix, /nix/store
    VirtualDir {
        virtual_path: PathBuf,
    },

    // Node backed by a real host path.
    Real {
        virtual_path: PathBuf,
        real_path: PathBuf,

        // true for allowed directory roots.
        // false for exact file/symlink entries.
        allow_descendants: bool,
    },
}

struct OpenFile {
    file: File,

    // Only Some when kernel fd-passthrough succeeded.
    // Must be dropped during release(), not immediately after open().
    backing_id: Option<BackingId>,
}

struct DirEntry {
    ino: INodeNo,
    name: OsString,
    kind: FileType,
}
```

Root inode should be a constant:

```rust
const ROOT_INO: INodeNo = INodeNo::new(1);
```

If `INodeNo::new()` is not available in the exact `fuser` version, use that version's documented constructor or conversion method. Do not guess; check docs and compile errors.

## Tree-building phase

This happens before mounting.

Required functions:

```rust
impl PathViewFs {
    pub fn new(allowed_paths: Vec<PathBuf>) -> io::Result<Self>;

    fn insert_allowed_path(&mut self, real_path: PathBuf) -> io::Result<INodeNo>;

    fn ensure_virtual_dir(&mut self, virtual_path: &Path) -> io::Result<INodeNo>;

    fn insert_real_node(
        &mut self,
        virtual_path: PathBuf,
        real_path: PathBuf,
        allow_descendants: bool,
    ) -> io::Result<INodeNo>;

    fn alloc_ino(&self) -> INodeNo;

    fn add_child(
        &mut self,
        parent: INodeNo,
        name: OsString,
        child: INodeNo,
    ) -> io::Result<()>;
}
```

### `new`

Pseudocode:

```rust
pub fn new(allowed_paths: Vec<PathBuf>) -> io::Result<Self> {
    let mut fs = Self::empty_with_root();

    for path in allowed_paths {
        fs.insert_allowed_path(path)?;
    }

    Ok(fs)
}
```

### `insert_allowed_path`

Given:

```text
/nix/store/aaa-package
```

Create virtual parents:

```text
/
 /nix
 /nix/store
```

Then create a real node:

```text
/nix/store/aaa-package -> /nix/store/aaa-package
```

Pseudocode:

```rust
fn insert_allowed_path(&mut self, real_path: PathBuf) -> io::Result<INodeNo> {
    let meta = std::fs::symlink_metadata(&real_path)?;

    let virtual_path = real_path.clone();

    // Create all parent virtual directories.
    if let Some(parent) = virtual_path.parent() {
        self.ensure_virtual_dir(parent)?;
    }

    let allow_descendants = meta.is_dir();

    self.insert_real_node(
        virtual_path,
        real_path,
        allow_descendants,
    )
}
```

### `ensure_virtual_dir`

This should create synthetic directories for path prefixes.

Example input:

```text
/nix/store
```

Required result:

```text
/
  nix/
    store/
```

Rules:

```text
if virtual dir already exists:
    return existing inode

else:
    recursively ensure parent
    allocate inode
    insert Node::VirtualDir
    add to parent children map
```

## Lookup and lazy child expansion

The most important helper is `resolve_child`.

```rust
impl PathViewFs {
    fn resolve_child(&self, parent: INodeNo, name: &OsStr) -> io::Result<INodeNo>;

    fn maybe_materialize_real_child(
        &self,
        parent: INodeNo,
        name: &OsStr,
    ) -> io::Result<INodeNo>;
}
```

Resolution order:

1. Check prebuilt `children[parent][name]`.
2. If found, return it.
3. If parent is a `Real` directory with `allow_descendants = true`, check whether `real_path.join(name)` exists.
4. If it exists, create a new `Real` child node lazily.
5. Otherwise return `ENOENT`.

This lets us avoid pre-scanning huge Nix store trees.

Example:

```text
allowed root:
/nix/store/aaa-package

lookup sequence:
lookup("/", "nix")                       -> synthetic /nix
lookup("/nix", "store")                  -> synthetic /nix/store
lookup("/nix/store", "aaa-package")      -> real allowed root
lookup("/nix/store/aaa-package", "bin")  -> lazy real child
lookup("/nix/store/aaa-package/bin", "x")-> lazy real child
```

Forbidden sibling example:

```text
allowed root:
/nix/store/aaa-package

lookup("/nix/store", "bbb-package") -> ENOENT
```

Unless `/nix/store/bbb-package` was also in the allowed input list.

## Attribute conversion

Required function:

```rust
fn attr_for_node(&self, ino: INodeNo) -> io::Result<fuser::FileAttr>;
```

Behavior:

```text
VirtualDir:
    return fake directory attrs
    kind = FileType::Directory
    perm = 0o555
    nlink = 2

Real:
    use symlink_metadata(real_path)
    convert file type, size, mode, uid, gid, times
```

Use `symlink_metadata`, not `metadata`, so symlinks remain symlinks.

For a first prototype, it is acceptable to simplify timestamps and block counts as long as normal tools work.

## Directory listing

Required function:

```rust
fn list_dir(&self, ino: INodeNo) -> io::Result<Vec<DirEntry>>;
```

Behavior:

```text
VirtualDir:
    list prebuilt children only

Real dir with allow_descendants = true:
    list real directory entries from backing path
    but materialize each child as a Real node
    return only those children

Real file:
    ENOTDIR

Real symlink:
    ENOTDIR
```

For first prototype, include `.` and `..` entries if needed by `fuser` behavior. Many examples manually add them in `readdir`.

## FUSE callbacks to implement

Implement these first:

```rust
impl fuser::Filesystem for PathViewFs {
    fn lookup(
        &self,
        req: &fuser::Request<'_>,
        parent: fuser::INodeNo,
        name: &OsStr,
        reply: fuser::ReplyEntry,
    );

    fn getattr(
        &self,
        req: &fuser::Request<'_>,
        ino: fuser::INodeNo,
        fh: Option<fuser::FileHandle>,
        reply: fuser::ReplyAttr,
    );

    fn readlink(
        &self,
        req: &fuser::Request<'_>,
        ino: fuser::INodeNo,
        reply: fuser::ReplyData,
    );

    fn opendir(
        &self,
        req: &fuser::Request<'_>,
        ino: fuser::INodeNo,
        flags: fuser::OpenFlags,
        reply: fuser::ReplyOpen,
    );

    fn readdir(
        &self,
        req: &fuser::Request<'_>,
        ino: fuser::INodeNo,
        fh: fuser::FileHandle,
        offset: u64,
        reply: fuser::ReplyDirectory,
    );

    fn releasedir(
        &self,
        req: &fuser::Request<'_>,
        ino: fuser::INodeNo,
        fh: fuser::FileHandle,
        flags: fuser::OpenFlags,
        reply: fuser::ReplyEmpty,
    );

    fn open(
        &self,
        req: &fuser::Request<'_>,
        ino: fuser::INodeNo,
        flags: fuser::OpenFlags,
        reply: fuser::ReplyOpen,
    );

    fn read(
        &self,
        req: &fuser::Request<'_>,
        ino: fuser::INodeNo,
        fh: fuser::FileHandle,
        offset: u64,
        size: u32,
        flags: fuser::OpenFlags,
        lock_owner: Option<fuser::LockOwner>,
        reply: fuser::ReplyData,
    );

    fn release(
        &self,
        req: &fuser::Request<'_>,
        ino: fuser::INodeNo,
        fh: fuser::FileHandle,
        flags: fuser::OpenFlags,
        lock_owner: Option<fuser::LockOwner>,
        flush: bool,
        reply: fuser::ReplyEmpty,
    );

    fn access(
        &self,
        req: &fuser::Request<'_>,
        ino: fuser::INodeNo,
        mask: fuser::AccessFlags,
        reply: fuser::ReplyEmpty,
    );

    fn statfs(
        &self,
        req: &fuser::Request<'_>,
        ino: fuser::INodeNo,
        reply: fuser::ReplyStatfs,
    );
}
```

Check the exact signatures against the `fuser` version in `Cargo.lock`. The design assumes `fuser` 0.17-style typed arguments.

## Callback behavior

### `lookup`

Purpose:

```text
parent inode + child name -> child inode + attrs
```

Pseudocode:

```rust
fn lookup(&self, _req: &Request<'_>, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
    match self.resolve_child(parent, name)
        .and_then(|ino| self.attr_for_node(ino).map(|attr| (ino, attr)))
    {
        Ok((_ino, attr)) => {
            reply.entry(&TTL, &attr, Generation(0));
        }
        Err(err) => {
            reply.error(errno_from_io(err));
        }
    }
}
```

`lookup` is the main visibility gate. If a child is not allowed, return `ENOENT`.

### `getattr`

Purpose:

```text
inode -> attrs
```

Pseudocode:

```rust
fn getattr(&self, _req: &Request<'_>, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
    match self.attr_for_node(ino) {
        Ok(attr) => reply.attr(&TTL, &attr),
        Err(err) => reply.error(errno_from_io(err)),
    }
}
```

### `readlink`

Purpose:

```text
return symlink target bytes
```

Pseudocode:

```rust
fn readlink(&self, _req: &Request<'_>, ino: INodeNo, reply: ReplyData) {
    let real_path = match self.real_path_for_symlink(ino) {
        Ok(path) => path,
        Err(err) => {
            reply.error(errno_from_io(err));
            return;
        }
    };

    match std::fs::read_link(real_path) {
        Ok(target) => {
            use std::os::unix::ffi::OsStrExt;
            reply.data(target.as_os_str().as_bytes());
        }
        Err(err) => reply.error(errno_from_io(err)),
    }
}
```

Do not rewrite symlinks in the first prototype.

### `opendir`

Purpose:

```text
open a directory handle
```

First prototype can return a dummy handle.

```rust
fn opendir(&self, _req: &Request<'_>, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
    if self.is_dir(ino) {
        reply.opened(FileHandle::new(0), FopenFlags::empty());
    } else {
        reply.error(Errno::ENOTDIR);
    }
}
```

If `FileHandle::new()` does not exist in the exact `fuser` version, use the documented constructor/conversion.

### `readdir`

Purpose:

```text
list visible children only
```

Pseudocode:

```rust
fn readdir(
    &self,
    _req: &Request<'_>,
    ino: INodeNo,
    _fh: FileHandle,
    offset: u64,
    mut reply: ReplyDirectory,
) {
    let entries = match self.list_dir(ino) {
        Ok(entries) => entries,
        Err(err) => {
            reply.error(errno_from_io(err));
            return;
        }
    };

    for (i, entry) in entries.into_iter().enumerate().skip(offset as usize) {
        let next_offset = (i + 1) as i64;

        if reply.add(entry.ino, next_offset, entry.kind, entry.name) {
            break;
        }
    }

    reply.ok();
}
```

### `releasedir`

Purpose:

```text
clean up directory handle state
```

If using dummy handles, just return ok.

```rust
fn releasedir(
    &self,
    _req: &Request<'_>,
    _ino: INodeNo,
    _fh: FileHandle,
    _flags: OpenFlags,
    reply: ReplyEmpty,
) {
    reply.ok();
}
```

### `open`

Purpose:

```text
open real file backing an inode
```

First version: userspace read only.

```rust
fn open(&self, _req: &Request<'_>, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
    let real_path = match self.real_path_for_regular_file(ino) {
        Ok(path) => path,
        Err(err) => {
            reply.error(errno_from_io(err));
            return;
        }
    };

    let file = match File::open(real_path) {
        Ok(file) => file,
        Err(err) => {
            reply.error(errno_from_io(err));
            return;
        }
    };

    let fh = self.alloc_fh();

    self.open_files.lock().unwrap().insert(fh, OpenFile {
        file,
        backing_id: None,
    });

    reply.opened(fh, FopenFlags::empty());
}
```

### `read`

Purpose:

```text
read bytes from opened file handle
```

Use `FileExt::read_at` on Unix.

```rust
fn read(
    &self,
    _req: &Request<'_>,
    _ino: INodeNo,
    fh: FileHandle,
    offset: u64,
    size: u32,
    _flags: OpenFlags,
    _lock_owner: Option<LockOwner>,
    reply: ReplyData,
) {
    use std::os::unix::fs::FileExt;

    let mut buf = vec![0_u8; size as usize];

    let open_files = self.open_files.lock().unwrap();
    let Some(open) = open_files.get(&fh) else {
        reply.error(Errno::EBADF);
        return;
    };

    match open.file.read_at(&mut buf, offset) {
        Ok(n) => reply.data(&buf[..n]),
        Err(err) => reply.error(errno_from_io(err)),
    }
}
```

### `release`

Purpose:

```text
drop open file and optional passthrough BackingId
```

```rust
fn release(
    &self,
    _req: &Request<'_>,
    _ino: INodeNo,
    fh: FileHandle,
    _flags: OpenFlags,
    _lock_owner: Option<LockOwner>,
    _flush: bool,
    reply: ReplyEmpty,
) {
    self.open_files.lock().unwrap().remove(&fh);
    reply.ok();
}
```

### `access`

Purpose:

```text
permission check
```

For first prototype, deny writes and allow reads/execs according to real mode.

Rules:

```text
if inode missing:
    ENOENT

if mask asks for write:
    EROFS or EACCES

else:
    OK if file exists and is visible
```

### `statfs`

Purpose:

```text
return filesystem stats
```

First prototype can return simple/fake stats. It mainly prevents tools from failing when they call `statfs`.

## Mount function

First version:

```rust
pub fn mount_path_view(
    allowed_paths: Vec<PathBuf>,
    mountpoint: &Path,
) -> anyhow::Result<()> {
    let fs = PathViewFs::new(allowed_paths)?;

    let options = vec![
        fuser::MountOption::RO,
        fuser::MountOption::NoSuid,
        fuser::MountOption::NoDev,
        fuser::MountOption::FSName("nix-closure-view".to_string()),
    ];

    fuser::mount2(fs, mountpoint, &options)?;

    Ok(())
}
```

`mount2` blocks until the filesystem is unmounted. If using `spawn_mount2`, store the returned background session handle; dropping it unmounts the filesystem.

## Passthrough phase

Do this only after normal read-only behavior works.

### Add `init`

```rust
fn init(&mut self, _req: &Request<'_>, config: &mut KernelConfig) -> Result<(), nix::errno::Errno> {
    let _ = config.set_max_stack_depth(1);
    Ok(())
}
```

`set_max_stack_depth(1)` is needed for backing-file passthrough. If the exact return type differs in the selected `fuser` version, follow that version's trait signature. Prefer `nix::errno::Errno` in our code instead of exposing raw `libc` error integers.

### Update `open`

Passthrough version:

```rust
fn open(&self, _req: &Request<'_>, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
    let file = match self.open_real_file(ino) {
        Ok(file) => file,
        Err(errno) => {
            reply.error(errno);
            return;
        }
    };

    let fh = self.alloc_fh();

    match reply.open_backing(&file) {
        Ok(backing_id) => {
            self.open_files.lock().unwrap().insert(fh, OpenFile {
                file,
                backing_id: Some(backing_id),
            });

            // Need to borrow the stored backing_id so it lives after this function.
            let open_files = self.open_files.lock().unwrap();
            let backing_id = open_files
                .get(&fh)
                .unwrap()
                .backing_id
                .as_ref()
                .unwrap();

            reply.opened_passthrough(fh, FopenFlags::empty(), backing_id);
        }

        Err(_) => {
            // Fall back to normal userspace read.
            self.open_files.lock().unwrap().insert(fh, OpenFile {
                file,
                backing_id: None,
            });

            reply.opened(fh, FopenFlags::empty());
        }
    }
}
```

Important: this pseudocode locks `open_files` twice. In real code, avoid deadlocks and make sure the borrow of `backing_id` is valid until `opened_passthrough` consumes the reply. If borrow checker makes this awkward, restructure the storage layer. The semantic requirement is:

```text
BackingId must be stored in PathViewFs state before reply
BackingId must remain alive until release()
```

## Error mapping

We need a helper:

```rust
fn errno_from_io(err: io::Error) -> nix::errno::Errno;
```

Suggested mapping:

```text
NotFound           -> ENOENT
PermissionDenied   -> EACCES
AlreadyExists      -> EEXIST
InvalidInput       -> EINVAL
IsADirectory       -> EISDIR
NotADirectory      -> ENOTDIR
other raw_os_error -> that errno
fallback           -> EIO
```

Depending on `fuser` version, `reply.error()` may accept an errno-like type or raw OS errno value. In our code, prefer `nix::errno::Errno` as the internal representation and convert only at the boundary if the exact `fuser` version requires it. Follow compile errors.

## Read-only behavior

Mount with:

```rust
MountOption::RO
MountOption::NoSuid
MountOption::NoDev
```

Optionally add:

```rust
MountOption::NoExec
```

Do **not** add `NoExec` if you want to run binaries from the mounted Nix closure.

All write-like callbacks should either be unimplemented or return `EROFS`.

## Security and hardening notes

The first prototype is for proving behavior, not a sandbox boundary.

Later hardening should avoid path races:

```text
problem:
    lookup checks symlink_metadata(path)
    later open(path) could race if attacker can mutate parent dirs

Nix store is normally immutable, so this is less dangerous for /nix/store.
For arbitrary user paths, harden later.
```

Hardening options:

```text
openat2 with RESOLVE_BENEATH / RESOLVE_NO_SYMLINKS where appropriate
cap-std style directory capabilities
store parent directory fds instead of plain PathBuf
avoid following symlinks during traversal unless explicitly intended
```

For Nix store closure view, the first reasonable assumption is:

```text
/nix/store is immutable/trusted enough for prototype
other arbitrary mutable paths are not hardened yet
```

## Nix closure input

External command to get closure paths:

```bash
nix-store -qR /nix/store/<hash>-package
```

Or for a built derivation result:

```bash
nix-store -qR ./result
```

Feed each resulting store path as an allowed directory root.

The filesystem itself should not need to understand Nix derivations. It only consumes paths.

## Test plan

### Manual test 1: file path

Input:

```text
/path/to/file.txt
```

Expected:

```bash
ls ./filtered-mnt/path/to
cat ./filtered-mnt/path/to/file.txt
```

Sibling files under `/path/to` should not appear unless explicitly allowed.

### Manual test 2: directory path

Input:

```text
/nix/store/aaa-package
```

Expected:

```bash
ls ./filtered-mnt/nix/store
ls ./filtered-mnt/nix/store/aaa-package
```

`aaa-package` appears. Other store paths do not appear unless allowed.

### Manual test 3: two store paths

Input:

```text
/nix/store/aaa-package
/nix/store/bbb-package
```

Expected:

```bash
ls ./filtered-mnt/nix/store
```

shows only:

```text
aaa-package
bbb-package
```

### Manual test 4: forbidden sibling

If `/nix/store/ccc-package` was not in the input:

```bash
ls ./filtered-mnt/nix/store/ccc-package
```

Expected:

```text
ENOENT / No such file or directory
```

### Manual test 5: symlink

If allowed tree contains symlink:

```bash
readlink ./filtered-mnt/nix/store/aaa-package/some-link
```

Expected:

```text
same target as backing symlink
```

Do not rewrite target in the first prototype.

### Manual test 6: passthrough fallback

Run once without passthrough enabled. Confirm `cat` works.

Then enable passthrough. If passthrough fails due to kernel or privilege, `open()` should fall back to normal userspace read and `cat` should still work.

## Suggested implementation phases

### Phase 0: crate setup

Dependencies:

```toml
[dependencies]
fuser = "0.17"
nix = { version = "0.31", features = ["fs"] }
anyhow = "1"
```

Use `nix` for Unix-facing helpers such as `Errno`, `openat`/`openat2`-style hardening work, file metadata helpers, and other Linux/POSIX interfaces. Avoid depending directly on `libc` unless a needed API is not exposed by `nix`.

### Phase 1: in-memory virtual tree

Implement:

```text
PathViewFs::new
insert_allowed_path
ensure_virtual_dir
insert_real_node
resolve_child
attr_for_node
list_dir
```

No mounting yet. Unit test the tree.

### Phase 2: basic FUSE mount

Implement:

```text
lookup
getattr
opendir
readdir
releasedir
access
statfs
```

Test with `ls`.

### Phase 3: file reads

Implement:

```text
open
read
release
```

Test with `cat`.

### Phase 4: symlinks

Implement:

```text
readlink
```

Test with `readlink`.

### Phase 5: Nix closure workflow

Generate closure path list externally:

```bash
nix-store -qR ./result > closure.txt
```

Load paths from `closure.txt`.

Mount the view and test:

```bash
ls ./filtered-mnt/nix/store
```

### Phase 6: optional fd passthrough

Implement:

```text
init
ReplyOpen::open_backing
ReplyOpen::opened_passthrough
OpenFile.backing_id
```

Keep fallback read path.

### Phase 7: hardening

Only after behavior works:

```text
openat2 / capability-style path resolution
race prevention
mount namespace integration
optional absolute symlink strategy
more complete attrs
more complete statfs
```

## Codex instruction summary

When implementing, do this:

```text
Do not build a huge CLI first.
Do not implement writes.
Do not use OverlayFS.
Use fuser.
Start with a read-only path view.
Preserve absolute path shape inside the mount.
Use lazy expansion for allowed directory roots.
Use symlink_metadata so symlinks stay symlinks.
Make normal userspace open/read work before adding passthrough.
Passthrough is optional and must fall back cleanly.
Keep BackingId alive until release().
```

