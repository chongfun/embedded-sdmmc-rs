//! Moving a file by moving its directory entry.
//!
//! A move on FAT is two writes: give the chain a second name, then take the
//! first name away. There is no way to make that one atomic write, so the
//! window between them is part of the primitive's contract rather than an
//! implementation detail, and these tests pin down what is true inside it.

use core::ops::ControlFlow;

use embedded_sdmmc::{
    Block, BlockDevice, BlockIdx, ClusterId, Mode, MoveFate, ShortFileName, VolumeIdx,
    VolumeManager,
};

mod utils;

type Manager = VolumeManager<utils::RamDisk<Vec<u8>>, utils::TestTimeSource, 4, 4, 1>;

fn manager() -> Manager {
    let device = utils::make_block_device(utils::DISK_SOURCE).expect("disk image");
    VolumeManager::new_with_limits(device, utils::make_time_source(), 0xAA)
}

fn read_all<D: embedded_sdmmc::BlockDevice>(
    directory: &embedded_sdmmc::Directory<'_, D, utils::TestTimeSource, 4, 4, 1>,
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
        .iterate_dir(|entry| {
            names.push(entry.name.to_string());
            ControlFlow::Continue(())
        })
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
        .open_file_in_dir("STAGED.TMP", Mode::ReadWriteCreate)
        .expect("create staging file");
    staged.write(b"the whole book").expect("write");
    staged.close().expect("close");

    // One call installs it under its real name and retires the staging one.
    source
        .move_file_in_dir_lfn("STAGED.TMP", &root, "A Real Book.epub")
        .expect("move");

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

/// A move is two directory writes and cannot be made one, so the second can
/// fail on its own. When it does, the first is undone: a move that could not
/// finish is a move that did not happen, rather than one the caller is left
/// holding half of.
#[test]
fn a_move_that_cannot_retire_the_old_name_leaves_nothing_behind() {
    let device = utils::FailRegion::new(
        utils::make_block_device(utils::DISK_SOURCE).expect("disk image"),
    );
    let manager: VolumeManager<_, _, 4, 4, 1> =
        VolumeManager::new_with_limits(device, utils::make_time_source(), 0xAA);
    let volume = manager.open_volume(VolumeIdx(0)).expect("volume");
    let root = volume.open_root_dir().expect("root");
    let source = root.open_dir("TEST").expect("TEST");

    let staged = source
        .open_file_in_dir("STAGED.TMP", Mode::ReadWriteCreate)
        .expect("create");
    staged.write(b"unfinished").expect("write");
    staged.close().expect("close");

    // Learn which sector holds the source directory, and make writes to it
    // fail. The destination is the root directory, which is elsewhere, so the
    // first half of the move still succeeds and only the unlink cannot.
    let staged_entry = source.find_directory_entry("STAGED.TMP").expect("entry");
    let source_dir_block = staged_entry.entry_block.0;
    manager.device(|d| {
        d.write_region
            .set(Some((source_dir_block, source_dir_block + 1)))
    });
    let attempt =
        source.move_file_in_dir_lfn("STAGED.TMP", &root, "A Real Book.epub");
    manager.device(|d| d.write_region.set(None));

    assert!(
        manager.device(|d| d.injected.get()) > 0,
        "no write was intercepted, so this proves nothing about the failure path"
    );
    assert!(attempt.is_err(), "the move reported success despite failing");

    // The destination must have been taken back off the disk.
    assert!(
        matches!(
            root.find_directory_entry("AREALB~1.EPU"),
            Err(embedded_sdmmc::Error::NotFound)
        ),
        "a move that failed left its destination name behind"
    );
    // And the source is untouched and still readable.
    assert_eq!(read_all(&source, "STAGED.TMP"), b"unfinished");
}

/// If the undo cannot be written either -- the same as losing power between
/// the two writes -- both names are left on the disk. That state is only ever
/// found, never handed out: the chain has two entries with their own copies of
/// its length and start cluster, so nothing may be written through either name
/// until one of them is gone.
///
/// Recovery is to unlink the unwanted name, which leaves the chain alone.
#[test]
fn a_move_interrupted_between_its_two_writes_is_recoverable() {
    let device = utils::FailRegion::new(
        utils::make_block_device(utils::DISK_SOURCE).expect("disk image"),
    );
    let manager: VolumeManager<_, _, 4, 4, 1> =
        VolumeManager::new_with_limits(device, utils::make_time_source(), 0xAA);
    let volume = manager.open_volume(VolumeIdx(1)).expect("volume");
    let root = volume.open_root_dir().expect("root");

    let staged = root
        .open_file_in_dir("STAGED.TMP", Mode::ReadWriteCreate)
        .expect("create");
    staged.write(b"interrupted").expect("write");
    staged.close().expect("close");

    // Source and destination share a directory, so every write of the move
    // lands in one region and they can be counted. The long name needs two
    // entries plus its alias; the short source name needs one. Cutting in
    // after the third stops the move exactly between its halves, and stops
    // the undo as well.
    let entry = root.find_directory_entry("STAGED.TMP").expect("entry");
    let dir_block = entry.entry_block.0;
    // A FAT32 cluster here is eight blocks; the move's entries can land in
    // any of them, so cover the whole cluster.
    let cluster_start = dir_block - (dir_block % 8);
    manager.device(|d| d.write_region.set(Some((cluster_start, cluster_start + 8))));
    // A clean move here is four writes: three to install the long name and
    // its alias, one to retire the old name. Cutting in at the fourth stops
    // it exactly between its halves, and stops the undo with it.
    manager.device(|d| d.fail_writes_from.set(Some(4)));
    let attempt = root.move_file_in_dir_lfn("STAGED.TMP", &root, "A Real Book.epub");
    manager.device(|d| d.fail_writes_from.set(None));
    manager.device(|d| d.write_region.set(None));

    assert!(attempt.is_err(), "the move reported success despite failing");
    assert_eq!(
        manager.device(|d| d.injected.get()),
        2,
        "expected both the unlink and the undo to have been stopped"
    );

    // Both names are on the disk, and both reach the same body.
    assert_eq!(read_all(&root, "STAGED.TMP"), b"interrupted");
    assert_eq!(read_all(&root, "AREALB~1.EPU"), b"interrupted");

    // They are two names for one file, so only one may be open at a time --
    // truncating through either would free the chain the other still uses.
    let held = root
        .open_file_in_dir("STAGED.TMP", Mode::ReadOnly)
        .expect("open one name");
    assert!(
        matches!(
            root.open_file_in_dir("AREALB~1.EPU", Mode::ReadWriteTruncate),
            Err(embedded_sdmmc::Error::FileAlreadyOpen)
        ),
        "the second name for an open chain must be refused"
    );
    held.close().expect("close");

    // Finish the move by retiring the old name. The survivor is unharmed,
    // because unlinking a name does not touch the chain behind it.
    root.delete_entry_in_dir("STAGED.TMP").expect("recover");
    assert_eq!(read_all(&root, "AREALB~1.EPU"), b"interrupted");
    assert!(matches!(
        root.find_directory_entry("STAGED.TMP"),
        Err(embedded_sdmmc::Error::NotFound)
    ));
}

#[test]
fn moving_onto_a_taken_name_is_refused() {
    let manager = manager();
    let volume = manager.open_volume(VolumeIdx(0)).expect("volume");
    let root = volume.open_root_dir().expect("root");
    let source = root.open_dir("TEST").expect("TEST");

    let staged = source
        .open_file_in_dir("STAGED.TMP", Mode::ReadWriteCreate)
        .expect("create");
    staged.write(b"body").expect("write");
    staged.close().expect("close");
    let occupant = root
        .create_file_in_dir_lfn("A Real Book.epub")
        .expect("create occupant");
    occupant.write(b"do not clobber me").expect("write");
    occupant.close().expect("close");

    assert!(
        matches!(
            source.move_file_in_dir_lfn("STAGED.TMP", &root, "A Real Book.epub"),
            Err(embedded_sdmmc::Error::FileAlreadyExists)
        ),
        "a name already in the destination must be refused, not overwritten"
    );
    assert_eq!(read_all(&root, "AREALB~1.EPU"), b"do not clobber me");
    assert_eq!(read_all(&source, "STAGED.TMP"), b"body");
}

#[test]
fn the_moved_entry_carries_the_sources_chain_and_size() {
    let manager = manager();
    let volume = manager.open_volume(VolumeIdx(0)).expect("volume");
    let root = volume.open_root_dir().expect("root");
    let source = root.open_dir("TEST").expect("TEST");

    let staged = source
        .open_file_in_dir("STAGED.TMP", Mode::ReadWriteCreate)
        .expect("create");
    staged.write(&[7u8; 1500]).expect("write");
    staged.close().expect("close");

    let before = source.find_directory_entry("STAGED.TMP").expect("look up");
    source.move_file_in_dir_lfn("STAGED.TMP", &root, "A Real Book.epub")
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

/// What [`ClusterId::value`] is for: a caller writes the number down and
/// recognises the same file later, under a name it could not have predicted.
/// Neither name can do that job — the long name is whatever the move was told
/// to use, and the 8.3 alias is unique only within one directory and is handed
/// to the next file that needs one as soon as an entry is deleted.
#[test]
fn a_recorded_cluster_number_identifies_a_file_across_a_move() {
    let manager = manager();
    let volume = manager.open_volume(VolumeIdx(0)).expect("volume");
    let root = volume.open_root_dir().expect("root");
    let source = root.open_dir("TEST").expect("TEST");

    for (name, fill) in [("FIRST.TMP", 1u8), ("SECOND.TMP", 2u8)] {
        let file = source
            .open_file_in_dir(name, Mode::ReadWriteCreate)
            .expect("create");
        file.write(&[fill; 1500]).expect("write");
        file.close().expect("close");
    }

    let recorded = source
        .find_directory_entry("FIRST.TMP")
        .expect("look up")
        .cluster
        .value();
    let other = source
        .find_directory_entry("SECOND.TMP")
        .expect("look up")
        .cluster
        .value();
    assert_ne!(recorded, other, "two files must not answer to one number");
    assert_ne!(
        recorded,
        ClusterId::EMPTY.value(),
        "a file with a body starts somewhere"
    );

    source
        .move_file_in_dir_lfn("FIRST.TMP", &root, "A Real Book.epub")
        .expect("link");

    assert_eq!(
        root.find_directory_entry("AREALB~1.EPU")
            .expect("look up")
            .cluster
            .value(),
        recorded,
        "the number has to outlive the name it was found under"
    );
}

/// The two halves taken separately, which is what a caller reaches for when it
/// wants the second name to *stay* for a while.
///
/// What is actually pinned here is narrow, so this checks only that: the link
/// names the same chain as the source, neither entry knows about the other,
/// and retiring one with an unlink leaves the survivor whole. That the chain
/// stays allocated for as long as a name holds it is true of this driver and
/// of nothing else — FAT counts no references, so a delete through either name
/// from anywhere frees it under both. `link_file_in_dir_lfn` spells out what
/// that means for a caller using a link as recovery evidence.
#[test]
fn a_link_holds_the_chain_until_its_name_is_taken_away() {
    let manager = manager();
    let volume = manager.open_volume(VolumeIdx(0)).expect("volume");
    let root = volume.open_root_dir().expect("root");
    let source = root.open_dir("TEST").expect("TEST");

    let staged = source
        .open_file_in_dir("STAGED.TMP", Mode::ReadWriteCreate)
        .expect("create");
    staged.write(&[9u8; 10000]).expect("write");
    staged.close().expect("close");
    let chain = source
        .find_directory_entry("STAGED.TMP")
        .expect("look up")
        .cluster;

    let alias = source
        .link_file_in_dir_lfn("STAGED.TMP", &root, "A Real Book.epub")
        .expect("link");
    assert_eq!(
        alias,
        ShortFileName::create_from_str("AREALB~1.EPU").unwrap()
    );

    // Both names, one chain, and neither entry knows about the other.
    let by_scratch = source.find_directory_entry("STAGED.TMP").expect("source");
    let by_book = root.find_directory_entry("AREALB~1.EPU").expect("dest");
    assert_eq!(by_scratch.cluster, chain);
    assert_eq!(by_book.cluster, chain);
    assert_eq!(by_book.size, by_scratch.size);

    // Retiring one with an unlink leaves the other whole -- this is the half
    // the caller owns, and the only correct way to take it.
    source.delete_entry_in_dir("STAGED.TMP").expect("unlink");
    assert!(matches!(
        source.find_directory_entry("STAGED.TMP"),
        Err(embedded_sdmmc::Error::NotFound)
    ));
    let survivor = root.find_directory_entry("AREALB~1.EPU").expect("survivor");
    assert_eq!(survivor.cluster, chain, "the chain did not move");
    assert_eq!(read_all(&root, "AREALB~1.EPU"), vec![9u8; 10000]);
}

/// The wrapper carries its own cross-manager check, so it needs its own test:
/// the one below goes through `move_file_in_dir_lfn`, and a link that reached
/// the raw handles without this guard would write into a different filesystem
/// with nothing to report.
#[test]
fn linking_into_another_managers_directory_is_refused() {
    let manager_a = VolumeManager::new(
        utils::make_block_device(utils::DISK_SOURCE).expect("disk image"),
        utils::make_time_source(),
    );
    let manager_b = VolumeManager::new(
        utils::make_block_device(utils::DISK_SOURCE).expect("disk image"),
        utils::make_time_source(),
    );

    let vol_a = manager_a.open_volume(VolumeIdx(1)).expect("open A");
    let vol_b = manager_b.open_volume(VolumeIdx(1)).expect("open B");
    let root_a = vol_a.open_root_dir().expect("root A");
    let root_b = vol_b.open_root_dir().expect("root B");

    let staged = root_a
        .open_file_in_dir("STAGED.TMP", Mode::ReadWriteCreate)
        .expect("create on A");
    staged.write(b"belongs to A").expect("write");
    staged.close().expect("close");

    let attempt = root_a.link_file_in_dir_lfn("STAGED.TMP", &root_b, "A Real Book.epub");
    assert!(
        matches!(attempt, Err(embedded_sdmmc::Error::BadHandle)),
        "a destination from another manager must be refused, got {:?}",
        attempt.err()
    );
    assert!(matches!(
        root_b.find_directory_entry("AREALB~1.EPU"),
        Err(embedded_sdmmc::Error::NotFound)
    ));

    // Checked last, because reading a handle consumes it: the two really are
    // indistinguishable by value, so nothing below the wrapper could catch it.
    assert_eq!(
        root_a.to_raw_directory(),
        root_b.to_raw_directory(),
        "expected colliding handles; without them this proves nothing"
    );
}

/// A link is refused wherever a whole move would be, since it is the half that
/// does the checking. The move's own tests cover each reason; this one pins
/// that the exposed half applies them too rather than being a back door.
#[test]
fn a_link_refuses_a_name_already_taken() {
    let manager = manager();
    let volume = manager.open_volume(VolumeIdx(0)).expect("volume");
    let root = volume.open_root_dir().expect("root");
    let source = root.open_dir("TEST").expect("TEST");

    let staged = source
        .open_file_in_dir("STAGED.TMP", Mode::ReadWriteCreate)
        .expect("create");
    staged.write(b"body").expect("write");
    staged.close().expect("close");

    let taken = root
        .create_file_in_dir_lfn("A Real Book.epub")
        .expect("first");
    taken.close().expect("close");

    assert!(matches!(
        source.link_file_in_dir_lfn("STAGED.TMP", &root, "A Real Book.epub"),
        Err(embedded_sdmmc::Error::FileAlreadyExists)
    ));

    // And the source is untouched, so the caller can still finish or abandon.
    assert_eq!(
        source
            .find_directory_entry("STAGED.TMP")
            .expect("source")
            .size,
        4
    );
}

// ---------------------------------------------------------------------------
// What the primitive refuses.
//
// Each of these is a case where the link would otherwise be written against
// facts that are not true: a source whose entry is out of date, a source whose
// cluster number belongs to a different FAT, or a source that is not a file at
// all. They are checks the old `&DirEntry` signature had no way to make.
// ---------------------------------------------------------------------------

/// A file's directory entry is only brought up to date when it is flushed or
/// closed, so an open file's entry still describes what it was before the
/// write. Linking from it would copy that stale length and start cluster, and
/// unlinking the original afterwards would leave the only surviving name
/// pointing at nothing while the real chain became unreachable.
#[test]
fn moving_an_open_source_is_refused() {
    let manager = manager();
    let volume = manager.open_volume(VolumeIdx(1)).expect("open volume");
    let root = volume.open_root_dir().expect("open root");

    let staged = root
        .open_file_in_dir("STAGED.TMP", Mode::ReadWriteCreate)
        .expect("create");
    staged.write(b"the body that must survive").expect("write");

    // Still open, so the on-disk entry has not caught up with that write.
    let attempt = root.move_file_in_dir_lfn("STAGED.TMP", &root, "A Real Book.epub");
    assert!(
        matches!(attempt, Err(embedded_sdmmc::Error::FileAlreadyOpen)),
        "linking from an open source must be refused, got {:?}",
        attempt.err()
    );

    // Once it is closed the same call is fine, and carries the real body.
    staged.close().expect("close");
    root.move_file_in_dir_lfn("STAGED.TMP", &root, "A Real Book.epub")
        .expect("link after close");
    assert_eq!(read_all(&root, "AREALB~1.EPU"), b"the body that must survive");
}

/// A directory carries a `..` entry naming its parent. Linking it under a
/// second parent would not update that back-pointer, so unlinking the original
/// would leave a directory whose `..` still resolves to where it used to be.
#[test]
fn moving_a_directory_is_refused() {
    let manager = manager();
    let volume = manager.open_volume(VolumeIdx(1)).expect("open volume");
    let root = volume.open_root_dir().expect("open root");

    root.make_dir_in_dir("SUBDIR").expect("make SUBDIR");

    let attempt = root.move_file_in_dir_lfn("SUBDIR", &root, "A Real Folder");
    assert!(
        matches!(attempt, Err(embedded_sdmmc::Error::OpenedDirAsFile)),
        "linking from a directory source must be refused, got {:?}",
        attempt.err()
    );
}

/// A cluster number only means anything against the FAT it was allocated from.
/// Linking across volumes would reinterpret it against the destination's FAT,
/// aiming the new name at whatever happens to live at that number there.
#[test]
fn moving_across_volumes_is_refused() {
    let device = utils::make_block_device(utils::DISK_SOURCE).expect("disk image");
    // Two volumes open at once, which the other tests in this file cannot do.
    let manager: VolumeManager<_, _, 4, 4, 2> =
        VolumeManager::new_with_limits(device, utils::make_time_source(), 0xAA);

    let fat16 = manager.open_volume(VolumeIdx(0)).expect("open FAT16");
    let fat32 = manager.open_volume(VolumeIdx(1)).expect("open FAT32");
    let fat16_root = fat16.open_root_dir().expect("open FAT16 root");
    let fat32_root = fat32.open_root_dir().expect("open FAT32 root");

    let staged = fat16_root
        .open_file_in_dir("STAGED.TMP", Mode::ReadWriteCreate)
        .expect("create on FAT16");
    staged.write(b"body on the other volume").expect("write");
    staged.close().expect("close");

    let attempt =
        fat16_root.move_file_in_dir_lfn("STAGED.TMP", &fat32_root, "A Real Book.epub");
    assert!(
        matches!(attempt, Err(embedded_sdmmc::Error::Unsupported)),
        "linking across volumes must be refused, got {:?}",
        attempt.err()
    );

    // And nothing was written to the destination.
    assert!(matches!(
        fat32_root.find_directory_entry("AREALB~1.EPU"),
        Err(embedded_sdmmc::Error::NotFound)
    ));
}

/// Handles are plain numbers, and every manager built with
/// `VolumeManager::new` starts numbering from the same offset, so two of them
/// hand out the same values for the same sequence of calls. A destination
/// borrowed from one manager and used with another would therefore resolve to
/// a real -- but entirely unrelated -- directory, and the link would be
/// written to the wrong filesystem with nothing to report.
#[test]
fn moving_into_another_managers_directory_is_refused() {
    let manager_a = VolumeManager::new(
        utils::make_block_device(utils::DISK_SOURCE).expect("disk image"),
        utils::make_time_source(),
    );
    let manager_b = VolumeManager::new(
        utils::make_block_device(utils::DISK_SOURCE).expect("disk image"),
        utils::make_time_source(),
    );

    let vol_a = manager_a.open_volume(VolumeIdx(1)).expect("open A");
    let vol_b = manager_b.open_volume(VolumeIdx(1)).expect("open B");
    let root_a = vol_a.open_root_dir().expect("root A");
    let root_b = vol_b.open_root_dir().expect("root B");

    let staged = root_a
        .open_file_in_dir("STAGED.TMP", Mode::ReadWriteCreate)
        .expect("create on A");
    staged.write(b"belongs to A").expect("write");
    staged.close().expect("close");

    let attempt =
        root_a.move_file_in_dir_lfn("STAGED.TMP", &root_b, "A Real Book.epub");
    assert!(
        matches!(attempt, Err(embedded_sdmmc::Error::BadHandle)),
        "a destination from another manager must be refused, got {:?}",
        attempt.err()
    );

    // Neither filesystem was touched.
    assert!(matches!(
        root_a.find_directory_entry("AREALB~1.EPU"),
        Err(embedded_sdmmc::Error::NotFound)
    ));
    assert!(matches!(
        root_b.find_directory_entry("AREALB~1.EPU"),
        Err(embedded_sdmmc::Error::NotFound)
    ));

    // The premise, checked last because reading the handle consumes it: the
    // two really are indistinguishable by value, so nothing downstream of the
    // wrapper could have caught this.
    assert_eq!(
        root_a.to_raw_directory(),
        root_b.to_raw_directory(),
        "expected colliding handles; if this stops being true the test is no \
         longer exercising the confusion it was written for"
    );
}

/// The alias is a name in the same namespace as everything else, so it has to
/// be checked against existing long names too -- not only against existing
/// short ones. An entry whose *long* name is `ALIAS.TXT` makes that alias
/// taken, even though no short name in the directory is.
#[test]
fn a_derived_alias_never_collides_with_a_name_already_there() {
    let manager = manager();
    let volume = manager.open_volume(VolumeIdx(1)).expect("open volume");
    let root = volume.open_root_dir().expect("open root");

    // Two long names that reduce to the same eight characters must not end up
    // sharing an alias -- the caller never sees them, so nothing else would
    // catch it.
    root.create_file_in_dir_lfn("Report for April.txt")
        .expect("first")
        .close()
        .expect("close");
    root.create_file_in_dir_lfn("Report for August.txt")
        .expect("second")
        .close()
        .expect("close");

    let mut aliases: Vec<String> = Vec::new();
    let mut storage = [0u8; 256];
    let mut lfn_buffer = embedded_sdmmc::LfnBuffer::new(&mut storage);
    root.iterate_dir_lfn(&mut lfn_buffer, |entry, long| {
        if long.is_some_and(|l| l.starts_with("Report for ")) {
            aliases.push(entry.name.to_string());
        }
        ControlFlow::Continue(())
    })
    .expect("iterate");
    assert_eq!(aliases.len(), 2, "both files should be there");
    assert_ne!(aliases[0], aliases[1], "the two aliases must differ");

    // And a long name that is itself one of those aliases is refused, because
    // an alias is a name in the same namespace.
    let taken = aliases[0].clone();
    assert!(
        matches!(
            root.create_file_in_dir_lfn(&taken),
            Err(embedded_sdmmc::Error::FileAlreadyExists)
        ),
        "a long name matching an existing alias must be refused, got a success for {taken}"
    );
}

/// The sequence that made a public link primitive unsafe: link, close
/// everything, then truncate through one of the two names. The entries hold
/// their own copies of the start cluster and the length, so truncating one
/// freed the chain and updated that entry alone, leaving the other describing
/// a file whose clusters had gone back to the volume -- with nothing open at
/// any point, so no open-file rule could see it coming.
///
/// A completed move leaves one name, so there is no second entry to strand.
#[test]
fn a_completed_move_leaves_no_second_name_to_strand() {
    let manager = manager();
    let volume = manager.open_volume(VolumeIdx(1)).expect("volume");
    let root = volume.open_root_dir().expect("root");

    let staged = root
        .open_file_in_dir("STAGED.TMP", Mode::ReadWriteCreate)
        .expect("create");
    // Several clusters, so truncation has something to free.
    staged.write(&[b'x'; 10000]).expect("write");
    staged.close().expect("close");

    root.move_file_in_dir_lfn("STAGED.TMP", &root, "A Real Book.epub")
        .expect("move");

    let moved = root.find_directory_entry("AREALB~1.EPU").expect("entry");
    assert_eq!(moved.size, 10000);

    // The old name is gone, so it cannot be opened at all -- let alone
    // truncated out from under the name that survived.
    assert!(matches!(
        root.open_file_in_dir("STAGED.TMP", Mode::ReadWriteTruncate),
        Err(embedded_sdmmc::Error::NotFound)
    ));

    // Truncating the surviving name is an ordinary truncate: it owns the chain
    // outright, and nothing else refers to it.
    root.open_file_in_dir("AREALB~1.EPU", Mode::ReadWriteTruncate)
        .expect("truncate the moved file")
        .close()
        .expect("close");

    let after = root.find_directory_entry("AREALB~1.EPU").expect("entry");
    assert_eq!(after.size, 0);
    assert!(
        listing(&root).iter().filter(|n| *n == "AREALB~1.EPU").count() == 1,
        "exactly one entry should name this chain"
    );
}

// ---------------------------------------------------------------------------
// The short-named move: the same two writes, and the exact name asked for
// ---------------------------------------------------------------------------

/// The reason the short variant exists. A long-name move of `BOOK.BIN` files
/// it under a derived alias, `BOOK~1.BIN`, and a caller that opens its files
/// by short name would then miss it. The short move lands the exact name and
/// writes no long-name entries beside it.
#[test]
fn a_short_named_move_lands_under_exactly_that_name() {
    let manager = manager();
    let volume = manager.open_volume(VolumeIdx(0)).expect("volume");
    let root = volume.open_root_dir().expect("root");
    let source = root.open_dir("TEST").expect("TEST");

    let staged = source
        .open_file_in_dir("STAGED.TMP", Mode::ReadWriteCreate)
        .expect("create");
    staged.write(b"the whole index").expect("write");
    staged.close().expect("close");

    source
        .move_file_in_dir("STAGED.TMP", &root, "BOOK.BIN")
        .expect("move");

    assert_eq!(read_all(&root, "BOOK.BIN"), b"the whole index");
    assert!(
        matches!(
            root.find_directory_entry("BOOK~1.BIN"),
            Err(embedded_sdmmc::Error::NotFound)
        ),
        "no alias was derived; the name asked for is the name filed"
    );
    assert!(
        !listing(&source).iter().any(|n| n == "STAGED.TMP"),
        "the old name must be gone"
    );

    // No long-name entries either: the listing with long names shows none
    // for this file.
    let mut storage = [0u8; 64];
    let mut lfn_buffer = embedded_sdmmc::LfnBuffer::new(&mut storage);
    let mut long_name_seen = false;
    root.iterate_dir_lfn(&mut lfn_buffer, |entry, long| {
        if entry.name.to_string() == "BOOK.BIN" {
            long_name_seen = long.is_some();
        }
        ControlFlow::Continue(())
    })
    .expect("iterate");
    assert!(!long_name_seen, "a short move writes no long-name entries");
}

#[test]
fn a_short_named_move_onto_a_taken_name_is_refused() {
    let manager = manager();
    let volume = manager.open_volume(VolumeIdx(0)).expect("volume");
    let root = volume.open_root_dir().expect("root");
    let source = root.open_dir("TEST").expect("TEST");

    let staged = source
        .open_file_in_dir("STAGED.TMP", Mode::ReadWriteCreate)
        .expect("create");
    staged.write(b"body").expect("write");
    staged.close().expect("close");
    let occupant = root
        .open_file_in_dir("BOOK.BIN", Mode::ReadWriteCreate)
        .expect("create occupant");
    occupant.write(b"do not clobber me").expect("write");
    occupant.close().expect("close");

    assert!(
        matches!(
            source.move_file_in_dir("STAGED.TMP", &root, "BOOK.BIN"),
            Err(embedded_sdmmc::Error::FileAlreadyExists)
        ),
        "a short name already in the destination must be refused"
    );
    assert_eq!(read_all(&root, "BOOK.BIN"), b"do not clobber me");
    assert_eq!(read_all(&source, "STAGED.TMP"), b"body");

    // The namespace is one across long and short names: a long name that
    // spells the same thing in another case takes the short name too.
    let long_occupant = root
        .create_file_in_dir_lfn("cont.bin")
        .expect("create long occupant");
    long_occupant.write(b"long").expect("write");
    long_occupant.close().expect("close");
    assert!(
        matches!(
            source.move_file_in_dir("STAGED.TMP", &root, "CONT.BIN"),
            Err(embedded_sdmmc::Error::FileAlreadyExists)
        ),
        "a long name answering to the same spelling refuses the short one"
    );
    assert_eq!(read_all(&source, "STAGED.TMP"), b"body");
}

/// The short link is the same half of a move as the long one: the new entry
/// describes the source's chain, size and times, and unlinking the old name
/// afterwards leaves the survivor whole.
#[test]
fn a_short_named_link_carries_the_sources_chain_and_size() {
    let manager = manager();
    let volume = manager.open_volume(VolumeIdx(0)).expect("volume");
    let root = volume.open_root_dir().expect("root");
    let source = root.open_dir("TEST").expect("TEST");

    let staged = source
        .open_file_in_dir("STAGED.TMP", Mode::ReadWriteCreate)
        .expect("create");
    staged.write(&[9u8; 1500]).expect("write");
    staged.close().expect("close");

    let before = source.find_directory_entry("STAGED.TMP").expect("look up");
    source
        .link_file_in_dir("STAGED.TMP", &root, "S000.BIN")
        .expect("link");
    let after = root.find_directory_entry("S000.BIN").expect("look up");

    assert_eq!(after.cluster, before.cluster, "same first cluster");
    assert_eq!(after.size, before.size, "same size");
    assert_eq!(after.ctime, before.ctime, "created when the data was");
    assert_eq!(
        after.name,
        ShortFileName::create_from_str("S000.BIN").unwrap(),
        "under exactly the name asked for"
    );
    assert_eq!(read_all(&root, "S000.BIN"), vec![9u8; 1500]);
    assert_eq!(read_all(&source, "STAGED.TMP"), vec![9u8; 1500]);

    // Finish the move by hand, the way a recovery does.
    source.delete_entry_in_dir("STAGED.TMP").expect("unlink");
    assert_eq!(read_all(&root, "S000.BIN"), vec![9u8; 1500]);
}

/// The short move links in two writes and unlinks in one, with the same window as the
/// long one. Cutting in at the second stops the unlink and the undo, both
/// names remain, and unlinking the one not wanted recovers it.
#[test]
fn a_short_named_move_interrupted_between_its_two_writes_is_recoverable() {
    let device = utils::FailRegion::new(
        utils::make_block_device(utils::DISK_SOURCE).expect("disk image"),
    );
    let manager: VolumeManager<_, _, 4, 4, 1> =
        VolumeManager::new_with_limits(device, utils::make_time_source(), 0xAA);
    let volume = manager.open_volume(VolumeIdx(1)).expect("volume");
    let root = volume.open_root_dir().expect("root");

    let staged = root
        .open_file_in_dir("STAGED.TMP", Mode::ReadWriteCreate)
        .expect("create");
    staged.write(b"interrupted").expect("write");
    staged.close().expect("close");

    // Source and destination share a directory, so every write lands in one
    // cluster and can be counted. A short move is three writes: two install
    // the new entry, one retires the old. Cutting in at the third stops the
    // move between its halves, and stops the undo as well.
    let entry = root.find_directory_entry("STAGED.TMP").expect("entry");
    let dir_block = entry.entry_block.0;
    let cluster_start = dir_block - (dir_block % 8);
    manager.device(|d| d.write_region.set(Some((cluster_start, cluster_start + 8))));
    manager.device(|d| d.fail_writes_from.set(Some(3)));
    let attempt = root.move_file_in_dir("STAGED.TMP", &root, "BOOK.BIN");
    manager.device(|d| d.fail_writes_from.set(None));
    manager.device(|d| d.write_region.set(None));

    assert!(attempt.is_err(), "the move reported success despite failing");
    assert_eq!(
        manager.device(|d| d.injected.get()),
        2,
        "expected both the unlink and the undo to have been stopped"
    );

    assert_eq!(read_all(&root, "STAGED.TMP"), b"interrupted");
    assert_eq!(read_all(&root, "BOOK.BIN"), b"interrupted");
    let before = root.find_directory_entry("STAGED.TMP").expect("entry");
    let after = root.find_directory_entry("BOOK.BIN").expect("entry");
    assert_eq!(after.cluster, before.cluster, "one chain under two names");

    root.delete_entry_in_dir("STAGED.TMP").expect("recover");
    assert_eq!(read_all(&root, "BOOK.BIN"), b"interrupted");
    assert!(matches!(
        root.find_directory_entry("STAGED.TMP"),
        Err(embedded_sdmmc::Error::NotFound)
    ));
}

/// A manager over a device that can fail writes after they land.
type FaultyManager =
    VolumeManager<utils::FailRegion<utils::RamDisk<Vec<u8>>>, utils::TestTimeSource, 4, 4, 1>;

/// A move over a device whose source-unlink write lands and then reports an
/// error, the way an SD card can take a sector and fail the status read after
/// it. `blind` also stops reads of the source directory from then on.
fn move_whose_unlink_lands_then_fails(
    short: bool,
    blind: bool,
) -> (
    Result<(), embedded_sdmmc::Error<utils::FailError>>,
    FaultyManager,
) {
    let device =
        utils::FailRegion::new(utils::make_block_device(utils::DISK_SOURCE).expect("disk image"));
    let manager: VolumeManager<_, _, 4, 4, 1> =
        VolumeManager::new_with_limits(device, utils::make_time_source(), 0xAA);
    let attempt = {
        let volume = manager.open_volume(VolumeIdx(0)).expect("volume");
        let root = volume.open_root_dir().expect("root");
        let source = root.open_dir("TEST").expect("TEST");
        let staged = source
            .open_file_in_dir("STAGED.TMP", Mode::ReadWriteCreate)
            .expect("create");
        staged.write(b"the only copy").expect("write");
        staged.close().expect("close");

        // The source directory's sector takes one write, the unlink, and
        // reports it failed; the destination is the root, elsewhere.
        let block = source
            .find_directory_entry("STAGED.TMP")
            .expect("entry")
            .entry_block
            .0;
        manager.device(|d| {
            d.write_region.set(Some((block, block + 1)));
            d.fail_write_number.set(Some(1));
            d.land_before_failing.set(true);
            d.blind_after_failing.set(blind);
        });
        let attempt = if short {
            source.move_file_in_dir("STAGED.TMP", &root, "MOVED.BIN")
        } else {
            source.move_file_in_dir_lfn("STAGED.TMP", &root, "A Real Book.epub")
        };
        manager.device(|d| {
            d.write_region.set(None);
            d.region.set(None);
            d.fail_write_number.set(None);
        });
        assert_eq!(
            manager.device(|d| d.writes_seen.get()),
            1,
            "the unlink was the one write to the source directory, and it was failed"
        );
        attempt
    };
    (attempt, manager)
}

fn after_the_move(manager: &FaultyManager, moved_name: &str) -> (bool, Option<Vec<u8>>) {
    let volume = manager.open_volume(VolumeIdx(0)).expect("volume");
    let root = volume.open_root_dir().expect("root");
    let source = root.open_dir("TEST").expect("TEST");
    let source_left = source.find_directory_entry("STAGED.TMP").is_ok();
    let moved = root
        .find_directory_entry(moved_name)
        .is_ok()
        .then(|| read_all(&root, moved_name));
    (source_left, moved)
}

/// The unlink landed, so the move happened: the destination is the file's
/// only name, and the move reports success rather than undoing itself into a
/// file with no name at all.
#[test]
fn a_move_whose_unlink_landed_despite_an_error_keeps_the_new_name() {
    for short in [false, true] {
        let (attempt, manager) = move_whose_unlink_lands_then_fails(short, false);
        assert!(attempt.is_ok(), "short={short}: {attempt:?}");
        let name = if short { "MOVED.BIN" } else { "AREALB~1.EPU" };
        let (source_left, moved) = after_the_move(&manager, name);
        assert!(!source_left, "short={short}: the old name is gone");
        assert_eq!(
            moved.as_deref(),
            Some(&b"the only copy"[..]),
            "short={short}"
        );
    }
}

/// The unlink may have landed and the directory will not say: the error is
/// returned and the destination stays, so the file has at least one name.
#[test]
fn a_move_that_cannot_tell_whether_its_unlink_landed_keeps_the_new_name() {
    for short in [false, true] {
        let (attempt, manager) = move_whose_unlink_lands_then_fails(short, true);
        assert!(attempt.is_err(), "short={short}");
        let name = if short { "MOVED.BIN" } else { "AREALB~1.EPU" };
        let (source_left, moved) = after_the_move(&manager, name);
        assert!(!source_left, "short={short}: the unlink did land");
        assert_eq!(
            moved.as_deref(),
            Some(&b"the only copy"[..]),
            "short={short}"
        );
    }
}

// ---------------------------------------------------------------------------
// The batched move: the short move's contract for a set of names, in a fixed
// number of walks
// ---------------------------------------------------------------------------

/// Create `count` files named `S000.BIN` onward, each holding its own name.
fn make_section_files<D: embedded_sdmmc::BlockDevice>(
    directory: &embedded_sdmmc::Directory<'_, D, utils::TestTimeSource, 4, 4, 1>,
    count: usize,
) -> Vec<String> {
    (0..count)
        .map(|i| {
            let name = format!("S{i:03}.BIN");
            let file = directory
                .open_file_in_dir(name.as_str(), Mode::ReadWriteCreate)
                .expect("create");
            file.write(name.as_bytes()).expect("write");
            file.close().expect("close");
            name
        })
        .collect()
}

/// Every name lands under exactly that name with its body and leaves the
/// source, as a run of single short moves would.
#[test]
fn a_batched_move_lands_every_name_under_exactly_that_name() {
    let manager = manager();
    let volume = manager.open_volume(VolumeIdx(1)).expect("volume");
    let root = volume.open_root_dir().expect("root");
    root.make_dir_in_dir("FROM").expect("make FROM");
    root.make_dir_in_dir("TO").expect("make TO");
    let from = root.open_dir("FROM").expect("FROM");
    let to = root.open_dir("TO").expect("TO");
    let names = make_section_files(&from, 20);
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();

    let fates = from
        .move_files_in_dir(&to, &refs[..16])
        .expect("first batch");
    assert_eq!(fates.len(), 16);
    assert!(fates.iter().all(|f| *f == MoveFate::Moved));
    let fates = from
        .move_files_in_dir(&to, &refs[16..])
        .expect("second batch");
    assert_eq!(fates.len(), 4);
    assert!(fates.iter().all(|f| *f == MoveFate::Moved));

    for name in &names {
        assert_eq!(read_all(&to, name), name.as_bytes(), "{name} kept its body");
        assert!(
            matches!(
                from.find_directory_entry(name.as_str()),
                Err(embedded_sdmmc::Error::NotFound)
            ),
            "{name} left the source"
        );
    }
    let mut storage = [0u8; 64];
    let mut lfn_buffer = embedded_sdmmc::LfnBuffer::new(&mut storage);
    let mut long_names = 0;
    to.iterate_dir_lfn(&mut lfn_buffer, |_, long| {
        long_names += usize::from(long.is_some());
        ControlFlow::Continue(())
    })
    .expect("iterate");
    assert_eq!(long_names, 0, "a batched move writes no long-name entries");
}

/// Each moved entry describes the source's chain, size and times, as a
/// single short link does.
#[test]
fn a_batched_move_carries_each_sources_chain_and_size() {
    let manager = manager();
    let volume = manager.open_volume(VolumeIdx(0)).expect("volume");
    let root = volume.open_root_dir().expect("root");
    let source = root.open_dir("TEST").expect("TEST");
    let names = make_section_files(&source, 3);
    let big = source
        .open_file_in_dir("BIG.BIN", Mode::ReadWriteCreate)
        .expect("create");
    big.write(&[7u8; 3000]).expect("write");
    big.close().expect("close");
    let moving = ["S000.BIN", "BIG.BIN", "S002.BIN"];

    let before: Vec<_> = moving
        .iter()
        .map(|n| source.find_directory_entry(*n).expect("look up"))
        .collect();
    source.move_files_in_dir(&root, &moving).expect("move");
    for (old, name) in before.iter().zip(moving) {
        let new = root.find_directory_entry(name).expect("look up");
        assert_eq!(new.cluster, old.cluster, "{name}: same first cluster");
        assert_eq!(new.size, old.size, "{name}: same size");
        assert_eq!(new.ctime, old.ctime, "{name}: same creation time");
        assert_eq!(new.mtime, old.mtime, "{name}: same modification time");
        assert_eq!(new.attributes, old.attributes, "{name}: same attributes");
        assert_eq!(new.name, ShortFileName::create_from_str(name).unwrap());
    }
    assert_eq!(read_all(&root, "BIG.BIN"), vec![7u8; 3000]);
    assert_eq!(read_all(&source, names[1].as_str()), names[1].as_bytes());
}

/// A name the destination answers to, by a short name or by a long name in
/// any case, is reported and its source left alone. A name the source lacks
/// is reported missing. The rest still move.
#[test]
fn a_batched_move_reports_taken_and_missing_names_and_moves_the_rest() {
    let manager = manager();
    let volume = manager.open_volume(VolumeIdx(0)).expect("volume");
    let root = volume.open_root_dir().expect("root");
    let source = root.open_dir("TEST").expect("TEST");
    make_section_files(&source, 4);
    let occupant = root
        .open_file_in_dir("S001.BIN", Mode::ReadWriteCreate)
        .expect("create occupant");
    occupant.write(b"do not clobber me").expect("write");
    occupant.close().expect("close");
    let long_occupant = root
        .create_file_in_dir_lfn("s002.bin")
        .expect("create long occupant");
    long_occupant.write(b"long").expect("write");
    long_occupant.close().expect("close");

    let fates = source
        .move_files_in_dir(
            &root,
            &["S000.BIN", "S001.BIN", "S002.BIN", "GONE.BIN", "S003.BIN"],
        )
        .expect("move");
    assert_eq!(
        &fates[..],
        &[
            MoveFate::Moved,
            MoveFate::AlreadyExists,
            MoveFate::AlreadyExists,
            MoveFate::NotFound,
            MoveFate::Moved,
        ]
    );
    assert_eq!(read_all(&root, "S001.BIN"), b"do not clobber me");
    assert_eq!(read_all(&source, "S001.BIN"), b"S001.BIN");
    assert_eq!(read_all(&source, "S002.BIN"), b"S002.BIN");
    assert_eq!(read_all(&root, "S000.BIN"), b"S000.BIN");
    assert_eq!(read_all(&root, "S003.BIN"), b"S003.BIN");
    assert!(matches!(
        root.find_directory_entry("GONE.BIN"),
        Err(embedded_sdmmc::Error::NotFound)
    ));
}

/// A source with a long name loses its long-name entries with it, including
/// one in the block before its short entry.
#[test]
fn a_batched_move_unlinks_a_long_name_across_a_block_boundary() {
    let manager = manager();
    let volume = manager.open_volume(VolumeIdx(1)).expect("volume");
    let root = volume.open_root_dir().expect("root");
    root.make_dir_in_dir("FROM").expect("make FROM");
    let from = root.open_dir("FROM").expect("FROM");
    // `.` and `..` take slots 0 and 1 and these take 2 to 14, so a two-entry
    // long name starts in the last slot of the first block.
    make_section_files(&from, 13);
    let long = from
        .create_file_in_dir_lfn("A long name of twenty.txt")
        .expect("create long");
    long.write(b"long body").expect("write");
    long.close().expect("close");
    let alias = from.find_directory_entry("ALONGN~1.TXT").expect("alias");
    assert_eq!(
        alias.entry_offset, 32,
        "the short entry is its block's second slot"
    );
    let raw = |block: u32| {
        manager.device(|d| {
            let mut blocks = [Block::new()];
            d.read(&mut blocks, BlockIdx(block)).expect("read");
            blocks[0].clone()
        })
    };
    let before = raw(alias.entry_block.0 - 1);
    assert_eq!(
        before[480 + 11],
        0x0F,
        "the first long-name slot is in the block before"
    );

    from.move_files_in_dir(&root, &["ALONGN~1.TXT"])
        .expect("move");

    assert_eq!(read_all(&root, "ALONGN~1.TXT"), b"long body");
    assert_eq!(
        raw(alias.entry_block.0 - 1)[480],
        0xE5,
        "earlier long-name slot"
    );
    let block = raw(alias.entry_block.0);
    assert_eq!(block[0], 0xE5, "later long-name slot");
    assert_eq!(block[32], 0xE5, "short entry");
}

/// Cut the batch at each of its writes, as power loss. Every file keeps at
/// least one name on its chain, a file under two names has one chain, and
/// unlinking the source name of each twin and retrying finishes the move.
#[test]
fn a_batched_move_cut_at_any_write_leaves_every_file_a_name() {
    let mut twins_seen = 0;
    for cut in 1.. {
        let device =
            utils::FailRegion::new(utils::make_block_device(utils::DISK_SOURCE).expect("disk"));
        let manager: FaultyManager =
            VolumeManager::new_with_limits(device, utils::make_time_source(), 0xAA);
        let volume = manager.open_volume(VolumeIdx(1)).expect("volume");
        let root = volume.open_root_dir().expect("root");
        root.make_dir_in_dir("FROM").expect("make FROM");
        root.make_dir_in_dir("TO").expect("make TO");
        let from = root.open_dir("FROM").expect("FROM");
        let to = root.open_dir("TO").expect("TO");
        // Enough to span blocks on both sides.
        let names = make_section_files(&from, 16);
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let before: Vec<_> = refs
            .iter()
            .map(|n| from.find_directory_entry(*n).expect("entry").cluster)
            .collect();

        manager.device(|d| {
            d.write_region.set(Some((0, u32::MAX)));
            d.writes_seen.set(0);
            d.fail_writes_from.set(Some(cut));
        });
        let attempt = from.move_files_in_dir(&to, &refs);
        let failed = manager.device(|d| d.injected.get()) > 0;
        manager.device(|d| {
            d.fail_writes_from.set(None);
            d.write_region.set(None);
        });
        if !failed {
            assert!(
                attempt.is_ok(),
                "cut {cut}: no write failed, so the batch finished"
            );
            assert!(twins_seen > 0, "no cut left a file under two names");
            break;
        }
        assert!(attempt.is_err(), "cut {cut}: a failed write is reported");

        for (name, cluster) in refs.iter().zip(&before) {
            let old = from.find_directory_entry(*name).ok();
            let new = to.find_directory_entry(*name).ok();
            match (old, new) {
                (None, None) => panic!("cut {cut}: {name} lost every name"),
                (Some(old), Some(new)) => {
                    assert_eq!(old.cluster, new.cluster, "cut {cut}: {name} has one chain");
                    twins_seen += 1;
                    from.delete_entry_in_dir(*name).expect("recover");
                }
                (Some(old), None) => assert_eq!(old.cluster, *cluster),
                (None, Some(new)) => assert_eq!(new.cluster, *cluster),
            }
        }
        let fates = from.move_files_in_dir(&to, &refs).expect("retry");
        for (name, fate) in refs.iter().zip(&fates) {
            assert_ne!(*fate, MoveFate::AlreadyExists, "cut {cut}: {name}");
            assert_eq!(read_all(&to, name), name.as_bytes(), "cut {cut}: {name}");
        }
        assert!(cut < 64, "the batch made more writes than expected");
    }
}

/// Cut each write of a 16-name batch at every byte it changes, as a sector
/// the card took only the front of before power went, then lose every later
/// write. The torn directories show each file under its old name, its new
/// name, or both on one chain, and nothing else: no part of a name, no name
/// over a cluster or size that is not the file's. Unlinking the source side
/// of each twin and retrying finishes the move. The disk is reset between
/// cuts from the harness's undo log, so the image is unpacked once.
#[test]
fn a_batched_move_torn_inside_any_sector_leaves_every_file_a_name() {
    let device =
        utils::FailRegion::new(utils::make_block_device(utils::DISK_SOURCE).expect("disk"));
    let mut manager: FaultyManager =
        VolumeManager::new_with_limits(device, utils::make_time_source(), 0xAA);
    let names: Vec<String>;
    let before: Vec<_>;
    {
        let volume = manager.open_volume(VolumeIdx(1)).expect("volume");
        let root = volume.open_root_dir().expect("root");
        root.make_dir_in_dir("FROM").expect("make FROM");
        root.make_dir_in_dir("TO").expect("make TO");
        let from = root.open_dir("FROM").expect("FROM");
        // Enough to span blocks on both sides.
        names = make_section_files(&from, 16);
        before = names
            .iter()
            .map(|n| {
                from.find_directory_entry(n.as_str())
                    .expect("entry")
                    .cluster
            })
            .collect();
    }
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();

    // One clean batch, undone afterwards, to learn which bytes each write
    // changes: a cut just after each is a state no other cut reaches.
    manager.device(|d| {
        d.write_region.set(Some((0, u32::MAX)));
        d.writes_seen.set(0);
        // Count and log every write, fail none.
        d.fail_writes_from.set(Some(u32::MAX));
        d.log_changes.set(true);
        *d.undo.borrow_mut() = Some(Vec::new());
    });
    {
        let volume = manager.open_volume(VolumeIdx(1)).expect("volume");
        let root = volume.open_root_dir().expect("root");
        let from = root.open_dir("FROM").expect("FROM");
        let to = root.open_dir("TO").expect("TO");
        from.move_files_in_dir(&to, &refs).expect("clean batch");
        // Before the handles close: closing the volume writes too.
        manager.device(|d| {
            d.log_changes.set(false);
            d.fail_writes_from.set(None);
            d.write_region.set(None);
        });
    }
    let (device, time) = manager.free();
    device.restore().expect("restore");
    let changes = device.changes.replace(Vec::new());
    manager = VolumeManager::new_with_limits(device, time, 0xAA);
    assert!(
        changes.iter().any(|c| c.iter().any(|b| b % 32 != 0)),
        "no write changed a byte inside an entry, so no cut lands inside one"
    );

    let mut twins_seen = 0;
    let mut partial_blocks = 0;
    for (write, changed) in changes.iter().enumerate() {
        let number = write as u32 + 1;
        let mut cuts = vec![0];
        cuts.extend(changed.iter().map(|b| b + 1));
        for cut in cuts {
            manager.device(|d| {
                d.write_region.set(Some((0, u32::MAX)));
                d.writes_seen.set(0);
                d.injected.set(0);
                d.fail_writes_from.set(Some(number));
                d.land_before_failing.set(true);
                d.land_bytes_before_failing.set(Some(cut));
                *d.undo.borrow_mut() = Some(Vec::new());
            });
            {
                let volume = manager.open_volume(VolumeIdx(1)).expect("volume");
                let root = volume.open_root_dir().expect("root");
                let from = root.open_dir("FROM").expect("FROM");
                let to = root.open_dir("TO").expect("TO");
                let attempt = from.move_files_in_dir(&to, &refs);
                assert!(
                    manager.device(|d| d.injected.get()) > 0,
                    "write {number} cut {cut}: the batch made fewer writes than the clean run"
                );
                assert!(
                    attempt.is_err(),
                    "write {number} cut {cut}: the cut is reported"
                );
                manager.device(|d| {
                    d.fail_writes_from.set(None);
                    d.land_before_failing.set(false);
                    d.land_bytes_before_failing.set(None);
                    d.write_region.set(None);
                });

                // What a reader of the torn destination sees.
                let mut visible = 0;
                to.iterate_dir(|entry| {
                    let name = entry.name.to_string();
                    if name == "." || name == ".." {
                        return ControlFlow::Continue(());
                    }
                    let Some(i) = names.iter().position(|n| *n == name) else {
                        panic!("write {number} cut {cut}: TO lists {name:?}, which is no name of the batch");
                    };
                    assert_eq!(
                        entry.cluster, before[i],
                        "write {number} cut {cut}: {name} in TO stands over another chain"
                    );
                    assert_eq!(
                        entry.size as usize,
                        name.len(),
                        "write {number} cut {cut}: {name} in TO has another size"
                    );
                    visible += 1;
                    ControlFlow::Continue(())
                })
                .expect("list TO");
                // The cut fell inside a block's run of new entries: the state
                // a whole-write cut cannot produce.
                let per_block = changed.iter().filter(|b| **b % 32 == 0).count();
                if cut > 0 && per_block > 1 && visible % per_block != 0 {
                    partial_blocks += 1;
                }

                for (name, cluster) in refs.iter().zip(&before) {
                    let old = from.find_directory_entry(*name).ok();
                    let new = to.find_directory_entry(*name).ok();
                    match (old, new) {
                        (None, None) => panic!("write {number} cut {cut}: {name} lost every name"),
                        (Some(old), Some(new)) => {
                            assert_eq!(
                                old.cluster, new.cluster,
                                "write {number} cut {cut}: {name} has one chain"
                            );
                            twins_seen += 1;
                            from.delete_entry_in_dir(*name).expect("recover");
                        }
                        (Some(old), None) => assert_eq!(old.cluster, *cluster),
                        (None, Some(new)) => assert_eq!(new.cluster, *cluster),
                    }
                }
                let fates = from.move_files_in_dir(&to, &refs).expect("retry");
                for (name, fate) in refs.iter().zip(&fates) {
                    assert_ne!(
                        *fate,
                        MoveFate::AlreadyExists,
                        "write {number} cut {cut}: {name}"
                    );
                    assert_eq!(
                        read_all(&to, name),
                        name.as_bytes(),
                        "write {number} cut {cut}: {name}"
                    );
                }
            }
            let (device, time) = manager.free();
            device.restore().expect("restore");
            manager = VolumeManager::new_with_limits(device, time, 0xAA);
        }
    }
    assert!(twins_seen > 0, "no cut left a file under two names");
    assert!(
        partial_blocks > 0,
        "no cut left part of a block's new entries visible, so none fell inside the second write"
    );
}

/// The single short move's link, torn at every byte it changes as a sector
/// the card took only the front of, then power loss. The destination lists
/// the file whole on its own chain or not at all, the file keeps a name, and
/// the retry finishes.
#[test]
fn a_short_move_torn_inside_its_sector_leaves_the_file_a_name() {
    let device =
        utils::FailRegion::new(utils::make_block_device(utils::DISK_SOURCE).expect("disk"));
    let mut manager: FaultyManager =
        VolumeManager::new_with_limits(device, utils::make_time_source(), 0xAA);
    let cluster;
    {
        let volume = manager.open_volume(VolumeIdx(1)).expect("volume");
        let root = volume.open_root_dir().expect("root");
        root.make_dir_in_dir("FROM").expect("make FROM");
        root.make_dir_in_dir("TO").expect("make TO");
        let from = root.open_dir("FROM").expect("FROM");
        let file = from
            .open_file_in_dir("BOOK.BIN", Mode::ReadWriteCreate)
            .expect("create");
        file.write(b"the index").expect("write");
        file.close().expect("close");
        cluster = from
            .find_directory_entry("BOOK.BIN")
            .expect("entry")
            .cluster;
    }

    manager.device(|d| {
        d.write_region.set(Some((0, u32::MAX)));
        d.writes_seen.set(0);
        d.fail_writes_from.set(Some(u32::MAX));
        d.log_changes.set(true);
        *d.undo.borrow_mut() = Some(Vec::new());
    });
    {
        let volume = manager.open_volume(VolumeIdx(1)).expect("volume");
        let root = volume.open_root_dir().expect("root");
        let from = root.open_dir("FROM").expect("FROM");
        let to = root.open_dir("TO").expect("TO");
        from.move_file_in_dir("BOOK.BIN", &to, "BOOK.BIN")
            .expect("clean move");
        manager.device(|d| {
            d.log_changes.set(false);
            d.fail_writes_from.set(None);
            d.write_region.set(None);
        });
    }
    let (device, time) = manager.free();
    device.restore().expect("restore");
    let changes = device.changes.replace(Vec::new());
    manager = VolumeManager::new_with_limits(device, time, 0xAA);
    assert_eq!(changes.len(), 3, "a link of two writes and an unlink");

    let mut twins_seen = 0;
    for (write, changed) in changes.iter().enumerate() {
        let number = write as u32 + 1;
        let mut cuts = vec![0];
        cuts.extend(changed.iter().map(|b| b + 1));
        for cut in cuts {
            manager.device(|d| {
                d.write_region.set(Some((0, u32::MAX)));
                d.writes_seen.set(0);
                d.injected.set(0);
                d.fail_writes_from.set(Some(number));
                d.land_before_failing.set(true);
                d.land_bytes_before_failing.set(Some(cut));
                *d.undo.borrow_mut() = Some(Vec::new());
            });
            {
                let volume = manager.open_volume(VolumeIdx(1)).expect("volume");
                let root = volume.open_root_dir().expect("root");
                let from = root.open_dir("FROM").expect("FROM");
                let to = root.open_dir("TO").expect("TO");
                // A torn unlink whose mark landed is a move that happened,
                // and the move looks before saying otherwise, so the attempt
                // may report success.
                let _ = from.move_file_in_dir("BOOK.BIN", &to, "BOOK.BIN");
                assert!(
                    manager.device(|d| d.injected.get()) > 0,
                    "write {number} cut {cut}: fewer writes than the clean run"
                );
                manager.device(|d| {
                    d.fail_writes_from.set(None);
                    d.land_before_failing.set(false);
                    d.land_bytes_before_failing.set(None);
                    d.write_region.set(None);
                });

                to.iterate_dir(|entry| {
                    let name = entry.name.to_string();
                    if name != "." && name != ".." {
                        assert_eq!(
                            name, "BOOK.BIN",
                            "write {number} cut {cut}: TO lists {name:?}"
                        );
                        assert_eq!(entry.cluster, cluster, "write {number} cut {cut}");
                        assert_eq!(entry.size, 9, "write {number} cut {cut}");
                    }
                    ControlFlow::Continue(())
                })
                .expect("list TO");
                let old = from.find_directory_entry("BOOK.BIN").ok();
                let new = to.find_directory_entry("BOOK.BIN").ok();
                match (old, new) {
                    (None, None) => {
                        panic!("write {number} cut {cut}: the file lost every name")
                    }
                    (Some(old), Some(new)) => {
                        assert_eq!(old.cluster, new.cluster, "write {number} cut {cut}");
                        twins_seen += 1;
                        from.delete_entry_in_dir("BOOK.BIN").expect("recover");
                    }
                    (Some(old), None) => {
                        assert_eq!(old.cluster, cluster);
                        from.move_file_in_dir("BOOK.BIN", &to, "BOOK.BIN")
                            .expect("retry");
                    }
                    (None, Some(new)) => assert_eq!(new.cluster, cluster),
                }
                assert_eq!(
                    read_all(&to, "BOOK.BIN"),
                    b"the index",
                    "write {number} cut {cut}"
                );
            }
            let (device, time) = manager.free();
            device.restore().expect("restore");
            manager = VolumeManager::new_with_limits(device, time, 0xAA);
        }
    }
    assert!(twins_seen > 0, "no cut left the file under two names");
}

/// The single move's refusals, made before anything is written, plus the
/// batch's own: too many names, or one name twice.
#[test]
fn a_batched_move_refuses_before_writing() {
    use embedded_sdmmc::Error;
    let manager = manager();
    let volume = manager.open_volume(VolumeIdx(1)).expect("volume");
    let root = volume.open_root_dir().expect("root");
    root.make_dir_in_dir("FROM").expect("make FROM");
    let from = root.open_dir("FROM").expect("FROM");
    let names = make_section_files(&from, 17);
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    from.make_dir_in_dir("SUBDIR").expect("make SUBDIR");

    assert!(matches!(
        from.move_files_in_dir(&root, &refs),
        Err(Error::Unsupported)
    ));
    assert!(matches!(
        from.move_files_in_dir(&root, &["S000.BIN", "S000.BIN"]),
        Err(Error::Unsupported)
    ));
    assert!(matches!(
        from.move_files_in_dir(&root, &["S000.BIN", "SUBDIR"]),
        Err(Error::OpenedDirAsFile)
    ));
    let open = from
        .open_file_in_dir("S001.BIN", Mode::ReadWriteAppend)
        .expect("open");
    assert!(matches!(
        from.move_files_in_dir(&root, &["S000.BIN", "S001.BIN"]),
        Err(Error::FileAlreadyOpen)
    ));
    open.close().expect("close");

    let other = self::manager();
    let other_volume = other.open_volume(VolumeIdx(1)).expect("volume");
    let other_root = other_volume.open_root_dir().expect("root");
    assert!(matches!(
        from.move_files_in_dir(&other_root, &["S000.BIN"]),
        Err(Error::BadHandle)
    ));

    for name in ["S000.BIN", "S001.BIN"] {
        assert_eq!(read_all(&from, name), name.as_bytes(), "{name} stayed");
        assert!(matches!(
            root.find_directory_entry(name),
            Err(Error::NotFound)
        ));
    }
}

/// Counts the blocks a manager reads and writes.
struct Counting<D> {
    inner: D,
    reads: std::cell::Cell<u32>,
    writes: std::cell::Cell<u32>,
}

impl<D: BlockDevice> BlockDevice for Counting<D> {
    type Error = D::Error;

    fn read(&self, blocks: &mut [Block], start: BlockIdx) -> Result<(), D::Error> {
        self.reads.set(self.reads.get() + blocks.len() as u32);
        self.inner.read(blocks, start)
    }

    fn write(&self, blocks: &[Block], start: BlockIdx) -> Result<(), D::Error> {
        self.writes.set(self.writes.get() + blocks.len() as u32);
        self.inner.write(blocks, start)
    }

    fn num_blocks(&self) -> Result<embedded_sdmmc::BlockCount, D::Error> {
        self.inner.num_blocks()
    }
}

/// The point of the batch: 64 files cost a fixed number of walks per batch
/// rather than six per name, and two writes per destination block and one
/// per source block, against three per name.
#[test]
fn a_batched_move_costs_walks_per_batch_not_per_name() {
    let count = |batched: bool| {
        let device = Counting {
            inner: utils::make_block_device(utils::DISK_SOURCE).expect("disk image"),
            reads: Default::default(),
            writes: Default::default(),
        };
        let manager: VolumeManager<_, _, 4, 4, 1> =
            VolumeManager::new_with_limits(device, utils::make_time_source(), 0xAA);
        let volume = manager.open_volume(VolumeIdx(1)).expect("volume");
        let root = volume.open_root_dir().expect("root");
        root.make_dir_in_dir("FROM").expect("make FROM");
        root.make_dir_in_dir("TO").expect("make TO");
        let from = root.open_dir("FROM").expect("FROM");
        let to = root.open_dir("TO").expect("TO");
        let names = make_section_files(&from, 64);
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let (reads, writes) = manager.device(|d| (d.reads.get(), d.writes.get()));
        if batched {
            for chunk in refs.chunks(embedded_sdmmc::MAX_MOVE_BATCH) {
                from.move_files_in_dir(&to, chunk).expect("batch");
            }
        } else {
            for name in &refs {
                from.move_file_in_dir(*name, &to, *name).expect("move");
            }
        }
        let cost = manager.device(|d| (d.reads.get() - reads, d.writes.get() - writes));
        for name in &refs {
            assert_eq!(read_all(&to, name), name.as_bytes());
        }
        cost
    };
    let (single_reads, single_writes) = count(false);
    let (batch_reads, batch_writes) = count(true);
    println!(
        "64 files: single {single_reads}r/{single_writes}w, batched {batch_reads}r/{batch_writes}w"
    );
    assert!(
        batch_reads * 4 < single_reads,
        "batched {batch_reads} reads against {single_reads}"
    );
    assert!(
        batch_writes * 5 < single_writes,
        "batched {batch_writes} writes against {single_writes}"
    );
}
