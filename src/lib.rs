use std::collections::{BTreeMap, HashMap};
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, Metadata};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileExt, FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Mutex, RwLock,
};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use fuser::{
    AccessFlags, BackingId, Config, FileAttr, FileHandle, FileType, Filesystem, FopenFlags,
    Generation, INodeNo, InitFlags, KernelConfig, LockOwner, MountOption, OpenAccMode, OpenFlags,
    ReplyAttr, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs, Request,
};
use nix::errno::Errno;

const ROOT_INO: INodeNo = INodeNo::ROOT;
const TTL: Duration = Duration::from_secs(1);
const CAP_SYS_ADMIN_BIT: u32 = 21;

#[derive(Clone, Copy, Debug, Default)]
pub struct PathViewOptions {
    pub enable_passthrough: bool,
    pub no_exec: bool,
}

#[derive(Clone, Debug)]
enum Node {
    VirtualDir {
        virtual_path: PathBuf,
        parent: Option<INodeNo>,
    },
    Real {
        virtual_path: PathBuf,
        real_path: PathBuf,
        parent: Option<INodeNo>,
        allow_descendants: bool,
    },
}

impl Node {
    fn parent(&self) -> Option<INodeNo> {
        match self {
            Self::VirtualDir { parent, .. } | Self::Real { parent, .. } => *parent,
        }
    }
}

#[derive(Default)]
struct TreeState {
    nodes: HashMap<INodeNo, Node>,
    children: HashMap<INodeNo, BTreeMap<OsString, INodeNo>>,
    path_index: HashMap<PathBuf, INodeNo>,
}

struct OpenFile {
    file: File,
    backing_id: Option<BackingId>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DirEntry {
    pub ino: INodeNo,
    pub name: OsString,
    pub kind: FileType,
}

pub struct PathViewFs {
    tree: RwLock<TreeState>,
    next_ino: AtomicU64,
    open_files: Mutex<HashMap<FileHandle, OpenFile>>,
    next_fh: AtomicU64,
    options: PathViewOptions,
}

impl PathViewFs {
    pub fn new(allowed_paths: Vec<PathBuf>) -> io::Result<Self> {
        Self::with_options(allowed_paths, PathViewOptions::default())
    }

    pub fn with_options(allowed_paths: Vec<PathBuf>, options: PathViewOptions) -> io::Result<Self> {
        let fs = Self::empty_with_root(options);
        for path in allowed_paths {
            fs.insert_allowed_path(path)?;
        }
        Ok(fs)
    }

    fn empty_with_root(options: PathViewOptions) -> Self {
        let mut tree = TreeState::default();
        tree.nodes.insert(
            ROOT_INO,
            Node::VirtualDir {
                virtual_path: PathBuf::from("/"),
                parent: None,
            },
        );
        tree.children.insert(ROOT_INO, BTreeMap::new());
        tree.path_index.insert(PathBuf::from("/"), ROOT_INO);

        Self {
            tree: RwLock::new(tree),
            next_ino: AtomicU64::new(2),
            open_files: Mutex::new(HashMap::new()),
            next_fh: AtomicU64::new(1),
            options,
        }
    }

    fn insert_allowed_path(&self, real_path: PathBuf) -> io::Result<INodeNo> {
        let real_path = normalize_absolute_path(real_path)?;
        let meta = fs::symlink_metadata(&real_path)?;
        let virtual_path = real_path.clone();

        if let Some(parent) = virtual_path.parent() {
            self.ensure_virtual_dir(parent)?;
        }

        self.insert_real_node(virtual_path, real_path, meta.is_dir())
    }

    fn ensure_virtual_dir(&self, virtual_path: &Path) -> io::Result<INodeNo> {
        if virtual_path == Path::new("/") {
            return Ok(ROOT_INO);
        }

        if !virtual_path.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("virtual path must be absolute: {}", virtual_path.display()),
            ));
        }

        {
            let tree = self.tree.read().unwrap();
            if let Some(&ino) = tree.path_index.get(virtual_path) {
                return Ok(ino);
            }
        }

        let parent_path = virtual_path.parent().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("virtual path has no parent: {}", virtual_path.display()),
            )
        })?;
        let parent_ino = self.ensure_virtual_dir(parent_path)?;
        let name = basename(virtual_path)?.to_os_string();

        let mut tree = self.tree.write().unwrap();
        if let Some(&ino) = tree.path_index.get(virtual_path) {
            return Ok(ino);
        }

        let ino = self.alloc_ino();
        tree.nodes.insert(
            ino,
            Node::VirtualDir {
                virtual_path: virtual_path.to_path_buf(),
                parent: Some(parent_ino),
            },
        );
        tree.children.entry(ino).or_default();
        tree.path_index.insert(virtual_path.to_path_buf(), ino);
        drop(tree);
        self.add_child(parent_ino, name, ino)?;
        Ok(ino)
    }

    fn insert_real_node(
        &self,
        virtual_path: PathBuf,
        real_path: PathBuf,
        allow_descendants: bool,
    ) -> io::Result<INodeNo> {
        let mut tree = self.tree.write().unwrap();
        self.insert_real_node_locked(&mut tree, virtual_path, real_path, allow_descendants)
    }

    fn insert_real_node_locked(
        &self,
        tree: &mut TreeState,
        virtual_path: PathBuf,
        real_path: PathBuf,
        allow_descendants: bool,
    ) -> io::Result<INodeNo> {
        if let Some(&ino) = tree.path_index.get(&virtual_path) {
            match tree.nodes.get(&ino).cloned() {
                Some(Node::VirtualDir { parent, .. }) => {
                    tree.nodes.insert(
                        ino,
                        Node::Real {
                            virtual_path: virtual_path.clone(),
                            real_path,
                            parent,
                            allow_descendants,
                        },
                    );
                    return Ok(ino);
                }
                Some(Node::Real {
                    real_path: existing_real,
                    allow_descendants: _,
                    ..
                }) => {
                    if existing_real != real_path {
                        return Err(io::Error::new(
                            io::ErrorKind::AlreadyExists,
                            format!("conflicting real path for {}", virtual_path.display()),
                        ));
                    }
                    if allow_descendants {
                        if let Some(Node::Real {
                            allow_descendants: existing_allow_descendants,
                            ..
                        }) = tree.nodes.get_mut(&ino)
                        {
                            *existing_allow_descendants = true;
                        }
                    }
                    return Ok(ino);
                }
                None => {
                    return Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        format!("inode disappeared for {}", virtual_path.display()),
                    ));
                }
            }
        }

        let parent_ino = match virtual_path.parent() {
            Some(parent_path) => Some(*tree.path_index.get(parent_path).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("parent missing for {}", virtual_path.display()),
                )
            })?),
            None => None,
        };

        let ino = self.alloc_ino();
        tree.nodes.insert(
            ino,
            Node::Real {
                virtual_path: virtual_path.clone(),
                real_path,
                parent: parent_ino,
                allow_descendants,
            },
        );
        tree.children.entry(ino).or_default();
        tree.path_index.insert(virtual_path.clone(), ino);

        if let Some(parent_ino) = parent_ino {
            tree.children
                .entry(parent_ino)
                .or_default()
                .insert(basename(&virtual_path)?.to_os_string(), ino);
        }

        Ok(ino)
    }

    fn alloc_ino(&self) -> INodeNo {
        INodeNo(self.next_ino.fetch_add(1, Ordering::Relaxed))
    }

    fn alloc_fh(&self) -> FileHandle {
        FileHandle(self.next_fh.fetch_add(1, Ordering::Relaxed))
    }

    fn add_child(&self, parent: INodeNo, name: OsString, child: INodeNo) -> io::Result<()> {
        let mut tree = self.tree.write().unwrap();
        if !tree.nodes.contains_key(&parent) {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("parent inode {} missing", parent),
            ));
        }
        tree.children.entry(parent).or_default().insert(name, child);
        Ok(())
    }

    fn node(&self, ino: INodeNo) -> io::Result<Node> {
        let tree = self.tree.read().unwrap();
        tree.nodes.get(&ino).cloned().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, format!("unknown inode {}", ino))
        })
    }

    fn resolve_child(&self, parent: INodeNo, name: &OsStr) -> io::Result<INodeNo> {
        {
            let tree = self.tree.read().unwrap();
            if let Some(ino) = tree
                .children
                .get(&parent)
                .and_then(|children| children.get(name))
                .copied()
            {
                return Ok(ino);
            }
        }

        self.maybe_materialize_real_child(parent, name)
    }

    fn maybe_materialize_real_child(&self, parent: INodeNo, name: &OsStr) -> io::Result<INodeNo> {
        if name == OsStr::new(".") {
            return Ok(parent);
        }
        if name == OsStr::new("..") {
            return Ok(self.node(parent)?.parent().unwrap_or(ROOT_INO));
        }

        let parent_node = self.node(parent)?;
        let (parent_virtual, parent_real) = match parent_node {
            Node::Real {
                virtual_path,
                real_path,
                allow_descendants: true,
                ..
            } => (virtual_path, real_path),
            _ => return Err(io::Error::from_raw_os_error(Errno::ENOENT as i32)),
        };

        let child_real = parent_real.join(name);
        let child_meta = fs::symlink_metadata(&child_real)?;
        let child_virtual = parent_virtual.join(name);

        let mut tree = self.tree.write().unwrap();
        if let Some(ino) = tree
            .children
            .get(&parent)
            .and_then(|children| children.get(name))
            .copied()
        {
            return Ok(ino);
        }

        self.insert_real_node_locked(&mut tree, child_virtual, child_real, child_meta.is_dir())
    }

    pub fn attr_for_node(&self, ino: INodeNo) -> io::Result<FileAttr> {
        let node = self.node(ino)?;
        match node {
            Node::VirtualDir { .. } => Ok(FileAttr {
                ino,
                size: 0,
                blocks: 0,
                atime: UNIX_EPOCH,
                mtime: UNIX_EPOCH,
                ctime: UNIX_EPOCH,
                crtime: UNIX_EPOCH,
                kind: FileType::Directory,
                perm: 0o555,
                nlink: 2,
                uid: 0,
                gid: 0,
                rdev: 0,
                blksize: 4096,
                flags: 0,
            }),
            Node::Real { real_path, .. } => {
                metadata_to_attr(ino, &fs::symlink_metadata(real_path)?)
            }
        }
    }

    pub fn list_dir(&self, ino: INodeNo) -> io::Result<Vec<DirEntry>> {
        match self.node(ino)? {
            Node::VirtualDir { .. } => self.list_materialized_children(ino),
            Node::Real {
                real_path,
                allow_descendants: true,
                ..
            } => {
                for entry in fs::read_dir(real_path)? {
                    let entry = entry?;
                    let name = entry.file_name();
                    let _ = self.maybe_materialize_real_child(ino, &name)?;
                }
                self.list_materialized_children(ino)
            }
            Node::Real { .. } => Err(io::Error::from_raw_os_error(Errno::ENOTDIR as i32)),
        }
    }

    fn list_materialized_children(&self, ino: INodeNo) -> io::Result<Vec<DirEntry>> {
        let child_map = {
            let tree = self.tree.read().unwrap();
            tree.children.get(&ino).cloned().unwrap_or_default()
        };

        let mut entries = Vec::with_capacity(child_map.len());
        for (name, child_ino) in child_map {
            let kind = self.attr_for_node(child_ino)?.kind;
            entries.push(DirEntry {
                ino: child_ino,
                name,
                kind,
            });
        }
        Ok(entries)
    }

    fn is_dir(&self, ino: INodeNo) -> bool {
        self.attr_for_node(ino)
            .map(|attr| attr.kind == FileType::Directory)
            .unwrap_or(false)
    }

    fn real_path_for_regular_file(&self, ino: INodeNo) -> io::Result<PathBuf> {
        match self.node(ino)? {
            Node::VirtualDir { .. } => Err(io::Error::from_raw_os_error(Errno::EISDIR as i32)),
            Node::Real { real_path, .. } => {
                let meta = fs::symlink_metadata(&real_path)?;
                if meta.is_file() {
                    Ok(real_path)
                } else if meta.is_dir() {
                    Err(io::Error::from_raw_os_error(Errno::EISDIR as i32))
                } else {
                    Err(io::Error::from_raw_os_error(Errno::EINVAL as i32))
                }
            }
        }
    }

    fn real_path_for_symlink(&self, ino: INodeNo) -> io::Result<PathBuf> {
        match self.node(ino)? {
            Node::VirtualDir { .. } => Err(io::Error::from_raw_os_error(Errno::EINVAL as i32)),
            Node::Real { real_path, .. } => {
                let meta = fs::symlink_metadata(&real_path)?;
                if meta.file_type().is_symlink() {
                    Ok(real_path)
                } else {
                    Err(io::Error::from_raw_os_error(Errno::EINVAL as i32))
                }
            }
        }
    }
}

impl Filesystem for PathViewFs {
    fn init(&mut self, _req: &Request, config: &mut KernelConfig) -> io::Result<()> {
        eprintln!(
            "init: passthrough_requested={}, no_exec={}",
            self.options.enable_passthrough, self.options.no_exec
        );
        log_capability_diagnostics();
        eprintln!("init: kernel capabilities: {:?}", config.capabilities());

        if self.options.enable_passthrough {
            match config.add_capabilities(InitFlags::FUSE_PASSTHROUGH) {
                Ok(()) => {
                    eprintln!("init: requested FUSE_PASSTHROUGH capability successfully");
                }
                Err(unsupported) => {
                    eprintln!(
                        "init: failed to request FUSE_PASSTHROUGH capability, unsupported bits: {:?}",
                        unsupported
                    );
                }
            }

            let requested_stack_depth = 1;
            eprintln!(
                "init: attempting passthrough setup with max_stack_depth={}",
                requested_stack_depth
            );

            match config.set_max_stack_depth(requested_stack_depth) {
                Ok(actual_stack_depth) => {
                    eprintln!(
                        "init: set_max_stack_depth({}) succeeded, negotiated={}",
                        requested_stack_depth, actual_stack_depth
                    );
                }
                Err(err) => {
                    eprintln!(
                        "init: set_max_stack_depth({}) failed: {}",
                        requested_stack_depth, err
                    );
                }
            }
        } else {
            eprintln!("init: passthrough disabled");
        }

        Ok(())
    }

    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        match self
            .resolve_child(parent, name)
            .and_then(|ino| self.attr_for_node(ino).map(|attr| (ino, attr)))
        {
            Ok((_ino, attr)) => reply.entry(&TTL, &attr, Generation(0)),
            Err(err) => reply.error(fuser_errno_from_io(&err)),
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        match self.attr_for_node(ino) {
            Ok(attr) => reply.attr(&TTL, &attr),
            Err(err) => reply.error(fuser_errno_from_io(&err)),
        }
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        match self.real_path_for_symlink(ino).and_then(fs::read_link) {
            Ok(target) => reply.data(target.as_os_str().as_bytes()),
            Err(err) => reply.error(fuser_errno_from_io(&err)),
        }
    }

    fn opendir(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        if self.is_dir(ino) {
            reply.opened(FileHandle(0), FopenFlags::empty());
        } else {
            reply.error(fuser::Errno::ENOTDIR);
        }
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let mut entries = match self.list_dir(ino) {
            Ok(entries) => entries,
            Err(err) => {
                reply.error(fuser_errno_from_io(&err));
                return;
            }
        };

        let parent_ino = self
            .node(ino)
            .ok()
            .and_then(|node| node.parent())
            .unwrap_or(ROOT_INO);
        entries.insert(
            0,
            DirEntry {
                ino,
                name: OsString::from("."),
                kind: FileType::Directory,
            },
        );
        entries.insert(
            1,
            DirEntry {
                ino: parent_ino,
                name: OsString::from(".."),
                kind: FileType::Directory,
            },
        );

        for (i, entry) in entries.into_iter().enumerate().skip(offset as usize) {
            if reply.add(entry.ino, (i + 1) as u64, entry.kind, entry.name) {
                break;
            }
        }

        reply.ok();
    }

    fn releasedir(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _flags: OpenFlags,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        if flags.acc_mode() != OpenAccMode::O_RDONLY {
            reply.error(fuser::Errno::EROFS);
            return;
        }

        let real_path = match self.real_path_for_regular_file(ino) {
            Ok(path) => path,
            Err(err) => {
                reply.error(fuser_errno_from_io(&err));
                return;
            }
        };

        let file = match File::open(&real_path) {
            Ok(file) => file,
            Err(err) => {
                reply.error(fuser_errno_from_io(&err));
                return;
            }
        };

        let fh = self.alloc_fh();
        if self.options.enable_passthrough {
            match reply.open_backing(&file) {
                Ok(backing_id) => {
                    let mut open_files = self.open_files.lock().unwrap();
                    open_files.insert(
                        fh,
                        OpenFile {
                            file,
                            backing_id: Some(backing_id),
                        },
                    );
                    let backing_id = open_files
                        .get(&fh)
                        .and_then(|entry| entry.backing_id.as_ref())
                        .expect("backing id stored before passthrough reply");
                    reply.opened_passthrough(fh, FopenFlags::empty(), backing_id);
                    return;
                }
                Err(err) => {
                    eprintln!(
                        "passthrough open failed for {}, falling back to userspace read: {}",
                        real_path.display(),
                        err
                    );
                }
            }
        }

        self.open_files.lock().unwrap().insert(
            fh,
            OpenFile {
                file,
                backing_id: None,
            },
        );
        reply.opened(fh, FopenFlags::empty());
    }

    fn read(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        let mut buf = vec![0_u8; size as usize];
        let open_files = self.open_files.lock().unwrap();
        let Some(open_file) = open_files.get(&fh) else {
            reply.error(fuser::Errno::EBADF);
            return;
        };

        match open_file.file.read_at(&mut buf, offset) {
            Ok(n) => reply.data(&buf[..n]),
            Err(err) => reply.error(fuser::Errno::from(err)),
        }
    }

    fn release(
        &self,
        _req: &Request,
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

    fn access(&self, _req: &Request, ino: INodeNo, mask: AccessFlags, reply: ReplyEmpty) {
        match self.attr_for_node(ino) {
            Ok(_) if mask.contains(AccessFlags::W_OK) => reply.error(fuser::Errno::EROFS),
            Ok(_) => reply.ok(),
            Err(err) => reply.error(fuser_errno_from_io(&err)),
        }
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        let files = self.tree.read().unwrap().nodes.len() as u64;
        reply.statfs(0, 0, 0, files, 0, 4096, 255, 4096);
    }
}

pub fn mount_path_view(
    allowed_paths: Vec<PathBuf>,
    mountpoint: &Path,
    options: PathViewOptions,
) -> Result<()> {
    let fs = PathViewFs::with_options(allowed_paths, options)?;

    let mut config = Config::default();
    config.mount_options = vec![
        MountOption::RO,
        MountOption::NoSuid,
        MountOption::NoDev,
        MountOption::FSName("nix-closure-view".to_string()),
    ];
    if options.no_exec {
        config.mount_options.push(MountOption::NoExec);
    }

    fuser::mount2(fs, mountpoint, &config)
        .with_context(|| format!("failed to mount path view at {}", mountpoint.display()))?;
    Ok(())
}

pub fn load_allowed_paths_from_file(path: &Path) -> Result<Vec<PathBuf>> {
    let contents =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    parse_allowed_paths(&contents, &path.display().to_string())
}

pub fn parse_allowed_paths(contents: &str, source_name: &str) -> Result<Vec<PathBuf>> {
    let mut allowed_paths = Vec::new();

    for (line_no, line) in contents.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let path_buf = PathBuf::from(trimmed);
        if !path_buf.is_absolute() {
            return Err(anyhow!(
                "{}:{}: expected absolute path, got {}",
                source_name,
                line_no + 1,
                trimmed
            ));
        }
        allowed_paths.push(path_buf);
    }

    Ok(allowed_paths)
}

fn normalize_absolute_path(path: PathBuf) -> io::Result<PathBuf> {
    if path.is_absolute() {
        Ok(path)
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("allowed path must be absolute: {}", path.display()),
        ))
    }
}

fn basename(path: &Path) -> io::Result<&OsStr> {
    path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("path has no basename: {}", path.display()),
        )
    })
}

fn metadata_to_attr(ino: INodeNo, meta: &Metadata) -> io::Result<FileAttr> {
    Ok(FileAttr {
        ino,
        size: meta.size(),
        blocks: meta.blocks(),
        atime: system_time_from_unix(meta.atime(), meta.atime_nsec())?,
        mtime: system_time_from_unix(meta.mtime(), meta.mtime_nsec())?,
        ctime: system_time_from_unix(meta.ctime(), meta.ctime_nsec())?,
        crtime: UNIX_EPOCH,
        kind: file_type_from_metadata(meta),
        perm: (meta.mode() & 0o7777) as u16,
        nlink: meta.nlink() as u32,
        uid: meta.uid(),
        gid: meta.gid(),
        rdev: meta.rdev() as u32,
        blksize: meta.blksize() as u32,
        flags: 0,
    })
}

fn file_type_from_metadata(meta: &Metadata) -> FileType {
    let kind = meta.file_type();
    if kind.is_dir() {
        FileType::Directory
    } else if kind.is_symlink() {
        FileType::Symlink
    } else if kind.is_file() {
        FileType::RegularFile
    } else if kind.is_block_device() {
        FileType::BlockDevice
    } else if kind.is_char_device() {
        FileType::CharDevice
    } else if kind.is_fifo() {
        FileType::NamedPipe
    } else if kind.is_socket() {
        FileType::Socket
    } else {
        FileType::RegularFile
    }
}

fn system_time_from_unix(secs: i64, nanos: i64) -> io::Result<SystemTime> {
    if secs < 0 || !(0..1_000_000_000).contains(&nanos) {
        return Ok(UNIX_EPOCH);
    }
    Ok(UNIX_EPOCH + Duration::new(secs as u64, nanos as u32))
}

fn errno_from_io(err: &io::Error) -> Errno {
    if let Some(raw) = err.raw_os_error() {
        return Errno::from_raw(raw);
    }

    match err.kind() {
        io::ErrorKind::NotFound => Errno::ENOENT,
        io::ErrorKind::PermissionDenied => Errno::EACCES,
        io::ErrorKind::AlreadyExists => Errno::EEXIST,
        io::ErrorKind::InvalidInput => Errno::EINVAL,
        io::ErrorKind::IsADirectory => Errno::EISDIR,
        io::ErrorKind::NotADirectory => Errno::ENOTDIR,
        _ => Errno::EIO,
    }
}

fn fuser_errno_from_io(err: &io::Error) -> fuser::Errno {
    fuser::Errno::from_i32(errno_from_io(err) as i32)
}

fn log_capability_diagnostics() {
    match read_cap_eff_hex() {
        Ok(cap_eff_hex) => {
            let has_cap_sys_admin = cap_eff_hex
                .checked_shr(CAP_SYS_ADMIN_BIT)
                .map(|value| (value & 1) == 1)
                .unwrap_or(false);
            eprintln!(
                "init: capability check: CapEff=0x{cap_eff_hex:016x}, CAP_SYS_ADMIN={}",
                has_cap_sys_admin
            );
        }
        Err(err) => {
            eprintln!("init: capability check failed: {}", err);
        }
    }
}

fn read_cap_eff_hex() -> io::Result<u64> {
    let status = fs::read_to_string("/proc/self/status")?;
    for line in status.lines() {
        if let Some(value) = line.strip_prefix("CapEff:\t") {
            return u64::from_str_radix(value.trim(), 16).map_err(|err| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("failed to parse CapEff value {value:?}: {err}"),
                )
            });
        }
    }

    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "CapEff not found in /proc/self/status",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self, File};
    use std::io::Write;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(label: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "nix-closure-fuser-{label}-{unique}-{}",
            std::process::id()
        ))
    }

    fn lookup_virtual_path(fs: &PathViewFs, path: &Path) -> io::Result<INodeNo> {
        let mut current = ROOT_INO;
        for component in path.components() {
            use std::path::Component;
            match component {
                Component::RootDir => {}
                Component::Normal(name) => {
                    current = fs.resolve_child(current, name)?;
                }
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("unsupported test path {}", path.display()),
                    ));
                }
            }
        }
        Ok(current)
    }

    #[test]
    fn builds_tree_for_allowed_file() {
        let root = temp_dir("file");
        let file_path = root.join("alpha/beta/file.txt");
        fs::create_dir_all(file_path.parent().unwrap()).unwrap();
        let mut file = File::create(&file_path).unwrap();
        writeln!(file, "hello").unwrap();

        let path_view = PathViewFs::new(vec![file_path.clone()]).unwrap();
        let ino = lookup_virtual_path(&path_view, &file_path).unwrap();
        let attr = path_view.attr_for_node(ino).unwrap();
        assert_eq!(attr.kind, FileType::RegularFile);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn hides_forbidden_siblings() {
        let root = temp_dir("siblings");
        let allowed = root.join("allowed");
        let forbidden = root.join("forbidden");
        fs::create_dir_all(&allowed).unwrap();
        fs::create_dir_all(&forbidden).unwrap();
        File::create(allowed.join("visible.txt")).unwrap();
        File::create(forbidden.join("hidden.txt")).unwrap();

        let path_view = PathViewFs::new(vec![allowed.clone()]).unwrap();
        let parent_ino = lookup_virtual_path(&path_view, allowed.parent().unwrap()).unwrap();

        assert!(path_view
            .resolve_child(parent_ino, basename(&allowed).unwrap())
            .is_ok());
        assert!(path_view
            .resolve_child(parent_ino, basename(&forbidden).unwrap())
            .is_err());

        let _ = fs::remove_dir_all(root);
    }
}
