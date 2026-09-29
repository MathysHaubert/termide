//! Reading the table of contents and entry bytes out of zip and tar files.

use std::collections::HashSet;
use std::fs::File;
use std::io::{self, BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::format::{ArchiveFormat, TarCompression};
use super::index::{normalize_entry_name, ArchiveIndex, EntrySource, NodeKind};
use super::names;
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
    struct Entry {
        raw_name: Vec<u8>,
        cp437_name: String,
        is_dir: bool,
        is_symlink: bool,
        meta: VfsMetadata,
    }

    let mut entries = Vec::with_capacity(zip.len());
    for i in 0..zip.len() {
        let entry = zip.by_index_raw(i).map_err(zip_error)?;
        // The extended timestamp is exact UTC; the DOS field has no zone.
        let modified = entry
            .extra_data_fields()
            .find_map(|field| match field {
                zip::extra_fields::ExtraField::ExtendedTimestamp(ts) => ts.mod_time(),
                _ => None,
            })
            .map(|secs| UNIX_EPOCH + Duration::from_secs(u64::from(secs)))
            .or_else(|| entry.last_modified().and_then(dos_time));
        let file_type = if entry.is_dir() {
            VfsFileType::Directory
        } else if entry.is_symlink() {
            VfsFileType::Symlink
        } else {
            VfsFileType::File
        };
        entries.push(Entry {
            raw_name: entry.name_raw().to_vec(),
            cp437_name: entry.name().to_owned(),
            is_dir: entry.is_dir(),
            is_symlink: entry.is_symlink(),
            meta: metadata(file_type, entry.size(), modified, entry.unix_mode()),
        });
    }

    let legacy = names::guess(entries.iter().map(|e| e.raw_name.as_slice()));
    let mut index = ArchiveIndex::new();
    for (i, entry) in entries.into_iter().enumerate() {
        let name = names::decode(&entry.raw_name, &entry.cp437_name, legacy);
        let kind = if entry.is_dir {
            NodeKind::Dir
        } else if entry.is_symlink {
            // A zip symlink stores its target as the entry's content.
            let mut target = String::new();
            let file = zip.by_index(i).map_err(zip_error)?;
            file.take(MAX_LINK_TARGET)
                .read_to_string(&mut target)
                .map_err(|e| VfsError::Archive(format!("symlink {name:?}: {e}")))?;
            NodeKind::Symlink(target)
        } else {
            NodeKind::File(EntrySource::Zip(i))
        };
        index.insert(&name, kind, entry.meta);
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

/// The proleptic Gregorian date `days` after 1970-01-01 as (year, month,
/// day) — the inverse of [`days_from_civil`].
pub(crate) fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let mp = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
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

    #[test]
    fn civil_from_days_inverts_days_from_civil() {
        for days in [-1, 0, 11_016, 11_017, 20_725, 47_540] {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days, "{y}-{m}-{d}");
        }
        assert_eq!(civil_from_days(20_725), (2026, 9, 29));
    }
}
