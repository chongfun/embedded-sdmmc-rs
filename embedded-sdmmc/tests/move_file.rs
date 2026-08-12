//! Moving a file by moving its directory entry.
//!
//! A move on FAT is two writes: give the chain a second name, then take the
//! first name away. There is no way to make that one atomic write, so the
//! window between them is part of the primitive's contract rather than an
//! implementation detail, and these tests pin down what is true inside it.

use core::ops::ControlFlow;

use embedded_sdmmc::{ClusterId, Mode, ShortFileName, VolumeIdx, VolumeManager};

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
