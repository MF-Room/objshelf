//! Object store in a directory of a file system, one file per object (feature `fs`, Unix).
//!
//! Each object is serialized with postcard, its name and the 4-byte tag of its type first (see
//! [`type_tag`]), in a file of its own: `{id:08x}.obj`, a 16-byte header then the payload, padded
//! to whole 4 KiB blocks (see `file`). A write goes to `{id:08x}.tmp`, which is synced, then
//! renamed over the object's file.
//!
//! `ObjShelf` already caches the objects in RAM, so on Linux the files are read and written with
//! `O_DIRECT`, past the page cache, which would hold a second copy. A file system that refuses it
//! (`EINVAL`, as tmpfs on older kernels) gets buffered I/O for the rest of the mount: see
//! [`FsBackend::direct_io`].
//!
//! ```
//! use objshelf::backend::fs::{FsBackend, FsConfig, FsError};
//! use objshelf::{Error, ObjShelf, flush, get, insert, with};
//!
//! # let dir = std::env::temp_dir().join(format!("objshelf-doc-{}", std::process::id()));
//! let config = || FsConfig {
//!     dir: dir.clone(),
//!     capacity: 1 << 20,
//!     direct_io: true,
//! };
//! let mut mem = ObjShelf::<FsBackend>::mount(config(), 16 * 1024, || {})?;
//! insert(&mut mem, "boots", 1u32)?;
//! flush(&mut mem)?;
//! mem.unmount()?;
//!
//! // After a restart.
//! let mut mem = ObjShelf::<FsBackend>::mount(config(), 16 * 1024, || {})?;
//! let boots = get::<u32, _>(&mut mem, "boots")?;
//! assert_eq!(with(&mut mem, &boots, |n| *n)?, Some(1));
//! # mem.unmount()?;
//! # std::fs::remove_dir_all(&dir).unwrap();
//! # Ok::<(), Error<FsError>>(())
//! ```
//!
//! # Requirements
//!
//! - **System**: Unix, with `std`. `O_DIRECT` is used on Linux only.
//! - **Directory**: used by this backend only. A lock file in it makes a second mount, from this
//!   process or another one, fail with [`FsError::Locked`]. Other files are ignored.
//! - **Device**: for `O_DIRECT`, a logical block size of at most 4 KiB (512 or 4096 bytes). On
//!   a larger one, the first `EINVAL` turns `O_DIRECT` off.
//! - **Values**: `Serialize` / `DeserializeOwned` (postcard).
//!
//! # Guarantees
//!
//! - A `write` or `remove` is on disk when it returns: the file and the directory are synced.
//!   `O_DIRECT` skips the page cache, not the sync: the device's cache and the file's metadata
//!   still need it.
//! - Each object is updated atomically, by a rename: after a crash, it holds either its old or
//!   its new value. Mount deletes the `.tmp` files that writes cut by a crash leave. There are no
//!   transactions over several objects.
//! - Each file holds a CRC of its payload, checked by `read`: [`FsError::Corrupt`] if it doesn't
//!   match. Mount checks the header and the name of each file and skips the files that fail,
//!   leaving them on disk, so one damaged file never blocks the mount.
//! - Each file header holds the version of the on-disk format: mount fails with
//!   [`FsError::UnsupportedFormat`] on a file of another version, without changing anything.
//! - The files take at most `capacity` bytes in all: a write that would go past it fails with
//!   [`FsError::NoSpace`], and the object keeps its previous value. While it runs, the `.tmp` file
//!   also takes the new size on disk.
//!
//! # Time
//!
//! `n` objects, `b` blocks of 4 KiB in the object's file (`ceil((16 + len) / 4096)`).
//!
//! | Operation | Disk | CPU |
//! |---|---|---|
//! | `mount` | one directory listing; per object, one `stat` and one 4 KiB read | `O(n)` |
//! | `read` | one open and a `b`-block read | `O(len)` |
//! | `write` | one open, a `b`-block write, a file sync, a rename, a directory sync | `O(len)` |
//! | `remove` | one unlink, a directory sync | `O(1)` |
//! | `name` | none during mount (kept from mount), else a 4 KiB read | `O(1)` |
//! | `usage`, `ids` | none | `O(1)`, `O(n)` |
//!
//! Hash map operations are expected `O(1)`. `write` serializes twice: once to measure the object,
//! once into the buffer that goes to disk, so the object is held only once.
//!
//! # Space
//!
//! RAM, 64-bit target:
//!
//! - index: about 50 B per object, plus its name from mount until the memory layer takes it,
//!   during its mount;
//! - `read` and `write`: the whole file (header, object, padding), plus one block to align it.
//!
//! Disk: the header and the padding to whole blocks, so at least 4 KiB per object. An object,
//! serialized with its name and type tag, takes less than 4 GiB.

mod file;
#[cfg(test)]
mod tests;

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use hashbrown::HashMap;
use postcard::ser_flavors::Size;
use serde::{Serialize, de::DeserializeOwned};

use super::{Backend, Usage, decode, type_tag};
use crate::ObjectId;
use file::{AlignedBuf, BLOCK, HEADER_SIZE, Kind};

/// Name of the lock file in the directory.
const LOCK_FILE: &str = "lock";

/// Configuration of an [`FsBackend`].
pub struct FsConfig {
    /// Directory of the objects, created if missing, and used by nothing else.
    pub dir: PathBuf,
    /// Bytes the object files may take in all, in whole 4 KiB blocks: a write that would go past
    /// it fails with [`FsError::NoSpace`]. If the files found at mount already take more, `usage`
    /// reports `used` above it, and only writes that shrink the total succeed.
    pub capacity: usize,
    /// Read and write with `O_DIRECT`, past the page cache. Linux only: ignored elsewhere. If the
    /// file system refuses it, the backend falls back to buffered I/O (see
    /// [`FsBackend::direct_io`]).
    pub direct_io: bool,
}

/// Errors of an [`FsBackend`].
#[derive(Debug)]
pub enum FsError {
    /// The file system failed.
    Io(io::Error),
    /// Another `FsBackend`, in this process or another one, has the directory mounted.
    Locked,
    /// No object with this id.
    NotFound,
    /// The object would take the files past the capacity, or its payload is 4 GiB or more.
    NoSpace,
    /// The file of the object failed its checks.
    Corrupt,
    /// The value could not be serialized or deserialized.
    Serialization(postcard::Error),
    /// A file holds another version of the on-disk format, the one given, written by other
    /// software: the mount fails without changing anything.
    UnsupportedFormat(u16),
}

impl From<io::Error> for FsError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// An object file.
struct Entry {
    /// Size of the file: whole blocks.
    len: usize,
    /// Name read at mount, until [`Backend::name`] takes it.
    name: Option<String>,
}

/// Object store in a directory, one file per object.
pub struct FsBackend {
    dir: PathBuf,
    /// Locked for the whole mount, and unlocked when dropped.
    _lock: File,
    capacity: usize,
    /// `O_DIRECT` is on.
    direct: bool,
    objects: HashMap<ObjectId, Entry>,
    /// Bytes of all the object files.
    used: usize,
}

impl FsBackend {
    /// Whether the files are read and written with `O_DIRECT`: `FsConfig::direct_io` on Linux,
    /// until the file system refuses it once.
    pub fn direct_io(&self) -> bool {
        self.direct
    }

    fn path(&self, id: ObjectId, kind: Kind) -> PathBuf {
        self.dir.join(file::file_name(id, kind))
    }

    /// Runs `op`, given whether to use `O_DIRECT`. If the file system refuses `O_DIRECT`, turns it
    /// off and runs `op` once more, buffered.
    fn io<R>(&mut self, mut op: impl FnMut(bool) -> io::Result<R>) -> io::Result<R> {
        if self.direct {
            match op(true) {
                Err(error) if refuses_direct_io(&error) => self.direct = false,
                result => return result,
            }
        }
        op(false)
    }

    /// Reads the first `len` bytes of the file of `id`, a multiple of [`BLOCK`]:
    /// [`FsError::Corrupt`] if the file is shorter.
    fn read_file(&mut self, id: ObjectId, len: usize) -> Result<AlignedBuf, FsError> {
        let path = self.path(id, Kind::Object);
        let mut buf = AlignedBuf::new(len);
        let read = self.io(|direct| {
            open_options(direct)
                .read(true)
                .open(&path)?
                .read_exact(&mut buf)
        });
        match read {
            Ok(()) => Ok(buf),
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => Err(FsError::Corrupt),
            Err(error) => Err(error.into()),
        }
    }

    /// Indexes the file of `id`, of `len` bytes, from its first block. A file that fails its
    /// checks is skipped and left on disk; one of another format version fails the mount.
    fn load(&mut self, id: ObjectId, len: u64) -> Result<(), FsError> {
        let Ok(len) = usize::try_from(len) else {
            return Ok(());
        };
        let name = match self.read_file(id, BLOCK) {
            Ok(block) => file::name(&block, len).map(String::from),
            Err(error) => Err(error),
        };
        match name {
            Ok(name) => {
                self.used += len;
                let name = Some(name);
                self.objects.insert(id, Entry { len, name });
                Ok(())
            }
            Err(FsError::Corrupt) => Ok(()),
            Err(error) => Err(error),
        }
    }

    /// Makes the renames and unlinks in the directory durable.
    fn sync_dir(&self) -> Result<(), FsError> {
        File::open(&self.dir)?.sync_all()?;
        Ok(())
    }
}

/// Options to open a file, with `O_DIRECT` if `direct`.
fn open_options(direct: bool) -> OpenOptions {
    let mut options = OpenOptions::new();
    #[cfg(target_os = "linux")]
    if direct {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_DIRECT);
    }
    #[cfg(not(target_os = "linux"))]
    let _ = direct;
    options
}

/// The file system refused `O_DIRECT`, at the open or for the alignment: `EINVAL`.
fn refuses_direct_io(error: &io::Error) -> bool {
    #[cfg(target_os = "linux")]
    return error.raw_os_error() == Some(libc::EINVAL);
    #[cfg(not(target_os = "linux"))]
    {
        let _ = error;
        false
    }
}

/// Takes the lock of the directory: [`FsError::Locked`] if another mount has it.
fn lock(dir: &Path) -> Result<File, FsError> {
    let lock = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.join(LOCK_FILE))?;
    match lock.try_lock() {
        Ok(()) => Ok(lock),
        Err(TryLockError::WouldBlock) => Err(FsError::Locked),
        Err(TryLockError::Error(error)) => Err(error.into()),
    }
}

impl Backend for FsBackend {
    type Error = FsError;
    type Config = FsConfig;

    /// Lists the directory, reads the first block of each object file, then deletes the `.tmp`
    /// files left by writes cut by a crash.
    fn mount(config: Self::Config) -> Result<Self, Self::Error> {
        let FsConfig {
            dir,
            capacity,
            direct_io,
        } = config;
        fs::create_dir_all(&dir)?;
        let lock = lock(&dir)?;
        let mut backend = Self {
            dir,
            _lock: lock,
            capacity,
            direct: direct_io && cfg!(target_os = "linux"),
            objects: HashMap::new(),
            used: 0,
        };

        let mut objects = Vec::new();
        let mut temps = Vec::new();
        for entry in fs::read_dir(&backend.dir)? {
            let entry = entry?;
            let file_name = entry.file_name();
            let Some((id, kind)) = file_name.to_str().and_then(file::parse_name) else {
                continue;
            };
            let metadata = entry.metadata()?;
            if !metadata.is_file() {
                continue;
            }
            match kind {
                Kind::Object => objects.push((id, metadata.len())),
                Kind::Temp => temps.push(entry.path()),
            }
        }
        for (id, len) in objects {
            backend.load(id, len)?;
        }
        for temp in &temps {
            fs::remove_file(temp)?;
        }
        if !temps.is_empty() {
            backend.sync_dir()?;
        }
        Ok(backend)
    }

    fn write<T: Serialize>(
        &mut self,
        id: ObjectId,
        name: &str,
        value: &T,
    ) -> Result<(), Self::Error> {
        let object = (name, type_tag::<T>().to_le_bytes(), value);
        let payload_len = postcard::serialize_with_flavor(&object, Size::default())
            .map_err(FsError::Serialization)?;
        let payload_len = u32::try_from(payload_len).map_err(|_| FsError::NoSpace)?;
        let len = file::file_len(payload_len as usize);
        let old_len = self.objects.get(&id).map_or(0, |entry| entry.len);
        if self.used - old_len + len > self.capacity {
            return Err(FsError::NoSpace);
        }

        let mut buf = AlignedBuf::new(len);
        postcard::to_slice(&object, &mut buf[HEADER_SIZE..]).map_err(FsError::Serialization)?;
        file::seal(&mut buf, payload_len);
        let temp = self.path(id, Kind::Temp);
        self.io(|direct| {
            let mut file = open_options(direct)
                .write(true)
                .create(true)
                .truncate(true)
                .open(&temp)?;
            file.write_all(&buf)?;
            file.sync_all()
        })?;
        fs::rename(&temp, self.path(id, Kind::Object))?;
        // The file is replaced from here, even if the directory sync fails.
        self.used = self.used - old_len + len;
        self.objects.insert(id, Entry { len, name: None });
        self.sync_dir()
    }

    fn read<T: DeserializeOwned>(&mut self, id: ObjectId) -> Result<Option<T>, Self::Error> {
        let len = self.objects.get(&id).ok_or(FsError::NotFound)?.len;
        let file = self.read_file(id, len)?;
        decode(file::payload(&file)?).map_err(FsError::Serialization)
    }

    /// The name read at mount, the first time; then reads the first block of the file.
    fn name(&mut self, id: ObjectId) -> Result<String, Self::Error> {
        let entry = self.objects.get_mut(&id).ok_or(FsError::NotFound)?;
        if let Some(name) = entry.name.take() {
            return Ok(name);
        }
        let len = entry.len;
        let block = self.read_file(id, BLOCK)?;
        Ok(file::name(&block, len)?.into())
    }

    /// A file already gone counts as removed.
    fn remove(&mut self, id: ObjectId) -> Result<(), Self::Error> {
        if !self.objects.contains_key(&id) {
            return Err(FsError::NotFound);
        }
        match fs::remove_file(self.path(id, Kind::Object)) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error.into()),
            _ => {}
        }
        let entry = self.objects.remove(&id).expect("checked above");
        self.used -= entry.len;
        self.sync_dir()
    }

    /// In whole 4 KiB blocks, headers and padding included. Nothing is left to reclaim: a file is
    /// freed when it is replaced or removed.
    fn usage(&self) -> Usage {
        Usage {
            capacity: self.capacity,
            used: self.used,
            free: self.capacity.saturating_sub(self.used),
            reclaimable: 0,
            objects: self.objects.len(),
        }
    }

    fn ids(&self) -> Vec<ObjectId> {
        self.objects.keys().copied().collect()
    }
}
