//! Positional reads, with the holes a sparse source has and the alignment a device needs.
//!
//! A reader asks a [`ReadAt`] for bytes at an offset, never for a stream, so any part can be
//! read in any order and from several threads at once. A source may report holes
//! ([`ReadAt::segment`]): a hole reads as zeros and need not be read at all. A raw device may
//! need aligned reads ([`ReadAt::alignment`]): [`read_at`] honours it with an aligned bounce
//! buffer when the caller's range or buffer is not aligned.
//!
//! Sources: [`FileSource`] (a file, read with `pread` on Unix and with a pool of handles on
//! Windows, where reads through one synchronous handle are serialized), [`Device`] (a raw volume
//! or disk: share modes, optional unbuffered reads, alignment), byte slices, and anything that
//! implements [`ReadAt`] (an archive member, a decrypted stream).

use std::{
    fs::File,
    io,
    path::{Path, PathBuf},
    sync::Arc,
};

/// A stretch of a source, from an offset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Segment {
    /// This many bytes of data.
    Data(u64),
    /// This many bytes of hole, which read as zeros.
    Hole(u64),
}

/// A source read by position.
pub trait ReadAt: Send + Sync {
    /// The source's length in bytes (`u64::MAX` when unknown, as for some raw devices).
    fn len(&self) -> u64;

    /// Whether the source is empty.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Fill `buf` from `offset`, holes as zeros. Reads may come from several threads at once.
    ///
    /// # Errors
    /// If the read fails, or `buf` runs past the end.
    fn read_exact_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()>;

    /// The segment starting at `offset`, and how far it runs; the default knows of no holes.
    /// Past the end, an empty data segment.
    ///
    /// # Errors
    /// If the source cannot tell.
    fn segment(&self, offset: u64) -> io::Result<Segment> {
        Ok(Segment::Data(self.len().saturating_sub(offset)))
    }

    /// The alignment reads need: offsets, lengths and buffer addresses multiples of it. 1 when
    /// any read is fine.
    fn alignment(&self) -> usize {
        1
    }
}

impl<T: ReadAt + ?Sized> ReadAt for &T {
    fn len(&self) -> u64 {
        (**self).len()
    }

    fn read_exact_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        (**self).read_exact_at(offset, buf)
    }

    fn segment(&self, offset: u64) -> io::Result<Segment> {
        (**self).segment(offset)
    }

    fn alignment(&self) -> usize {
        (**self).alignment()
    }
}

impl<T: ReadAt + ?Sized> ReadAt for Arc<T> {
    fn len(&self) -> u64 {
        (**self).len()
    }

    fn read_exact_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        (**self).read_exact_at(offset, buf)
    }

    fn segment(&self, offset: u64) -> io::Result<Segment> {
        (**self).segment(offset)
    }

    fn alignment(&self) -> usize {
        (**self).alignment()
    }
}

impl<T: ReadAt + ?Sized> ReadAt for Box<T> {
    fn len(&self) -> u64 {
        (**self).len()
    }

    fn read_exact_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        (**self).read_exact_at(offset, buf)
    }

    fn segment(&self, offset: u64) -> io::Result<Segment> {
        (**self).segment(offset)
    }

    fn alignment(&self) -> usize {
        (**self).alignment()
    }
}

impl ReadAt for [u8] {
    fn len(&self) -> u64 {
        self.len() as u64
    }

    fn read_exact_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        let bytes = usize::try_from(offset)
            .ok()
            .and_then(|start| self.get(start..)?.get(..buf.len()))
            .ok_or_else(|| io::Error::from(io::ErrorKind::UnexpectedEof))?;
        buf.copy_from_slice(bytes);
        Ok(())
    }
}

impl ReadAt for Vec<u8> {
    fn len(&self) -> u64 {
        self.as_slice().len() as u64
    }

    fn read_exact_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        self.as_slice().read_exact_at(offset, buf)
    }
}

/// A heap buffer whose start is aligned, for unbuffered device reads. Built from an
/// over-allocated `Vec` and `align_offset` (of pointers), without `unsafe`.
#[derive(Debug)]
pub struct AlignedBuf {
    storage: Vec<u8>,
    start: usize,
    len: usize,
}

impl AlignedBuf {
    /// `len` zero bytes starting at a multiple of `align` (a power of two; 1 for none).
    #[must_use]
    pub fn new(len: usize, align: usize) -> Self {
        let align = align.max(1).next_power_of_two();
        let storage = vec![0u8; len + align - 1];
        let start = storage.as_ptr().align_offset(align).min(align - 1);
        Self {
            storage,
            start,
            len,
        }
    }

    /// The buffer's length.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Whether the buffer is empty.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The bytes.
    #[must_use]
    pub fn as_slice(&self) -> &[u8] {
        self.storage
            .get(self.start..self.start + self.len)
            .unwrap_or_default()
    }

    /// The bytes, mutably.
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        self.storage
            .get_mut(self.start..self.start + self.len)
            .unwrap_or_default()
    }
}

/// Whether `offset`, `len` and the address of `buf` are multiples of `align`.
fn is_aligned(offset: u64, buf: &[u8], align: usize) -> bool {
    align <= 1
        || (offset.is_multiple_of(align as u64)
            && buf.len().is_multiple_of(align)
            && buf.as_ptr().align_offset(align) == 0)
}

/// Fill `buf` from `offset` of `source`, honouring its alignment: an unaligned range or buffer
/// is read through an aligned bounce buffer covering it.
///
/// # Errors
/// If the read fails.
pub fn read_at<S: ReadAt + ?Sized>(source: &S, offset: u64, buf: &mut [u8]) -> io::Result<()> {
    let align = source.alignment().max(1).next_power_of_two();
    if is_aligned(offset, buf, align) {
        return source.read_exact_at(offset, buf);
    }
    let align64 = align as u64;
    let start = offset - offset % align64;
    let end = offset
        .checked_add(buf.len() as u64)
        .and_then(|end| end.checked_next_multiple_of(align64))
        .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
    let len =
        usize::try_from(end - start).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    let mut bounce = AlignedBuf::new(len, align);
    source.read_exact_at(start, bounce.as_mut_slice())?;
    let skip = usize::try_from(offset - start)
        .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    let bytes = bounce
        .as_slice()
        .get(skip..skip + buf.len())
        .ok_or_else(|| io::Error::from(io::ErrorKind::UnexpectedEof))?;
    buf.copy_from_slice(bytes);
    Ok(())
}

/// Positional reads through one handle; on Windows, with spare handles reopened from the path
/// so reads from several threads proceed at once.
#[derive(Debug)]
struct Handles {
    file: File,
    #[cfg(windows)]
    path: Option<PathBuf>,
    #[cfg(windows)]
    spare: std::sync::Mutex<Vec<File>>,
    #[cfg(windows)]
    open: fn(&Path) -> io::Result<File>,
}

impl Handles {
    #[allow(unused_variables)]
    fn new(file: File, path: Option<PathBuf>, open: fn(&Path) -> io::Result<File>) -> Self {
        Self {
            file,
            #[cfg(windows)]
            path,
            #[cfg(windows)]
            spare: std::sync::Mutex::new(Vec::new()),
            #[cfg(windows)]
            open,
        }
    }

    #[cfg(unix)]
    fn read_exact_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        std::os::unix::fs::FileExt::read_exact_at(&self.file, buf, offset)
    }

    #[cfg(windows)]
    fn read_exact_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        // A spare handle if one is free (or can be opened), else the first one, whose reads
        // the system serializes.
        let spare = self
            .spare
            .lock()
            .ok()
            .and_then(|mut spare| spare.pop())
            .or_else(|| self.path.as_deref().and_then(|path| (self.open)(path).ok()));
        let result = read_windows(spare.as_ref().unwrap_or(&self.file), offset, buf);
        if let Some(handle) = spare
            && let Ok(mut pool) = self.spare.lock()
        {
            pool.push(handle);
        }
        result
    }

    #[cfg(not(any(unix, windows)))]
    fn read_exact_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        use std::io::{Read, Seek, SeekFrom};
        let mut file = &self.file;
        file.seek(SeekFrom::Start(offset))?;
        file.read_exact(buf)
    }
}

#[cfg(windows)]
fn read_windows(file: &File, mut offset: u64, mut buf: &mut [u8]) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        match file.seek_read(buf, offset) {
            Ok(0) => return Err(io::Error::from(io::ErrorKind::UnexpectedEof)),
            Ok(read) => {
                buf = buf.get_mut(read..).unwrap_or_default();
                offset += read as u64;
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// A file read by position (an exported `$MFT`, a disk image).
#[derive(Debug)]
pub struct FileSource {
    handles: Handles,
    len: u64,
}

impl FileSource {
    /// Opens `path` for reading.
    ///
    /// # Errors
    /// If it cannot be opened, or its length read.
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref();
        let file = File::open(path)?;
        let len = file.metadata()?.len();
        Ok(Self {
            handles: Handles::new(file, Some(path.to_owned()), |path| File::open(path)),
            len,
        })
    }

    /// Reads through `file`. On Windows its reads are serialized: prefer [`Self::open`].
    ///
    /// # Errors
    /// If its length cannot be read.
    pub fn new(file: File) -> io::Result<Self> {
        let len = file.metadata()?.len();
        Ok(Self {
            handles: Handles::new(file, None, |path| File::open(path)),
            len,
        })
    }
}

impl ReadAt for FileSource {
    fn len(&self) -> u64 {
        self.len
    }

    fn read_exact_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        self.handles.read_exact_at(offset, buf)
    }
}

/// How to open a [`Device`].
#[derive(Clone, Copy, Debug)]
pub struct DeviceOptions {
    /// Bypass the system cache (Windows: `FILE_FLAG_NO_BUFFERING`): faster full scans that do
    /// not evict everything else, at the cost of caching nothing for later reads.
    pub unbuffered: bool,
    /// The alignment reads use: a multiple of the sector size (4096 covers 512e and 4Kn).
    pub alignment: usize,
    /// The device's length, if known (raw volumes often cannot tell it).
    pub len: Option<u64>,
}

impl Default for DeviceOptions {
    fn default() -> Self {
        Self {
            unbuffered: false,
            alignment: 4096,
            len: None,
        }
    }
}

/// A raw volume or disk (`\\.\C:`, a shadow copy device, `/dev/sdb1`), read with sector-aligned
/// reads. On Windows it is opened with every share mode, so a live system volume stays usable.
#[derive(Debug)]
pub struct Device {
    handles: Handles,
    len: u64,
    alignment: usize,
}

#[cfg(windows)]
fn open_device(path: &Path, unbuffered: bool) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_SHARE_ALL: u32 = 0x7;
    const FILE_FLAG_NO_BUFFERING: u32 = 0x2000_0000;
    const FILE_FLAG_RANDOM_ACCESS: u32 = 0x1000_0000;
    let mut options = std::fs::OpenOptions::new();
    options.read(true).share_mode(FILE_SHARE_ALL);
    options.custom_flags(if unbuffered {
        FILE_FLAG_NO_BUFFERING
    } else {
        FILE_FLAG_RANDOM_ACCESS
    });
    options.open(path)
}

impl Device {
    /// Opens `path` as a device.
    ///
    /// # Errors
    /// If it cannot be opened.
    pub fn open(path: impl AsRef<Path>, options: DeviceOptions) -> io::Result<Self> {
        let path = path.as_ref();
        #[cfg(windows)]
        let (file, open): (File, fn(&Path) -> io::Result<File>) = if options.unbuffered {
            (open_device(path, true)?, |path| open_device(path, true))
        } else {
            (open_device(path, false)?, |path| open_device(path, false))
        };
        #[cfg(not(windows))]
        let (file, open): (File, fn(&Path) -> io::Result<File>) =
            (File::open(path)?, |path| File::open(path));
        let len = match options.len {
            Some(len) => len,
            None => device_len(&file).unwrap_or(u64::MAX),
        };
        Ok(Self {
            handles: Handles::new(file, Some(path.to_owned()), open),
            len,
            alignment: options.alignment.max(1).next_power_of_two(),
        })
    }
}

/// A device's length: its metadata's, or for a block device (whose metadata says 0) the end a
/// seek finds.
fn device_len(file: &File) -> io::Result<u64> {
    let len = file.metadata()?.len();
    if len > 0 {
        return Ok(len);
    }
    use std::io::{Seek, SeekFrom};
    let mut file = file;
    let end = file.seek(SeekFrom::End(0))?;
    file.seek(SeekFrom::Start(0))?;
    Ok(end)
}

impl ReadAt for Device {
    fn len(&self) -> u64 {
        self.len
    }

    fn read_exact_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        self.handles.read_exact_at(offset, buf)
    }

    fn alignment(&self) -> usize {
        self.alignment
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]

    use super::*;

    /// A source that requires 512-byte alignment of everything.
    struct Strict(Vec<u8>);

    impl ReadAt for Strict {
        fn len(&self) -> u64 {
            self.0.len() as u64
        }

        fn read_exact_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
            assert!(
                is_aligned(offset, buf, 512),
                "unaligned read reached the device"
            );
            self.0.read_exact_at(offset, buf)
        }

        fn alignment(&self) -> usize {
            512
        }
    }

    #[test]
    fn unaligned_reads_go_through_a_bounce_buffer() {
        let data: Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();
        let strict = Strict(data.clone());
        let mut buf = vec![0; 1000];
        read_at(&strict, 700, &mut buf).unwrap();
        assert_eq!(&buf[..], &data[700..1700]);
        let mut aligned = AlignedBuf::new(1024, 512);
        assert_eq!(aligned.as_slice().as_ptr().align_offset(512), 0);
        read_at(&strict, 1024, aligned.as_mut_slice()).unwrap();
        assert_eq!(aligned.as_slice(), &data[1024..2048]);
        assert!(read_at(&strict, 4000, &mut buf).is_err());
    }

    #[test]
    fn slices_read_by_position() {
        let data: Vec<u8> = (0..32).collect();
        let mut buf = [0; 4];
        ReadAt::read_exact_at(&data[..], 30, &mut buf[..2]).unwrap();
        assert_eq!(buf[..2], [30, 31]);
        assert!(ReadAt::read_exact_at(&data[..], 30, &mut buf).is_err());
        assert_eq!(data[..].segment(8).unwrap(), Segment::Data(24));
        assert_eq!(ReadAt::len(&Arc::new(data.clone())), 32);
    }

    #[test]
    fn files_read_by_position() {
        let dir = std::env::temp_dir().join(format!("ntfs-ng-source-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("data");
        let data: Vec<u8> = (0..8192u32).map(|i| (i % 253) as u8).collect();
        std::fs::write(&path, &data).unwrap();
        let file = FileSource::open(&path).unwrap();
        assert_eq!(file.len(), 8192);
        let mut buf = vec![0; 100];
        file.read_exact_at(5000, &mut buf).unwrap();
        assert_eq!(&buf[..], &data[5000..5100]);
        assert!(file.read_exact_at(8150, &mut buf).is_err());
        let device = Device::open(&path, DeviceOptions::default()).unwrap();
        assert_eq!(device.len(), 8192);
        read_at(&device, 4097, &mut buf).unwrap();
        assert_eq!(&buf[..], &data[4097..4197]);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
