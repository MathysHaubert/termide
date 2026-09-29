//! ISO 9660 images, with the Joliet and Rock Ridge extensions.
//!
//! An image carries up to two directory trees: the primary one, which Rock
//! Ridge extends with POSIX names, modes, times and symlinks, and a Joliet
//! one with UCS-2 names. The primary tree is read when its Rock Ridge
//! records name the files, the Joliet tree otherwise (`hdiutil` writes Rock
//! Ridge modes and times but keeps the long names in Joliet), and the
//! primary tree with its ISO 9660 names as the last resort.
//! UDF, the file system of Windows and video discs, is not read: such an
//! image shows its ISO 9660 bridge, usually a lone README.
//!
//! Everything in the image is untrusted. Directory extents must lie inside
//! the image, each is read once (a crafted tree may loop), and the bytes of
//! directories and continuation areas read in total are capped. File
//! extents are checked only when read, so a truncated image still lists.

use std::collections::HashSet;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::index::{ArchiveIndex, EntrySource, NodeKind};
use super::source::{days_from_civil, metadata};
use crate::error::{VfsError, VfsResult};
use crate::types::VfsFileType;

const SECTOR: u64 = 2048;
/// The first volume descriptor follows the 16-sector system area.
const FIRST_DESCRIPTOR: u64 = 16;
/// Volume descriptors read before giving up on a terminator.
const MAX_DESCRIPTORS: u64 = 64;
/// Directory and continuation bytes read in total; far above the tree of
/// any real image, and a bound on what a crafted one can make us read.
const MAX_TREE_BYTES: u64 = 256 << 20;
/// Continuation areas followed for one directory record.
const MAX_CONTINUATIONS: usize = 32;
/// Symlink targets longer than this are treated as corrupt.
const MAX_LINK_TARGET: usize = 4096;

const FLAG_DIRECTORY: u8 = 0x02;
/// An associated file: a Macintosh resource fork, not a file of its own.
const FLAG_ASSOCIATED: u8 = 0x04;
/// More extents of the same file follow in the next records.
const FLAG_MULTI_EXTENT: u8 = 0x80;

/// A contiguous run of a file's bytes in the image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Extent {
    start: u64,
    len: u64,
}

/// Which directory tree of the image to read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tree {
    RockRidge,
    Joliet,
    Plain,
}

/// Read the table of contents of the image at `path`. The index refers to
/// files by their position in the returned extent lists.
pub(crate) fn open(path: &Path) -> VfsResult<(ArchiveIndex, Vec<Vec<Extent>>)> {
    open_tree(path, None)
}

/// [`open`], with the tree forced instead of chosen by preference.
pub(crate) fn open_tree(
    path: &Path,
    forced: Option<Tree>,
) -> VfsResult<(ArchiveIndex, Vec<Vec<Extent>>)> {
    let file = File::open(path)?;
    let len = file.metadata()?.len();
    let mut image = Image {
        file,
        len,
        budget: MAX_TREE_BYTES,
    };
    let volumes = read_descriptors(&mut image)?;

    let primary = volumes.primary;
    let rock_ridge = match forced {
        Some(Tree::Joliet | Tree::Plain) => None,
        Some(Tree::RockRidge) | None => rock_ridge(&mut image, primary)?,
    };
    let (tree, root) = match (forced, rock_ridge, volumes.joliet) {
        (Some(Tree::RockRidge), None, _) => return Err(corrupt("no Rock Ridge tree")),
        (Some(Tree::RockRidge), Some(_), _) => (Tree::RockRidge, primary),
        (Some(Tree::Joliet), _, None) => return Err(corrupt("no Joliet tree")),
        (Some(Tree::Joliet), _, Some(joliet)) => (Tree::Joliet, joliet),
        (Some(Tree::Plain), _, _) | (None, None, None) => (Tree::Plain, primary),
        (None, Some(rr), Some(joliet)) if !rr.names => (Tree::Joliet, joliet),
        (None, Some(_), _) => (Tree::RockRidge, primary),
        (None, None, Some(joliet)) => (Tree::Joliet, joliet),
    };
    let mut walk = Walk {
        image,
        tree,
        skip: rock_ridge.map_or(0, |rr| rr.skip),
        index: ArchiveIndex::new(),
        files: Vec::new(),
    };
    walk.run(root)?;
    Ok((walk.index, walk.files))
}

/// Where a directory's records live.
#[derive(Debug, Clone, Copy)]
struct DirExtent {
    start: u64,
    len: u64,
}

struct Volumes {
    primary: DirExtent,
    joliet: Option<DirExtent>,
}

struct Image {
    file: File,
    len: u64,
    /// Tree bytes still allowed to be read.
    budget: u64,
}

impl Image {
    /// `len` bytes at `start`, counted against the tree budget.
    fn read(&mut self, start: u64, len: u64) -> VfsResult<Vec<u8>> {
        if start.checked_add(len).is_none_or(|end| end > self.len) {
            return Err(corrupt("a directory lies outside the image"));
        }
        self.budget = self
            .budget
            .checked_sub(len)
            .ok_or_else(|| corrupt("the directory tree is too large"))?;
        let mut buf = vec![0; len as usize];
        self.file.seek(SeekFrom::Start(start))?;
        self.file.read_exact(&mut buf)?;
        Ok(buf)
    }
}

fn read_descriptors(image: &mut Image) -> VfsResult<Volumes> {
    let mut primary = None;
    let mut joliet = None;
    for n in FIRST_DESCRIPTOR..FIRST_DESCRIPTOR + MAX_DESCRIPTORS {
        let Ok(sector) = image.read(n * SECTOR, SECTOR) else {
            break;
        };
        if &sector[1..6] != b"CD001" {
            break;
        }
        match sector[0] {
            1 if primary.is_none() => primary = Some(volume_root(&sector)?),
            2 if is_joliet(&sector) && joliet.is_none() => joliet = Some(volume_root(&sector)?),
            255 => break,
            _ => {}
        }
    }
    Ok(Volumes {
        primary: primary.ok_or_else(|| corrupt("no primary volume descriptor"))?,
        joliet,
    })
}

/// A supplementary descriptor is Joliet when its escape sequences name one
/// of the three UCS-2 levels.
fn is_joliet(sector: &[u8]) -> bool {
    matches!(&sector[88..91], b"%/@" | b"%/C" | b"%/E")
}

fn volume_root(sector: &[u8]) -> VfsResult<DirExtent> {
    if le16(sector, 128) != Some(SECTOR as u16) {
        return Err(VfsError::Archive(
            "ISO images with a block size other than 2048 are not supported".into(),
        ));
    }
    let root = Record::parse(&sector[156..190]).ok_or_else(|| corrupt("bad root directory"))?;
    Ok(root.dir_extent())
}

/// Rock Ridge in the primary tree, as its root announces it.
#[derive(Debug, Clone, Copy)]
struct RockRidge {
    /// Bytes to skip at the start of every other record's system use area.
    skip: usize,
    /// The root's entries carry `NM` names.
    names: bool,
}

/// The root's `.` record starts with an `SP` entry, followed by Rock Ridge
/// entries or the `ER` entry naming the extension.
fn rock_ridge(image: &mut Image, root: DirExtent) -> VfsResult<Option<RockRidge>> {
    let data = image.read(root.start, root.len.min(SECTOR))?;
    let mut records = records(&data);
    let Some(dot) = records.next() else {
        return Ok(None);
    };
    let susp = read_susp(image, dot.system_use)?;
    let Some(skip) = susp.sp_skip.filter(|_| susp.rock_ridge) else {
        return Ok(None);
    };
    let mut names = false;
    for record in records.filter(|r| !r.is_dot()) {
        let area = record.system_use.get(skip..).unwrap_or(&[]);
        if read_susp(image, area)?.name.is_some() {
            names = true;
            break;
        }
    }
    Ok(Some(RockRidge { skip, names }))
}

/// One directory record, borrowed from the directory's bytes.
struct Record<'a> {
    extent: u64,
    size: u64,
    flags: u8,
    recorded: Option<SystemTime>,
    id: &'a [u8],
    system_use: &'a [u8],
}

impl<'a> Record<'a> {
    fn parse(buf: &'a [u8]) -> Option<Self> {
        let len = usize::from(*buf.first()?);
        let rec = buf.get(..len)?;
        let id_len = usize::from(*rec.get(32)?);
        let id = rec.get(33..33 + id_len)?;
        // A padding byte keeps the system use area at an even offset.
        let system_use = rec.get(33 + id_len + (id_len + 1) % 2..).unwrap_or(&[]);
        // Extended attributes occupy the first blocks of the extent.
        let ext_attr_blocks = u64::from(rec[1]);
        Some(Self {
            extent: (u64::from(le32(rec, 2)?) + ext_attr_blocks) * SECTOR,
            size: u64::from(le32(rec, 10)?),
            flags: rec[25],
            recorded: short_time(&rec[18..25]),
            id,
            system_use,
        })
    }

    fn dir_extent(&self) -> DirExtent {
        DirExtent {
            start: self.extent,
            len: self.size,
        }
    }

    /// The `.` and `..` records every directory starts with.
    fn is_dot(&self) -> bool {
        matches!(self.id, [0] | [1])
    }
}

/// The records of a directory. A record never crosses a sector boundary;
/// a zero length byte pads the rest of a sector.
fn records(data: &[u8]) -> impl Iterator<Item = Record<'_>> {
    data.chunks(SECTOR as usize).flat_map(|sector| {
        let mut rest = sector;
        std::iter::from_fn(move || {
            let record = Record::parse(rest)?;
            rest = &rest[usize::from(rest[0])..];
            Some(record)
        })
    })
}

/// What the System Use Sharing Protocol entries of one record say.
#[derive(Default)]
struct Susp {
    /// `SP`: the skip length of the other records.
    sp_skip: Option<usize>,
    /// Any Rock Ridge entry or an `ER` entry was seen.
    rock_ridge: bool,
    name: Option<Vec<u8>>,
    symlink: Option<String>,
    mode: Option<u32>,
    modified: Option<SystemTime>,
    /// `CL`: a relocated directory stands here; its extent block.
    child_link: Option<u64>,
    /// `RE`: this is a relocated directory's real record, listed through
    /// its `CL` placeholder instead.
    relocated: bool,
}

fn read_susp(image: &mut Image, area: &[u8]) -> VfsResult<Susp> {
    let mut susp = Susp::default();
    let mut link = SymlinkBuilder::default();
    let mut owned;
    let mut area = area;
    for _ in 0..=MAX_CONTINUATIONS {
        let mut next = None;
        let mut rest = area;
        while rest.len() >= 4 {
            let len = usize::from(rest[2]);
            if len < 4 || len > rest.len() {
                break;
            }
            let sig = &rest[..2];
            let data = &rest[4..len];
            rest = &rest[len..];
            match sig {
                b"SP" if data.len() >= 3 && data[..2] == [0xbe, 0xef] => {
                    susp.sp_skip = Some(usize::from(data[2]));
                }
                b"CE" => {
                    next = le32(data, 0)
                        .zip(le32(data, 8))
                        .zip(le32(data, 16))
                        .map(|((block, offset), len)| (block, offset, len));
                }
                b"ST" => break,
                b"ER" | b"RR" => susp.rock_ridge = true,
                b"PX" => {
                    susp.rock_ridge = true;
                    susp.mode = le32(data, 0);
                }
                b"NM" if !data.is_empty() => {
                    susp.rock_ridge = true;
                    // The CURRENT and PARENT flags stand for `.` and `..`.
                    if data[0] & 0x06 == 0 {
                        susp.name
                            .get_or_insert_with(Vec::new)
                            .extend_from_slice(&data[1..]);
                    }
                }
                b"SL" if !data.is_empty() => {
                    susp.rock_ridge = true;
                    link.push_components(&data[1..]);
                }
                b"TF" if !data.is_empty() => {
                    susp.rock_ridge = true;
                    susp.modified = tf_modified(data);
                }
                b"CL" => {
                    susp.rock_ridge = true;
                    susp.child_link = le32(data, 0).map(u64::from);
                }
                b"RE" => {
                    susp.rock_ridge = true;
                    susp.relocated = true;
                }
                _ => {}
            }
        }
        let Some((block, offset, len)) = next else {
            break;
        };
        // A continuation area lies within one block.
        if u64::from(offset) + u64::from(len) > SECTOR {
            break;
        }
        owned = image.read(
            u64::from(block) * SECTOR + u64::from(offset),
            u64::from(len),
        )?;
        area = &owned;
    }
    susp.symlink = link.finish();
    Ok(susp)
}

/// A symlink target assembled from `SL` component records, which may
/// split one component over several records and several `SL` entries.
#[derive(Default)]
struct SymlinkBuilder {
    target: Option<String>,
    /// The last component continues in the next one.
    continued: bool,
}

impl SymlinkBuilder {
    fn push_components(&mut self, mut data: &[u8]) {
        while data.len() >= 2 {
            let (flags, len) = (data[0], usize::from(data[1]));
            let Some(content) = data.get(2..2 + len) else {
                break;
            };
            data = &data[2 + len..];
            let target = self.target.get_or_insert_with(String::new);
            if !self.continued && !target.is_empty() && !target.ends_with('/') {
                target.push('/');
            }
            if flags & 0x02 != 0 {
                target.push('.');
            } else if flags & 0x04 != 0 {
                target.push_str("..");
            } else if flags & 0x08 != 0 {
                target.push('/');
            } else {
                target.push_str(&String::from_utf8_lossy(content));
            }
            self.continued = flags & 0x01 != 0;
        }
    }

    fn finish(self) -> Option<String> {
        self.target.filter(|t| t.len() <= MAX_LINK_TARGET)
    }
}

/// The modification time of a `TF` entry, which follows the creation time
/// when that is recorded.
fn tf_modified(data: &[u8]) -> Option<SystemTime> {
    let flags = data[0];
    let long = flags & 0x80 != 0;
    let size = if long { 17 } else { 7 };
    if flags & 0x02 == 0 {
        return None;
    }
    let offset = 1 + if flags & 0x01 != 0 { size } else { 0 };
    let stamp = data.get(offset..offset + size)?;
    if long {
        long_time(stamp)
    } else {
        short_time(stamp)
    }
}

struct Walk {
    image: Image,
    tree: Tree,
    skip: usize,
    index: ArchiveIndex,
    files: Vec<Vec<Extent>>,
}

impl Walk {
    fn run(&mut self, root: DirExtent) -> VfsResult<()> {
        let mut seen = HashSet::from([root.start]);
        // An explicit stack: a crafted image may nest directories deeper
        // than the call stack allows.
        let mut stack = vec![(root, String::new())];
        let mut first = true;
        while let Some((dir, path)) = stack.pop() {
            let data = match self.image.read(dir.start, dir.len) {
                Ok(data) => data,
                Err(e) if first => return Err(e),
                Err(e) => {
                    log::warn!("iso: skipping directory {path:?}: {e}");
                    continue;
                }
            };
            first = false;
            self.read_dir(&data, &path, &mut |sub, name| {
                if seen.insert(sub.start) {
                    stack.push((sub, name));
                } else {
                    log::warn!("iso: {name:?} repeats a directory, not descending");
                }
            })?;
        }
        Ok(())
    }

    fn read_dir(
        &mut self,
        data: &[u8],
        parent: &str,
        descend: &mut dyn FnMut(DirExtent, String),
    ) -> VfsResult<()> {
        // Extents of a multi-extent file seen so far, with its identifier.
        let mut partial: Option<(Vec<u8>, Vec<Extent>)> = None;
        for record in records(data) {
            if record.is_dot() || record.flags & FLAG_ASSOCIATED != 0 {
                continue;
            }
            let susp = match self.tree {
                Tree::RockRidge => read_susp(
                    &mut self.image,
                    record.system_use.get(self.skip..).unwrap_or(&[]),
                )?,
                Tree::Joliet | Tree::Plain => Susp::default(),
            };
            if susp.relocated {
                continue;
            }
            let is_dir = record.flags & FLAG_DIRECTORY != 0 || susp.child_link.is_some();
            let name = match &susp.name {
                Some(name) => String::from_utf8_lossy(name).into_owned(),
                None => self.record_name(record.id, is_dir),
            };
            if name.is_empty() {
                continue;
            }
            let path = format!("{parent}/{name}");
            let modified = susp.modified.or(record.recorded);

            if let Some(target) = susp.symlink {
                let meta = metadata(VfsFileType::Symlink, 0, modified, susp.mode);
                self.index.insert(&path, NodeKind::Symlink(target), meta);
            } else if is_dir {
                let sub = match susp.child_link {
                    Some(block) => self.relocated_dir(block)?,
                    None => Some(record.dir_extent()),
                };
                let meta = metadata(VfsFileType::Directory, 0, modified, susp.mode);
                if self.index.insert(&path, NodeKind::Dir, meta) {
                    if let Some(sub) = sub {
                        descend(sub, path);
                    }
                }
            } else {
                let extent = Extent {
                    start: record.extent,
                    len: record.size,
                };
                let mut extents = match partial.take() {
                    Some((id, extents)) if id == record.id => extents,
                    _ => Vec::new(),
                };
                extents.push(extent);
                if record.flags & FLAG_MULTI_EXTENT != 0 {
                    partial = Some((record.id.to_vec(), extents));
                    continue;
                }
                let size = extents.iter().map(|e| e.len).sum();
                let meta = metadata(VfsFileType::File, size, modified, susp.mode);
                let source = EntrySource::Iso(self.files.len());
                if self.index.insert(&path, NodeKind::File(source), meta) {
                    self.files.push(extents);
                }
            }
        }
        Ok(())
    }

    /// The extent of a directory Rock Ridge moved to keep the ISO 9660 tree
    /// within eight levels, read from its own `.` record.
    fn relocated_dir(&mut self, block: u64) -> VfsResult<Option<DirExtent>> {
        let Ok(sector) = self.image.read(block * SECTOR, SECTOR) else {
            return Ok(None);
        };
        Ok(Record::parse(&sector).map(|dot| dot.dir_extent()))
    }

    /// A name from the record's identifier, without the `;1` version and,
    /// for a file without an extension, the trailing dot.
    fn record_name(&self, id: &[u8], is_dir: bool) -> String {
        let mut name = match self.tree {
            Tree::Joliet => {
                let units = id.chunks_exact(2).map(|c| u16::from_be_bytes([c[0], c[1]]));
                char::decode_utf16(units)
                    .map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER))
                    .collect()
            }
            Tree::RockRidge | Tree::Plain => String::from_utf8_lossy(id).into_owned(),
        };
        if !is_dir {
            if let Some(i) = name.rfind(';') {
                name.truncate(i);
            }
            if name.len() > 1 && name.ends_with('.') {
                name.pop();
            }
        }
        name
    }
}

/// Reads a file's extents in order.
pub(crate) struct ExtentReader<'a> {
    file: &'a mut File,
    extents: &'a [Extent],
    /// Bytes of the current extent already read.
    pos: u64,
    positioned: bool,
}

impl<'a> ExtentReader<'a> {
    pub(crate) fn new(file: &'a mut File, extents: &'a [Extent]) -> Self {
        Self {
            file,
            extents,
            pos: 0,
            positioned: false,
        }
    }
}

impl Read for ExtentReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            let Some(extent) = self.extents.first() else {
                return Ok(0);
            };
            if self.pos == extent.len {
                self.extents = &self.extents[1..];
                self.pos = 0;
                self.positioned = false;
                continue;
            }
            if buf.is_empty() {
                return Ok(0);
            }
            if !self.positioned {
                self.file.seek(SeekFrom::Start(extent.start + self.pos))?;
                self.positioned = true;
            }
            let want = buf
                .len()
                .min(usize::try_from(extent.len - self.pos).unwrap_or(usize::MAX));
            let n = self.file.read(&mut buf[..want])?;
            if n == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "the image ends inside a file",
                ));
            }
            self.pos += n as u64;
            return Ok(n);
        }
    }
}

/// A 7-byte directory record time: years since 1900, month, day, hour,
/// minute, second, and the offset from UTC in 15-minute steps.
fn short_time(b: &[u8]) -> Option<SystemTime> {
    let &[year, month, day, hour, minute, second, offset] = b else {
        return None;
    };
    to_system_time(
        1900 + i64::from(year),
        [month, day, hour, minute, second].map(i64::from),
        offset as i8,
    )
}

/// A 17-byte volume descriptor time, `YYYYMMDDHHMMSScc` in ASCII digits
/// followed by the offset from UTC in 15-minute steps.
fn long_time(b: &[u8]) -> Option<SystemTime> {
    let digits = b.get(..16)?;
    let number = |range: std::ops::Range<usize>| -> Option<i64> {
        std::str::from_utf8(&digits[range]).ok()?.parse().ok()
    };
    to_system_time(
        number(0..4)?,
        [
            number(4..6)?,
            number(6..8)?,
            number(8..10)?,
            number(10..12)?,
            number(12..14)?,
        ],
        *b.get(16)? as i8,
    )
}

fn to_system_time(
    year: i64,
    [month, day, hour, minute, second]: [i64; 5],
    offset: i8,
) -> Option<SystemTime> {
    // An unrecorded time is all zeros.
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    let secs = days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second
        - i64::from(offset) * 900;
    u64::try_from(secs)
        .ok()
        .map(|s| UNIX_EPOCH + Duration::from_secs(s))
}

fn le16(buf: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(buf.get(at..at + 2)?.try_into().ok()?))
}

/// The little-endian half of a both-endian field.
fn le32(buf: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(buf.get(at..at + 4)?.try_into().ok()?))
}

fn corrupt(what: &str) -> VfsError {
    VfsError::Archive(format!("ISO image: {what}"))
}

#[cfg(test)]
mod tests;
