//! Moving a file by moving its directory entry.
//!
//! A move on FAT is two writes: give the chain a second name, then take the
//! first name away. There is no way to make that one atomic write, so the
//! window between them is part of the primitive's contract rather than an
//! implementation detail, and these tests pin down what is true inside it.

use embedded_sdmmc::{Mode, ShortFileName, VolumeIdx, VolumeManager};

mod utils;

type Manager = VolumeManager<utils::RamDisk<Vec<u8>>, utils::TestTimeSource, 4, 4, 1>;

fn manager() -> Manager {
    let device = utils::make_block_device(utils::DISK_SOURCE).expect("disk image");
    VolumeManager::new_with_limits(device, utils::make_time_source(), 0xAA)
}

fn read_all(
    directory: &embedded_sdmmc::Directory<
        '_,
        utils::RamDisk<Vec<u8>>,
        utils::TestTimeSource,
        4,
        4,
        1,
    >,
    name: &str,
) -> Vec<u8> {
    let file = directory
        .open_file_in_dir(name, Mode::ReadOnly)
        .expect("open");
    let mut body = vec![0u8; file.length() as usize];
    file.read(&mut body).expect("read");
    body
}

/// Names in a directory listing, long names where they exist.
fn listing(
    directory: &embedded_sdmmc::Directory<
        '_,
        utils::RamDisk<Vec<u8>>,
        utils::TestTimeSource,
        4,
        4,
        1,
    >,
) -> Vec<String> {
    let mut names = Vec::new();
    directory
        .iterate_dir(|entry| names.push(entry.name.to_string()))
        .expect("iterate");
    names
}

#[test]
fn a_moved_file_keeps_its_body_and_loses_its_old_name() {
    let manager = manager();
    let volume = manager.open_volume(VolumeIdx(0)).expect("volume");
    let root = volume.open_root_dir().expect("root");
    let source = root.open_dir("TEST").expect("TEST");

    let staged = source
        .create_file_in_dir_lfn("Staged Upload.tmp", "STAGED.TMP")
        .expect("create staging file");
    staged.write(b"the whole book").expect("write");
    staged.close().expect("close");

    let entry = source
        .find_directory_entry("STAGED.TMP")
        .expect("look up the staged file");

    // Install it under its real name, then retire the staging name.
    root.link_file_in_dir_lfn("A Real Book.epub", "AREALB~1.EPU", &entry)
        .expect("link");
    source.delete_file_in_dir("STAGED.TMP").expect("unlink");

    assert_eq!(read_all(&root, "AREALB~1.EPU"), b"the whole book");
    assert!(
        !listing(&source).iter().any(|n| n == "STAGED.TMP"),
        "the staging name must be gone"
    );
    assert!(
        root.find_directory_entry("AREALB~1.EPU").is_ok(),
        "the installed name must be there"
    );
}

/// The window is real and both names work in it. This is what makes
/// `delete_file_in_dir` the only safe cleanup: anything that reclaims
/// clusters frees a chain the other name is still using.
#[test]
fn both_names_reach_the_same_body_before_the_move_finishes() {
    let manager = manager();
    let volume = manager.open_volume(VolumeIdx(0)).expect("volume");
    let root = volume.open_root_dir().expect("root");
    let source = root.open_dir("TEST").expect("TEST");

    let staged = source
        .create_file_in_dir_lfn("Staged Upload.tmp", "STAGED.TMP")
        .expect("create");
    staged.write(b"shared body").expect("write");
    staged.close().expect("close");

    let entry = source.find_directory_entry("STAGED.TMP").expect("look up");
    root.link_file_in_dir_lfn("A Real Book.epub", "AREALB~1.EPU", &entry)
        .expect("link");

    // Stop here, as a power cut would.
    assert_eq!(read_all(&source, "STAGED.TMP"), b"shared body");
    assert_eq!(read_all(&root, "AREALB~1.EPU"), b"shared body");

    // Completing the move from this state is the same unlink as always, and
    // the survivor is unharmed.
    source.delete_file_in_dir("STAGED.TMP").expect("unlink");
    assert_eq!(read_all(&root, "AREALB~1.EPU"), b"shared body");
}

/// Recovery's other option: abandon the move instead of finishing it. Same
/// unlink, other name, and the original is untouched.
#[test]
fn a_half_finished_move_can_be_rolled_back_instead() {
    let manager = manager();
    let volume = manager.open_volume(VolumeIdx(0)).expect("volume");
    let root = volume.open_root_dir().expect("root");
    let source = root.open_dir("TEST").expect("TEST");

    let staged = source
        .create_file_in_dir_lfn("Staged Upload.tmp", "STAGED.TMP")
        .expect("create");
    staged.write(b"rolled back").expect("write");
    staged.close().expect("close");

    let entry = source.find_directory_entry("STAGED.TMP").expect("look up");
    root.link_file_in_dir_lfn("A Real Book.epub", "AREALB~1.EPU", &entry)
        .expect("link");

    root.delete_file_in_dir("AREALB~1.EPU").expect("unlink");

    assert_eq!(read_all(&source, "STAGED.TMP"), b"rolled back");
    assert!(
        matches!(
            root.find_directory_entry("AREALB~1.EPU"),
            Err(embedded_sdmmc::Error::NotFound)
        ),
        "the abandoned name must be gone"
    );
}

#[test]
fn linking_onto_a_taken_alias_is_refused() {
    let manager = manager();
    let volume = manager.open_volume(VolumeIdx(0)).expect("volume");
    let root = volume.open_root_dir().expect("root");
    let source = root.open_dir("TEST").expect("TEST");

    let staged = source
        .create_file_in_dir_lfn("Staged Upload.tmp", "STAGED.TMP")
        .expect("create");
    staged.write(b"body").expect("write");
    staged.close().expect("close");
    let entry = source.find_directory_entry("STAGED.TMP").expect("look up");

    let occupant = root
        .create_file_in_dir_lfn("Already Here.epub", "AREALB~1.EPU")
        .expect("create occupant");
    occupant.write(b"do not clobber me").expect("write");
    occupant.close().expect("close");

    assert!(
        matches!(
            root.link_file_in_dir_lfn("A Real Book.epub", "AREALB~1.EPU", &entry),
            Err(embedded_sdmmc::Error::FileAlreadyExists)
        ),
        "an occupied alias must be refused, not overwritten"
    );
    assert_eq!(read_all(&root, "AREALB~1.EPU"), b"do not clobber me");
}

/// The installed entry describes the same file, not a fresh empty one.
#[test]
fn the_linked_entry_carries_the_sources_chain_and_size() {
    let manager = manager();
    let volume = manager.open_volume(VolumeIdx(0)).expect("volume");
    let root = volume.open_root_dir().expect("root");
    let source = root.open_dir("TEST").expect("TEST");

    let staged = source
        .create_file_in_dir_lfn("Staged Upload.tmp", "STAGED.TMP")
        .expect("create");
    staged.write(&[7u8; 1500]).expect("write");
    staged.close().expect("close");

    let before = source.find_directory_entry("STAGED.TMP").expect("look up");
    root.link_file_in_dir_lfn("A Real Book.epub", "AREALB~1.EPU", &before)
        .expect("link");
    let after = root.find_directory_entry("AREALB~1.EPU").expect("look up");

    assert_eq!(after.cluster, before.cluster, "same first cluster");
    assert_eq!(after.size, before.size, "same size");
    assert_eq!(after.ctime, before.ctime, "created when the data was");
    assert_eq!(
        after.name,
        ShortFileName::create_from_str("AREALB~1.EPU").unwrap(),
        "under the new alias"
    );
}
