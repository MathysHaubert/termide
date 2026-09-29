use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

use super::*;
use crate::VfsManager;

const README: &[u8] = b"hello from the archive\n";

fn big() -> Vec<u8> {
    (0..600_000u32).map(|i| (i % 251) as u8).collect()
}

fn names(entries: &[VfsEntry]) -> Vec<String> {
    entries.iter().map(|e| e.name.clone()).collect()
}

fn opened(archive: &Path) -> ArchiveProvider {
    let mut provider = ArchiveProvider::new(VfsPath::local(archive));
    provider.connect(ConnectOptions::default()).recv().unwrap();
    provider
}

fn at(archive: &Path, inner: &str) -> VfsPath {
    VfsPath::archive(VfsPath::local(archive), inner)
}

fn write_zip(path: &Path) {
    use zip::write::SimpleFileOptions;
    let mut zip = zip::ZipWriter::new(fs::File::create(path).unwrap());
    let deflated = SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .unix_permissions(0o640);
    zip.add_directory("docs/", SimpleFileOptions::default())
        .unwrap();
    zip.start_file("docs/readme.md", deflated).unwrap();
    zip.write_all(README).unwrap();
    // No explicit entry for `src/` or `src/deep/`.
    zip.start_file("src/deep/big.bin", deflated).unwrap();
    zip.write_all(&big()).unwrap();
    zip.add_symlink("docs/link.md", "readme.md", SimpleFileOptions::default())
        .unwrap();
    zip.finish().unwrap();
}

/// A tar stream with the same tree as [`write_zip`], plus a hard link.
fn tar_bytes() -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    append_dir(&mut builder, "docs/");
    append_file(&mut builder, "docs/readme.md", README);
    append_file(&mut builder, "src/deep/big.bin", &big());
    append_link(
        &mut builder,
        tar::EntryType::Symlink,
        "docs/link.md",
        "readme.md",
    );
    append_link(
        &mut builder,
        tar::EntryType::Link,
        "docs/hard.md",
        "docs/readme.md",
    );
    builder.into_inner().unwrap()
}

fn header(kind: tar::EntryType, size: u64) -> tar::Header {
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(kind);
    header.set_size(size);
    header.set_mode(0o640);
    header.set_mtime(1_700_000_000);
    header
}

/// Write the raw name, bypassing the builder's path checks, so tests can
/// plant names a hostile archive would carry.
fn set_raw_name(header: &mut tar::Header, name: &str) {
    let field = &mut header.as_old_mut().name;
    field.fill(0);
    field[..name.len()].copy_from_slice(name.as_bytes());
}

fn append_file(builder: &mut tar::Builder<Vec<u8>>, name: &str, data: &[u8]) {
    let mut header = header(tar::EntryType::Regular, data.len() as u64);
    set_raw_name(&mut header, name);
    header.set_cksum();
    builder.append(&header, data).unwrap();
}

fn append_dir(builder: &mut tar::Builder<Vec<u8>>, name: &str) {
    let mut header = header(tar::EntryType::Directory, 0);
    set_raw_name(&mut header, name);
    header.set_cksum();
    builder.append(&header, std::io::empty()).unwrap();
}

fn append_link(
    builder: &mut tar::Builder<Vec<u8>>,
    kind: tar::EntryType,
    name: &str,
    target: &str,
) {
    let mut header = header(kind, 0);
    set_raw_name(&mut header, name);
    header.set_link_name(target).unwrap();
    header.set_cksum();
    builder.append(&header, std::io::empty()).unwrap();
}

fn compress(data: &[u8], compression: TarCompression) -> Vec<u8> {
    match compression {
        TarCompression::None => data.to_vec(),
        TarCompression::Gzip => {
            let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
            enc.write_all(data).unwrap();
            enc.finish().unwrap()
        }
        TarCompression::Bzip2 => {
            let mut enc = bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::fast());
            enc.write_all(data).unwrap();
            enc.finish().unwrap()
        }
        TarCompression::Xz => {
            let mut enc =
                lzma_rust2::XzWriter::new(Vec::new(), lzma_rust2::XzOptions::with_preset(1))
                    .unwrap();
            enc.write_all(data).unwrap();
            enc.finish().unwrap()
        }
        TarCompression::Zstd => {
            ruzstd::encoding::compress_to_vec(data, ruzstd::encoding::CompressionLevel::Fastest)
        }
    }
}

/// Every supported format, built in `dir`, with the tree of [`write_zip`].
fn all_formats(dir: &Path) -> Vec<PathBuf> {
    let zip = dir.join("tree.zip");
    write_zip(&zip);
    let mut archives = vec![zip];
    for (name, compression) in [
        ("tree.tar", TarCompression::None),
        ("tree.tar.gz", TarCompression::Gzip),
        ("tree.tar.bz2", TarCompression::Bzip2),
        ("tree.tar.xz", TarCompression::Xz),
        ("tree.tar.zst", TarCompression::Zstd),
    ] {
        let path = dir.join(name);
        fs::write(&path, compress(&tar_bytes(), compression)).unwrap();
        archives.push(path);
    }
    archives
}

#[test]
fn every_format_lists_and_reads_the_same_tree() {
    let dir = tempfile::tempdir().unwrap();
    for archive in all_formats(dir.path()) {
        let what = archive.display().to_string();
        let provider = opened(&archive);
        assert_eq!(
            provider.connection_state(),
            ConnectionState::Connected,
            "{what}"
        );

        let root = provider.list_dir(&at(&archive, "/")).recv().unwrap();
        assert_eq!(names(&root), ["docs", "src"], "{what}");
        assert!(root.iter().all(VfsEntry::is_dir), "{what}");
        assert_eq!(root[0].path, at(&archive, "/docs"), "{what}");

        let docs = provider.list_dir(&at(&archive, "/docs")).recv().unwrap();
        let link = docs.iter().find(|e| e.name == "link.md").unwrap();
        assert!(link.is_symlink(), "listing reports the link itself: {what}");

        let readme = provider
            .read_file(&at(&archive, "/docs/readme.md"))
            .recv()
            .unwrap();
        assert_eq!(readme, README, "{what}");
        let via_link = provider
            .read_file(&at(&archive, "/docs/link.md"))
            .recv()
            .unwrap();
        assert_eq!(via_link, README, "{what}");

        let big_meta = provider
            .metadata(&at(&archive, "/src/deep/big.bin"))
            .recv()
            .unwrap();
        assert_eq!(big_meta.size, big().len() as u64, "{what}");
        assert!(big_meta.readonly, "{what}");
        let big_data = provider
            .read_file(&at(&archive, "src/deep/big.bin"))
            .recv()
            .unwrap();
        assert_eq!(big_data, big(), "{what}");

        assert!(provider.exists(&at(&archive, "/src/deep")).recv().unwrap());
        assert!(!provider.exists(&at(&archive, "/nope")).recv().unwrap());
        assert!(provider.read_file(&at(&archive, "/src")).recv().is_err());
    }
}

#[test]
fn tar_hard_links_read_their_target() {
    let dir = tempfile::tempdir().unwrap();
    let archive = dir.path().join("t.tar");
    fs::write(&archive, tar_bytes()).unwrap();
    let provider = opened(&archive);
    let data = provider
        .read_file(&at(&archive, "/docs/hard.md"))
        .recv()
        .unwrap();
    assert_eq!(data, README);
}

#[test]
fn extracting_a_directory_rebuilds_the_tree_with_progress() {
    let dir = tempfile::tempdir().unwrap();
    for archive in all_formats(dir.path()) {
        let what = archive.display().to_string();
        let provider = opened(&archive);
        let dest = dir.path().join(format!(
            "out-{}",
            archive.file_name().unwrap().to_string_lossy()
        ));

        let op = provider.download_with_progress(&at(&archive, "/"), &dest);
        let result = loop {
            if let Some(result) = op.try_recv() {
                break result;
            }
            std::thread::yield_now();
        };
        assert_eq!(result.unwrap(), dest, "{what}");
        let last = op.drain_progress().unwrap();
        assert_eq!(last.bytes_downloaded, last.total_bytes, "{what}");
        assert_eq!(last.files_downloaded, last.total_files, "{what}");

        assert_eq!(
            fs::read(dest.join("docs/readme.md")).unwrap(),
            README,
            "{what}"
        );
        assert_eq!(
            fs::read(dest.join("src/deep/big.bin")).unwrap(),
            big(),
            "{what}"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let link = fs::read_link(dest.join("docs/link.md")).unwrap();
            assert_eq!(link, PathBuf::from("readme.md"), "{what}");
            let mode = fs::metadata(dest.join("docs/readme.md"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o640, "{what}");
        }
    }
}

#[test]
fn extracting_a_file_writes_it_at_the_destination() {
    let dir = tempfile::tempdir().unwrap();
    let archive = dir.path().join("t.zip");
    write_zip(&archive);
    let provider = opened(&archive);
    let dest = dir.path().join("fresh/dir/readme.md");
    provider
        .download(&at(&archive, "/docs/readme.md"), &dest)
        .recv()
        .unwrap();
    assert_eq!(fs::read(&dest).unwrap(), README);
}

#[test]
fn hostile_names_and_links_stay_inside_the_destination() {
    let dir = tempfile::tempdir().unwrap();
    let mut builder = tar::Builder::new(Vec::new());
    append_file(&mut builder, "../escaped.txt", b"x");
    append_file(&mut builder, "ok/../../escaped2.txt", b"x");
    append_file(&mut builder, "/abs/file.txt", b"inside");
    append_link(
        &mut builder,
        tar::EntryType::Symlink,
        "abs/out",
        "../../../etc",
    );
    append_link(&mut builder, tar::EntryType::Symlink, "abs/root", "/etc");
    append_link(&mut builder, tar::EntryType::Symlink, "abs/in", "file.txt");
    let archive = dir.path().join("evil.tar");
    fs::write(&archive, builder.into_inner().unwrap()).unwrap();

    let provider = opened(&archive);
    let root = provider.list_dir(&at(&archive, "/")).recv().unwrap();
    assert_eq!(names(&root), ["abs"]);

    let dest = dir.path().join("sandbox/out");
    provider.download(&at(&archive, "/"), &dest).recv().unwrap();
    assert_eq!(fs::read(dest.join("abs/file.txt")).unwrap(), b"inside");
    assert!(!dir.path().join("sandbox/escaped.txt").exists());
    assert!(!dir.path().join("escaped2.txt").exists());
    assert!(fs::symlink_metadata(dest.join("abs/out")).is_err());
    assert!(fs::symlink_metadata(dest.join("abs/root")).is_err());
    #[cfg(unix)]
    assert!(fs::symlink_metadata(dest.join("abs/in"))
        .unwrap()
        .file_type()
        .is_symlink());
}

#[test]
fn a_link_out_of_an_extracted_subtree_is_not_created() {
    let dir = tempfile::tempdir().unwrap();
    let archive = dir.path().join("t.tar");
    let mut builder = tar::Builder::new(Vec::new());
    append_file(&mut builder, "shared.txt", b"s");
    append_link(
        &mut builder,
        tar::EntryType::Symlink,
        "sub/up",
        "../shared.txt",
    );
    fs::write(&archive, builder.into_inner().unwrap()).unwrap();

    let provider = opened(&archive);
    let dest = dir.path().join("sub-only");
    provider
        .download(&at(&archive, "/sub"), &dest)
        .recv()
        .unwrap();
    assert!(dest.is_dir());
    assert!(fs::symlink_metadata(dest.join("up")).is_err());
}

#[test]
fn a_later_tar_entry_with_the_same_name_wins() {
    let dir = tempfile::tempdir().unwrap();
    let archive = dir.path().join("t.tar");
    let mut builder = tar::Builder::new(Vec::new());
    append_file(&mut builder, "f.txt", b"old");
    append_file(&mut builder, "f.txt", b"newer");
    fs::write(&archive, builder.into_inner().unwrap()).unwrap();

    let provider = opened(&archive);
    let data = provider.read_file(&at(&archive, "/f.txt")).recv().unwrap();
    assert_eq!(data, b"newer");
}

#[test]
fn cancelling_removes_the_partial_file() {
    let dir = tempfile::tempdir().unwrap();
    let archive = dir.path().join("t.zip");
    write_zip(&archive);
    let provider = opened(&archive);
    let open = provider.opened().unwrap();

    let control = Control::none();
    control.cancel.store(true, Ordering::Relaxed);
    let dest = dir.path().join("big.bin");
    let result = extract::extract(&open, Path::new("/src/deep/big.bin"), &dest, &control, None);
    assert!(matches!(result, Err(VfsError::Cancelled)));
    assert!(!dest.exists());
}

#[test]
fn every_write_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let archive = dir.path().join("t.zip");
    write_zip(&archive);
    let provider = opened(&archive);
    let file = at(&archive, "/docs/readme.md");

    let refused = |r: VfsResult<()>| matches!(r, Err(VfsError::NotSupported(_)));
    assert!(refused(provider.write_file(&file, b"x").recv()));
    assert!(refused(provider.delete(&file).recv()));
    assert!(refused(provider.rename(&file, &at(&archive, "/x")).recv()));
    assert!(refused(provider.create_dir(&at(&archive, "/new")).recv()));
    assert!(refused(provider.upload(&archive, &file).recv()));
}

#[test]
fn rewriting_the_archive_fails_the_connection() {
    let dir = tempfile::tempdir().unwrap();
    let archive = dir.path().join("t.tar");
    fs::write(&archive, tar_bytes()).unwrap();
    let provider = opened(&archive);
    assert_eq!(provider.connection_state(), ConnectionState::Connected);

    fs::write(&archive, compress(&tar_bytes(), TarCompression::Gzip)).unwrap();
    assert_eq!(provider.connection_state(), ConnectionState::Failed);
}

#[test]
fn opening_something_that_is_not_an_archive_fails() {
    let dir = tempfile::tempdir().unwrap();
    let text = dir.path().join("notes.zip");
    fs::write(&text, "plain text").unwrap();
    let mut provider = ArchiveProvider::new(VfsPath::local(&text));
    let result = provider.connect(ConnectOptions::default()).recv();
    assert!(matches!(result, Err(VfsError::Archive(_))));

    let mut missing = ArchiveProvider::new(VfsPath::local(dir.path().join("gone.zip")));
    let result = missing.connect(ConnectOptions::default()).recv();
    assert!(matches!(result, Err(VfsError::NotFound { .. })));
}

#[test]
fn an_archive_on_a_remote_host_is_refused_for_now() {
    let container = VfsPath::remote(crate::VfsProtocol::Sftp, "host", "/a.zip");
    let mut provider = ArchiveProvider::new(container);
    let result = provider.connect(ConnectOptions::default()).recv();
    assert!(matches!(result, Err(VfsError::NotSupported(_))));
}

#[test]
fn the_manager_opens_and_serves_an_archive() {
    let dir = tempfile::tempdir().unwrap();
    let archive = dir.path().join("t.zip");
    write_zip(&archive);
    let manager = VfsManager::new();
    let root = at(&archive, "/");

    assert!(!manager.is_connected(&root));
    manager
        .connect_archive(&root, ConnectOptions::default())
        .recv()
        .unwrap();
    assert!(manager.is_connected(&at(&archive, "/docs")));
    assert_eq!(manager.get_home_dir(&root), Some(root.clone()));

    let entries = manager.list_dir(&root).recv().unwrap();
    assert_eq!(names(&entries), ["docs", "src"]);
    let data = manager
        .read_file(&at(&archive, "/docs/readme.md"))
        .recv()
        .unwrap();
    assert_eq!(data, README);
}
