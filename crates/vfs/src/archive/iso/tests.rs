use std::path::PathBuf;

use super::*;
use crate::archive::ArchiveProvider;
use crate::traits::VfsProvider;
use crate::types::{ConnectOptions, VfsPath};

/// Written by `hdiutil makehybrid -iso -joliet`: Rock Ridge with modes and
/// times but ISO 9660 names, the real names in Joliet. The tree:
/// `readme.txt` (modified 2024-05-06 07:08:09 UTC), `bin/tool` (0755),
/// `docs/Long File Name with spaces.md`, `docs/Привет.txt`,
/// `lib/libx.so.1`, `lib/libx.so` (a symlink, stored as its target text)
/// and `deep/1/…/9/deep.txt`.
const HDIUTIL: &[u8] = include_bytes!("../fixtures/hdiutil.iso.gz");

fn fixture(dir: &Path) -> PathBuf {
    let mut image = Vec::new();
    flate2::read::GzDecoder::new(HDIUTIL)
        .read_to_end(&mut image)
        .unwrap();
    write(dir, "hdiutil.iso", &image)
}

fn write(dir: &Path, name: &str, image: &[u8]) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, image).unwrap();
    path
}

fn keys(index: &ArchiveIndex) -> Vec<String> {
    index
        .subtree(Path::new("/"))
        .map(|(key, _)| key.display().to_string())
        .collect()
}

fn node<'a>(index: &'a ArchiveIndex, path: &str) -> &'a super::super::index::Node {
    index
        .get(Path::new(path))
        .unwrap_or_else(|| panic!("{path} missing"))
        .1
}

fn opened(path: &Path) -> ArchiveProvider {
    let mut provider = ArchiveProvider::new(VfsPath::local(path));
    provider.connect(ConnectOptions::default()).recv().unwrap();
    provider
}

fn read(provider: &ArchiveProvider, image: &Path, inner: &str) -> VfsResult<Vec<u8>> {
    provider
        .read_file(&VfsPath::archive(VfsPath::local(image), inner))
        .recv()
}

#[test]
fn an_hdiutil_image_reads_its_joliet_names() {
    let dir = tempfile::tempdir().unwrap();
    let path = fixture(dir.path());
    let (index, _) = open(&path).unwrap();
    let keys = keys(&index);
    for key in [
        "/readme.txt",
        "/bin/tool",
        "/docs/Long File Name with spaces.md",
        "/docs/Привет.txt",
        "/lib/libx.so.1",
        "/deep/1/2/3/4/5/6/7/8/9/deep.txt",
    ] {
        assert!(keys.iter().any(|k| k == key), "{key} not in {keys:?}");
    }
    let readme = node(&index, "/readme.txt");
    assert_eq!(readme.meta.size, 23);
    assert_eq!(
        readme.meta.modified,
        Some(UNIX_EPOCH + Duration::from_secs(1_714_979_289))
    );

    let provider = opened(&path);
    assert_eq!(
        read(&provider, &path, "/readme.txt").unwrap(),
        b"hello from the archive\n"
    );
    assert_eq!(
        read(&provider, &path, "/docs/Привет.txt").unwrap(),
        "привет\n".as_bytes()
    );
    assert_eq!(
        read(&provider, &path, "/deep/1/2/3/4/5/6/7/8/9/deep.txt").unwrap(),
        b"deep\n"
    );

    let out = dir.path().join("out");
    provider
        .download(&VfsPath::archive(VfsPath::local(&path), "/docs"), &out)
        .recv()
        .unwrap();
    assert_eq!(
        std::fs::read(out.join("Long File Name with spaces.md")).unwrap(),
        b"# notes\n"
    );
}

#[test]
fn every_tree_of_an_image_can_be_read() {
    let dir = tempfile::tempdir().unwrap();
    let path = fixture(dir.path());

    let (index, _) = open_tree(&path, Some(Tree::RockRidge)).unwrap();
    assert_eq!(node(&index, "/BIN/TOOL").meta.permissions, Some(0o755));
    assert_eq!(node(&index, "/README.TXT").meta.permissions, Some(0o644));

    let (index, _) = open_tree(&path, Some(Tree::Plain)).unwrap();
    assert!(keys(&index).contains(&"/DOCS/LONG FILE NAME WITH SPACES.MD".into()));
    assert_eq!(node(&index, "/README.TXT").meta.permissions, None);
}

#[test]
fn a_damaged_image_fails_or_lists_but_never_panics() {
    let dir = tempfile::tempdir().unwrap();
    let path = fixture(dir.path());
    let original = std::fs::read(&path).unwrap();
    // The descriptors and the directories hdiutil writes after them.
    let area = 16 * SECTOR as usize..60 * SECTOR as usize;
    let mut seed = 0x2545_f491_4f6c_dd1du64;
    for round in 0..300 {
        let mut image = original.clone();
        for _ in 0..1 + round % 8 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let at = area.start + (seed as usize) % area.len();
            image[at] = (seed >> 32) as u8;
        }
        let damaged = write(dir.path(), "damaged.iso", &image);
        for tree in [None, Some(Tree::Joliet), Some(Tree::Plain)] {
            let _ = open_tree(&damaged, tree);
        }
    }
}

/// A hand-made image: descriptors at sector 16, the rest placed by the test.
struct Image(Vec<u8>);

impl Image {
    fn new(root_lba: u32) -> Self {
        let mut image = Self(vec![0; 64 * SECTOR as usize]);
        let mut pvd = vec![0u8; SECTOR as usize];
        pvd[0] = 1;
        pvd[1..6].copy_from_slice(b"CD001");
        pvd[6] = 1;
        pvd[128..130].copy_from_slice(&2048u16.to_le_bytes());
        pvd[156..190].copy_from_slice(&record(&[0], root_lba, SECTOR as u32, FLAG_DIRECTORY, &[]));
        image.put(16, &pvd);
        image.put(17, &[255, b'C', b'D', b'0', b'0', b'1', 1]);
        image
    }

    fn put(&mut self, lba: u32, bytes: &[u8]) {
        let at = lba as usize * SECTOR as usize;
        self.0[at..at + bytes.len()].copy_from_slice(bytes);
    }

    /// A one-sector directory at `lba` holding `.`, `..` and `entries`.
    fn dir(&mut self, lba: u32, dot_use: &[u8], entries: &[Vec<u8>]) {
        let mut data = record(&[0], lba, SECTOR as u32, FLAG_DIRECTORY, dot_use);
        data.extend(record(&[1], lba, SECTOR as u32, FLAG_DIRECTORY, &[]));
        for entry in entries {
            data.extend(entry);
        }
        self.put(lba, &data);
    }

    fn save(&self, dir: &Path) -> PathBuf {
        write(dir, "made.iso", &self.0)
    }
}

fn record(id: &[u8], lba: u32, size: u32, flags: u8, system_use: &[u8]) -> Vec<u8> {
    let pad = (id.len() + 1) % 2;
    let mut r = vec![0u8; 33];
    r[0] = (33 + id.len() + pad + system_use.len()) as u8;
    r[2..6].copy_from_slice(&lba.to_le_bytes());
    r[6..10].copy_from_slice(&lba.to_be_bytes());
    r[10..14].copy_from_slice(&size.to_le_bytes());
    r[14..18].copy_from_slice(&size.to_be_bytes());
    r[18..25].copy_from_slice(&[124, 5, 6, 7, 8, 9, 0]);
    r[25] = flags;
    r[32] = id.len() as u8;
    r.extend(id);
    r.extend(std::iter::repeat_n(0, pad));
    r.extend(system_use);
    r
}

fn su(sig: &[u8; 2], data: &[u8]) -> Vec<u8> {
    let mut entry = vec![sig[0], sig[1], (4 + data.len()) as u8, 1];
    entry.extend(data);
    entry
}

fn both32(value: u32) -> Vec<u8> {
    let mut out = value.to_le_bytes().to_vec();
    out.extend(value.to_be_bytes());
    out
}

fn px(mode: u32) -> Vec<u8> {
    let mut data = both32(mode);
    data.extend(both32(1));
    data.extend(both32(0));
    data.extend(both32(0));
    su(b"PX", &data)
}

fn nm(flags: u8, name: &str) -> Vec<u8> {
    let mut data = vec![flags];
    data.extend(name.as_bytes());
    su(b"NM", &data)
}

/// The system use area of a Rock Ridge root's `.` record.
fn rr_dot() -> Vec<u8> {
    let mut area = su(b"SP", &[0xbe, 0xef, 0]);
    area.extend(px(0o40755));
    area
}

#[test]
fn rock_ridge_names_modes_times_and_links() {
    let dir = tempfile::tempdir().unwrap();
    let mut image = Image::new(20);

    // A name split over two NM entries, the second in a continuation area.
    let mut readme = nm(0x01, "read");
    readme.extend(px(0o100640));
    // TF with creation and modification, short form.
    readme.extend(su(
        b"TF",
        &[0x03, 100, 1, 1, 0, 0, 0, 0, 125, 1, 2, 3, 4, 5, 4],
    ));
    let mut ce = both32(30);
    ce.extend(both32(100));
    ce.extend(both32(64));
    readme.extend(su(b"CE", &ce));
    let mut continuation = nm(0, "me.txt");
    continuation.extend(su(b"ST", &[]));
    image.put(30, &[vec![0; 100], continuation].concat());

    // `lib/x` as two SL entries, the second continuing a split component,
    // and an absolute link.
    let mut relative = nm(0, "rel");
    relative.extend(px(0o120777));
    relative.extend(su(b"SL", &[0x01, 0x04, 0, 0x01, 1, b'l']));
    relative.extend(su(b"SL", &[0, 0, 2, b'i', b'b', 0, 1, b'x']));
    let mut absolute = nm(0, "abs");
    absolute.extend(su(b"SL", &[0, 0x08, 0, 0, 3, b'e', b't', b'c']));

    image.dir(
        20,
        &rr_dot(),
        &[
            record(b"README.TXT;1", 40, 5, 0, &readme),
            record(b"REL;1", 0, 0, 0, &relative),
            record(b"ABS;1", 0, 0, 0, &absolute),
        ],
    );
    image.put(40, b"hello");
    let path = image.save(dir.path());

    let (index, _) = open(&path).unwrap();
    assert_eq!(keys(&index), ["/", "/abs", "/readme.txt", "/rel"]);
    let readme = node(&index, "/readme.txt");
    assert_eq!(readme.meta.permissions, Some(0o640));
    // 2025-01-02 03:04:05 at UTC+01:00.
    assert_eq!(
        readme.meta.modified,
        Some(UNIX_EPOCH + Duration::from_secs(1_735_787_045 - 3600))
    );
    assert!(matches!(&node(&index, "/rel").kind, NodeKind::Symlink(t) if t == "../lib/x"));
    assert!(matches!(&node(&index, "/abs").kind, NodeKind::Symlink(t) if t == "/etc"));
    assert_eq!(
        read(&opened(&path), &path, "/readme.txt").unwrap(),
        b"hello"
    );
}

#[test]
fn a_relocated_directory_is_listed_where_it_belongs() {
    let dir = tempfile::tempdir().unwrap();
    let mut image = Image::new(20);
    let mut placeholder = nm(0, "deep");
    placeholder.extend(su(b"CL", &both32(22)));
    let mut moved = nm(0, "deep");
    moved.extend(su(b"RE", &[]));
    image.dir(
        20,
        &rr_dot(),
        &[
            record(b"DEEP;1", 0, 0, 0, &placeholder),
            record(
                b"RR_MOVED",
                21,
                SECTOR as u32,
                FLAG_DIRECTORY,
                &nm(0, "rr_moved"),
            ),
        ],
    );
    image.dir(
        21,
        &[],
        &[record(b"DEEP", 22, SECTOR as u32, FLAG_DIRECTORY, &moved)],
    );
    image.dir(
        22,
        &[],
        &[record(b"INNER.TXT;1", 40, 2, 0, &nm(0, "inner.txt"))],
    );
    let path = image.save(dir.path());

    let (index, _) = open(&path).unwrap();
    assert_eq!(keys(&index), ["/", "/deep", "/deep/inner.txt", "/rr_moved"]);
}

#[test]
fn a_multi_extent_file_reads_as_one() {
    let dir = tempfile::tempdir().unwrap();
    let mut image = Image::new(20);
    image.dir(
        20,
        &[],
        &[
            record(b"BIG.BIN;1", 40, SECTOR as u32, FLAG_MULTI_EXTENT, &[]),
            record(b"BIG.BIN;1", 45, 3, 0, &[]),
            record(b"NEXT.TXT;1", 46, 1, 0, &[]),
        ],
    );
    image.put(40, &[b'a'; SECTOR as usize]);
    image.put(45, b"xyz");
    image.put(46, b"n");
    let path = image.save(dir.path());

    let (index, _) = open(&path).unwrap();
    assert_eq!(keys(&index), ["/", "/BIG.BIN", "/NEXT.TXT"]);
    assert_eq!(node(&index, "/BIG.BIN").meta.size, SECTOR + 3);
    let provider = opened(&path);
    let big = read(&provider, &path, "/BIG.BIN").unwrap();
    assert_eq!(big.len(), SECTOR as usize + 3);
    assert!(big.ends_with(b"axyz"));
    assert_eq!(read(&provider, &path, "/NEXT.TXT").unwrap(), b"n");
}

#[test]
fn a_directory_loop_is_not_followed() {
    let dir = tempfile::tempdir().unwrap();
    let mut image = Image::new(20);
    image.dir(
        20,
        &[],
        &[record(b"SUB", 21, SECTOR as u32, FLAG_DIRECTORY, &[])],
    );
    image.dir(
        21,
        &[],
        &[record(b"BACK", 20, SECTOR as u32, FLAG_DIRECTORY, &[])],
    );
    let path = image.save(dir.path());

    let (index, _) = open(&path).unwrap();
    assert_eq!(keys(&index), ["/", "/SUB", "/SUB/BACK"]);
}

#[test]
fn extents_outside_the_image_fail_only_where_they_are_used() {
    let dir = tempfile::tempdir().unwrap();
    let mut image = Image::new(20);
    image.dir(
        20,
        &[],
        &[
            record(b"GONE", 10_000, SECTOR as u32, FLAG_DIRECTORY, &[]),
            record(b"CUT.BIN;1", 63, 2 * SECTOR as u32, 0, &[]),
            record(b"OK.TXT;1", 40, 2, 0, &[]),
        ],
    );
    image.put(40, b"ok");
    let path = image.save(dir.path());

    let (index, _) = open(&path).unwrap();
    assert_eq!(keys(&index), ["/", "/CUT.BIN", "/GONE", "/OK.TXT"]);
    let provider = opened(&path);
    assert!(matches!(
        read(&provider, &path, "/CUT.BIN"),
        Err(VfsError::Archive(_))
    ));
    assert_eq!(read(&provider, &path, "/OK.TXT").unwrap(), b"ok");

    let broken = write(dir.path(), "broken.iso", &Image::new(10_000).0);
    assert!(matches!(open(&broken), Err(VfsError::Archive(_))));
}

#[test]
fn record_times_carry_their_zone() {
    // 2024-05-06 07:08:09 at UTC+02:00, then unrecorded.
    assert_eq!(
        short_time(&[124, 5, 6, 7, 8, 9, 8]),
        Some(UNIX_EPOCH + Duration::from_secs(1_714_979_289 - 7200))
    );
    assert_eq!(short_time(&[0; 7]), None);
    assert_eq!(
        long_time(b"2024050607080900\x00"),
        Some(UNIX_EPOCH + Duration::from_secs(1_714_979_289))
    );
    assert_eq!(long_time(b"0000000000000000\x00"), None);
}
