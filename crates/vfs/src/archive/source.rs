//! Reading the table of contents and entry bytes out of zip and tar files.

use std::collections::HashSet;
use std::fs::File;
use std::io::{self, BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::format::{ArchiveFormat, TarCompression};
use super::index::{normalize_entry_name, ArchiveIndex, EntrySource, NodeKind};
use crate::error::{VfsError, VfsResult};
use crate::types::{VfsFileType, VfsMetadata};

/// Symlink targets longer than this are treated as corrupt.
const MAX_LINK_TARGET: u64 = 4096;

const READ_BUFFER: usize = 256 * 1024;

type ZipReader = zip::ZipArchive<BufReader<File>>;

/// Access to the entries of an opened archive file.
pub(crate) enum Backend {
    /// Zip keeps its parsed central directory, so any entry can be reached
    /// directly. The mutex serialises readers over the shared file handle.
    Zip(Mutex<ZipReader>),
    /// Tar has no directory: reaching an entry means streaming from the start.
    Tar {
        path: PathBuf,
        compression: TarCompression,
    },
}

/// Read the table of contents of `path`.
pub(crate) fn open(path: &Path, format: ArchiveFormat) -> VfsResult<(ArchiveIndex, Backend)> {
    match format {
        ArchiveFormat::Zip => {
            let mut zip =
                zip::ZipArchive::new(BufReader::new(File::open(path)?)).map_err(zip_error)?;
            let index = index_zip(&mut zip)?;
            Ok((index, Backend::Zip(Mutex::new(zip))))
        }
        ArchiveFormat::Tar(compression) => {
            let index = index_tar(path, compression)?;
            Ok((
                index,
                Backend::Tar {
                    path: path.to_path_buf(),
                    compression,
                },
            ))
        }
    }
}

impl Backend {
    /// Call `visit` with a reader for each of `sources`. Zip entries are
    /// visited in the given order; tar entries in stream order, in one pass
    /// that stops once every requested entry has been seen.
    pub(crate) fn read_sources(
        &self,
        sources: &[EntrySource],
        mut visit: impl FnMut(EntrySource, &mut dyn Read) -> VfsResult<()>,
    ) -> VfsResult<()> {
        match self {
            Backend::Zip(zip) => {
                for &source in sources {
                    let EntrySource::Zip(i) = source else {
                        continue;
                    };
                    let mut zip = zip
                        .lock()
                        .map_err(|_| VfsError::Archive("zip reader lock poisoned".into()))?;
                    let mut entry = zip.by_index(i).map_err(zip_error)?;
                    visit(source, &mut ArchiveRead(&mut entry))?;
                }
                Ok(())
            }
            Backend::Tar { path, compression } => {
                let mut wanted: HashSet<usize> = sources
                    .iter()
                    .filter_map(|s| match s {
                        EntrySource::Tar(ordinal) => Some(*ordinal),
                        EntrySource::Zip(_) => None,
                    })
                    .collect();
                if wanted.is_empty() {
                    return Ok(());
                }
                let mut archive = tar::Archive::new(tar_stream(path, *compression)?);
                for (ordinal, entry) in archive.entries().map_err(tar_error)?.enumerate() {
                    let mut entry = entry.map_err(tar_error)?;
                    if wanted.remove(&ordinal) {
                        visit(EntrySource::Tar(ordinal), &mut ArchiveRead(&mut entry))?;
                        if wanted.is_empty() {
                            return Ok(());
                        }
                    }
                }
                Err(VfsError::Archive(
                    "archive ended before all entries were read".into(),
                ))
            }
        }
    }
}

/// Wraps a reader over archive data so its failures surface as
/// [`VfsError::Archive`], not as I/O errors on the destination.
struct ArchiveRead<'a, R: Read + ?Sized>(&'a mut R);

impl<R: Read + ?Sized> Read for ArchiveRead<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0
            .read(buf)
            .map_err(|e| io::Error::other(ArchiveReadError(e)))
    }
}

/// Marker error: the source archive failed, not the destination.
#[derive(Debug)]
pub(crate) struct ArchiveReadError(io::Error);

impl std::fmt::Display for ArchiveReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::error::Error for ArchiveReadError {}

/// Map an I/O error from a copy loop, telling archive failures apart from
/// failures writing the destination.
pub(crate) fn copy_error(e: io::Error) -> VfsError {
    match e
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<ArchiveReadError>())
    {
        Some(inner) => VfsError::Archive(inner.to_string()),
        None => VfsError::Io(e),
    }
}

fn index_zip(zip: &mut ZipReader) -> VfsResult<ArchiveIndex> {
    let mut index = ArchiveIndex::new();
    for i in 0..zip.len() {
        let (name, is_dir, is_symlink, meta) = {
            let entry = zip.by_index_raw(i).map_err(zip_error)?;
            let modified = entry.last_modified().and_then(dos_time);
            let file_type = if entry.is_dir() {
                VfsFileType::Directory
            } else if entry.is_symlink() {
                VfsFileType::Symlink
            } else {
                VfsFileType::File
            };
            let meta = metadata(file_type, entry.size(), modified, entry.unix_mode());
            (
                entry.name().to_owned(),
                entry.is_dir(),
                entry.is_symlink(),
                meta,
            )
        };
        let kind = if is_dir {
            NodeKind::Dir
        } else if is_symlink {
            // A zip symlink stores its target as the entry's content.
            let mut target = String::new();
            let entry = zip.by_index(i).map_err(zip_error)?;
            entry
                .take(MAX_LINK_TARGET)
                .read_to_string(&mut target)
                .map_err(|e| VfsError::Archive(format!("symlink {name:?}: {e}")))?;
            NodeKind::Symlink(target)
        } else {
            NodeKind::File(EntrySource::Zip(i))
        };
        index.insert(&name, kind, meta);
    }
    Ok(index)
}

fn index_tar(path: &Path, compression: TarCompression) -> VfsResult<ArchiveIndex> {
    let mut index = ArchiveIndex::new();
    let mut archive = tar::Archive::new(tar_stream(path, compression)?);
    for (ordinal, entry) in archive.entries().map_err(tar_error)?.enumerate() {
        let entry = entry.map_err(tar_error)?;
        let header = entry.header();
        let name = String::from_utf8_lossy(&entry.path_bytes()).into_owned();
        let link = entry
            .link_name_bytes()
            .map(|l| String::from_utf8_lossy(&l).into_owned());
        let modified = header
            .mtime()
            .ok()
            .map(|secs| UNIX_EPOCH + Duration::from_secs(secs));
        let mode = header.mode().ok();

        use tar::EntryType as T;
        let (kind, file_type) = match header.entry_type() {
            T::Regular | T::Continuous | T::GNUSparse => {
                (NodeKind::File(EntrySource::Tar(ordinal)), VfsFileType::File)
            }
            T::Directory => (NodeKind::Dir, VfsFileType::Directory),
            T::Symlink => match link {
                Some(target) => (NodeKind::Symlink(target), VfsFileType::Symlink),
                None => continue,
            },
            T::Link => match link.as_deref().and_then(normalize_entry_name) {
                Some(target) => (NodeKind::HardLink(target), VfsFileType::File),
                None => continue,
            },
            // Devices, fifos and metadata-only records have nothing to browse.
            _ => continue,
        };
        let size = if file_type == VfsFileType::File {
            entry.size()
        } else {
            0
        };
        index.insert(&name, kind, metadata(file_type, size, modified, mode));
    }
    Ok(index)
}

/// The decompressed tar stream of `path`.
fn tar_stream(path: &Path, compression: TarCompression) -> VfsResult<Box<dyn Read>> {
    let file = BufReader::with_capacity(READ_BUFFER, File::open(path)?);
    Ok(match compression {
        TarCompression::None => Box::new(file),
        TarCompression::Gzip => Box::new(flate2::read::MultiGzDecoder::new(file)),
        TarCompression::Bzip2 => Box::new(bzip2::read::MultiBzDecoder::new(file)),
        TarCompression::Xz => Box::new(lzma_rust2::XzReader::new(file, true)),
        TarCompression::Zstd => Box::new(
            ruzstd::decoding::StreamingDecoder::new(file)
                .map_err(|e| VfsError::Archive(format!("zstd: {e}")))?,
        ),
    })
}

fn metadata(
    file_type: VfsFileType,
    size: u64,
    modified: Option<SystemTime>,
    mode: Option<u32>,
) -> VfsMetadata {
    VfsMetadata {
        file_type,
        size,
        modified,
        created: None,
        accessed: None,
        readonly: true,
        permissions: mode.map(|m| m & 0o7777),
    }
}

/// A zip timestamp as a point in time. DOS time carries no time zone; it is
/// read as UTC, which is exact for archives written by tools that do so and
/// off by the writer's offset otherwise.
fn dos_time(dt: zip::DateTime) -> Option<SystemTime> {
    let days = days_from_civil(
        i64::from(dt.year()),
        i64::from(dt.month()),
        i64::from(dt.day()),
    );
    let secs = days * 86_400
        + i64::from(dt.hour()) * 3600
        + i64::from(dt.minute()) * 60
        + i64::from(dt.second());
    u64::try_from(secs)
        .ok()
        .map(|s| UNIX_EPOCH + Duration::from_secs(s))
}

/// Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant's
/// `days_from_civil`).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn zip_error(e: zip::result::ZipError) -> VfsError {
    VfsError::Archive(e.to_string())
}

fn tar_error(e: io::Error) -> VfsError {
    VfsError::Archive(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_dates_count_days_from_the_epoch() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 3, 1), 11_017);
        assert_eq!(days_from_civil(2026, 9, 29), 20_725);
    }
}
