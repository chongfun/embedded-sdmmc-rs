//! A directory walk that cannot read the FAT must say so.
//!
//! Following a directory's cluster chain means reading the FAT, and that
//! read can fail. If the failure is discarded the walk simply ends, and the
//! caller is told the name it asked for is not there — which is the same
//! answer it would get for a name that genuinely does not exist.
//!
//! Callers build on that distinction. A journalling caller uses "this file
//! is absent" as evidence about what a previous run managed to do, so a
//! transient read error that arrives disguised as absence makes it undo work
//! that was actually completed.

use embedded_sdmmc::{Block, BlockDevice, BlockIdx, Mode, VolumeIdx, VolumeManager};
use std::cell::Cell;

use utils::FailRegion;

mod utils;

fn u16_at(block: &Block, offset: usize) -> u32 {
    u32::from(u16::from_le_bytes([
        block.contents[offset],
        block.contents[offset + 1],
    ]))
}

fn u32_at(block: &Block, offset: usize) -> u32 {
    u32::from_le_bytes([
        block.contents[offset],
        block.contents[offset + 1],
        block.contents[offset + 2],
        block.contents[offset + 3],
    ])
}

/// The blocks holding the FAT itself, read out of the given partition's BPB.
fn fat_region<D: BlockDevice>(device: &D, partition: usize) -> (u32, u32) {
    let mut block = [Block::new()];
    device.read(&mut block, BlockIdx(0)).ok();
    let partition_start = u32_at(&block[0], 446 + 16 * partition + 8);

    device.read(&mut block, BlockIdx(partition_start)).ok();
    let reserved = u16_at(&block[0], 14);
    let fat_count = u32::from(block[0].contents[16]);
    let per_fat = match u16_at(&block[0], 22) {
        0 => u32_at(&block[0], 36),
        short => short,
    };

    let start = partition_start + reserved;
    (start, start + fat_count * per_fat)
}

#[test]
fn a_lookup_that_cannot_read_the_fat_is_not_a_missing_file() {
    let inner = utils::make_block_device(utils::DISK_SOURCE).expect("disk image");
    let (fat_start, fat_end) = fat_region(&inner, 1);
    let device = FailRegion {
        inner,
        region: Cell::new(None),
        write_region: Cell::new(None),
        injected: Cell::new(0),
        writes_seen: Cell::new(0),
        fail_writes_from: Cell::new(None),
        fail_write_number: Cell::new(None),
    };

    let manager: VolumeManager<_, _, 4, 4, 1> =
        VolumeManager::new_with_limits(device, utils::make_time_source(), 0xAA);
    let volume = manager.open_volume(VolumeIdx(1)).expect("open FAT32 volume");
    let root = volume.open_root_dir().expect("open root");

    // The walk only consults the FAT when it runs off the end of a cluster.
    // A directory that fits in one cluster stops at the end-of-directory
    // marker instead, so it would never reach the code under test. One
    // FAT32 cluster here is 4096 bytes -- 128 directory entries -- so fill
    // past that: "." and ".." plus 130 files leaves the first cluster
    // entirely full of valid entries and forces the chain to be followed.
    root.make_dir_in_dir("CHAINDIR").expect("make CHAINDIR");
    let directory = root.open_dir("CHAINDIR").expect("open CHAINDIR");
    for i in 0..130 {
        let name = format!("F{i:07}.TXT");
        directory
            .open_file_in_dir(name.as_str(), Mode::ReadWriteCreate)
            .expect("create file")
            .close()
            .expect("close file");
    }

    // A name that is genuinely absent, with the FAT readable: NotFound is the
    // right answer, and it is what the caller is entitled to act on.
    let absent = directory.open_file_in_dir("GHOST.TXT", Mode::ReadOnly);
    assert!(
        matches!(absent, Err(embedded_sdmmc::Error::NotFound)),
        "a name that is not there must read as NotFound, got {:?}",
        absent.err()
    );

    // The same lookup with the FAT unreadable. The walk cannot finish, so the
    // driver does not know whether the name is there, and must not claim it
    // is absent.
    manager.device(|d| d.region.set(Some((fat_start, fat_end))));
    let unknowable = directory.open_file_in_dir("GHOST.TXT", Mode::ReadOnly);
    manager.device(|d| d.region.set(None));

    assert!(
        manager.device(|d| d.injected.get()) > 0,
        "no read was intercepted, so this proves nothing about the failure path"
    );
    match unknowable {
        Err(embedded_sdmmc::Error::NotFound) => {
            panic!("a FAT read failure was reported as a missing file")
        }
        Err(embedded_sdmmc::Error::DeviceError(_)) => {}
        other => panic!("expected the read failure to surface, got {other:?}"),
    }
}

/// Creating an entry in a directory whose FAT cannot be read must not report
/// a full disk. A caller told "no space" reasonably gives up, or starts
/// deleting things to make room; neither is the right answer to a transient
/// read error, and the second loses data to fix a problem that was never
/// there.
///
/// Both halves of the create path can reach that wrong answer. Today the
/// lookup that runs first is what saves it: the search for a free slot stops
/// at the first entry that is free *or* deleted, so it never walks further
/// along the chain than the preceding lookup already did, and the lookup
/// surfaces the read failure before the slot search is even reached. The
/// matching arm in `write_new_directory_entry` is what keeps this guarantee
/// true if that ordering ever changes -- it is not what makes this test pass
/// today.
///
/// `entries_per_cluster` is the geometry of the volume under test: 32 bytes
/// per entry, over 2048-byte clusters on the FAT16 partition of the test
/// image and 4096-byte clusters on the FAT32 one.
fn a_create_that_cannot_read_the_fat_is_not_a_full_disk(
    partition: usize,
    volume_idx: usize,
    entries_per_cluster: usize,
) {
    let inner = utils::make_block_device(utils::DISK_SOURCE).expect("disk image");
    let (fat_start, fat_end) = fat_region(&inner, partition);
    let device = FailRegion {
        inner,
        region: Cell::new(None),
        write_region: Cell::new(None),
        injected: Cell::new(0),
        writes_seen: Cell::new(0),
        fail_writes_from: Cell::new(None),
        fail_write_number: Cell::new(None),
    };

    let manager: VolumeManager<_, _, 4, 4, 1> =
        VolumeManager::new_with_limits(device, utils::make_time_source(), 0xAA);
    let volume = manager
        .open_volume(VolumeIdx(volume_idx))
        .expect("open volume");
    let root = volume.open_root_dir().expect("open root");

    // "." and ".." take the first two entries; fill the rest so that the
    // cluster holds no free slot at all. Anything created after this has to
    // leave the cluster to find room, and leaving means reading the FAT.
    root.make_dir_in_dir("FULLDIR").expect("make FULLDIR");
    let directory = root.open_dir("FULLDIR").expect("open FULLDIR");
    for i in 0..(entries_per_cluster - 2) {
        let name = format!("F{i:07}.TXT");
        directory
            .open_file_in_dir(name.as_str(), Mode::ReadWriteCreate)
            .expect("create file")
            .close()
            .expect("close file");
    }

    manager.device(|d| d.region.set(Some((fat_start, fat_end))));
    let created = directory.open_file_in_dir("ONEMORE.TXT", Mode::ReadWriteCreate);
    manager.device(|d| d.region.set(None));

    assert!(
        manager.device(|d| d.injected.get()) > 0,
        "no read was intercepted, so this proves nothing about the failure path"
    );
    // Drop the handle on success so the outcome is comparable either way.
    match created.map(|file| {
        file.close().ok();
    }) {
        Err(embedded_sdmmc::Error::NotEnoughSpace) => {
            panic!("a FAT write failure was reported as a full disk")
        }
        Err(embedded_sdmmc::Error::DeviceError(_)) => {}
        other => panic!("expected the read failure to surface, got {other:?}"),
    }
}

#[test]
fn a_create_that_cannot_read_the_fat_is_not_a_full_disk_fat16() {
    a_create_that_cannot_read_the_fat_is_not_a_full_disk(0, 0, 64);
}

#[test]
fn a_create_that_cannot_read_the_fat_is_not_a_full_disk_fat32() {
    a_create_that_cannot_read_the_fat_is_not_a_full_disk(1, 1, 128);
}

/// Growing a file past its current chain allocates a cluster, and that
/// allocation reads the FAT to find a free one. Only actually running out of
/// clusters is a full disk; a read that failed on the way to that answer has
/// established nothing about free space.
///
/// The distinction matters more here than in a lookup. A caller told "no
/// space" does not retry -- it gives up, or it starts deleting things to make
/// room, which destroys data to solve a problem that was never there.
#[test]
fn a_write_that_cannot_update_the_fat_is_not_a_full_disk() {
    let inner = utils::make_block_device(utils::DISK_SOURCE).expect("disk image");
    let (fat_start, fat_end) = fat_region(&inner, 1);
    let device = FailRegion {
        inner,
        region: Cell::new(None),
        write_region: Cell::new(None),
        injected: Cell::new(0),
        writes_seen: Cell::new(0),
        fail_writes_from: Cell::new(None),
        fail_write_number: Cell::new(None),
    };

    let manager: VolumeManager<_, _, 4, 4, 1> =
        VolumeManager::new_with_limits(device, utils::make_time_source(), 0xAA);
    let volume = manager.open_volume(VolumeIdx(1)).expect("open FAT32 volume");
    let root = volume.open_root_dir().expect("open root");

    let file = root
        .open_file_in_dir("GROWING.TXT", Mode::ReadWriteCreate)
        .expect("create file");
    // Fill the first cluster exactly, so the next write has to extend the
    // chain and therefore has to read the FAT.
    file.write(&[b'a'; 4096]).expect("fill first cluster");

    // Claiming the new cluster means writing the FAT. Failing that write is
    // what a worn sector looks like, and it is the failure alloc_cluster
    // reports -- the free-cluster search before it is usually served from the
    // block cache the chain walk just filled.
    manager.device(|d| d.write_region.set(Some((fat_start, fat_end))));
    let grown = file.write(&[b'b'; 512]);
    manager.device(|d| d.write_region.set(None));

    assert!(
        manager.device(|d| d.injected.get()) > 0,
        "no read was intercepted, so this proves nothing about the failure path"
    );
    match grown {
        Err(embedded_sdmmc::Error::DiskFull) => {
            panic!("a FAT write failure was reported as a full disk")
        }
        Err(embedded_sdmmc::Error::AllocationError) => {
            panic!("a FAT read failure was flattened into AllocationError")
        }
        Err(embedded_sdmmc::Error::DeviceError(_)) => {}
        other => panic!("expected the read failure to surface, got {other:?}"),
    }
}

/// The long-name open path asks the same question the short-name one does,
/// and until now gave the same wrong answer: any failure became NotFound. A
/// caller that reasons about absence has no way to tell "this book is not on
/// the card" from "the card could not be read".
#[test]
fn a_long_name_open_that_cannot_read_the_fat_is_not_a_missing_file() {
    let inner = utils::make_block_device(utils::DISK_SOURCE).expect("disk image");
    let (fat_start, fat_end) = fat_region(&inner, 1);
    let device = FailRegion {
        inner,
        region: Cell::new(None),
        write_region: Cell::new(None),
        injected: Cell::new(0),
        writes_seen: Cell::new(0),
        fail_writes_from: Cell::new(None),
        fail_write_number: Cell::new(None),
    };

    let manager: VolumeManager<_, _, 4, 4, 1> =
        VolumeManager::new_with_limits(device, utils::make_time_source(), 0xAA);
    let volume = manager.open_volume(VolumeIdx(1)).expect("open FAT32 volume");
    let root = volume.open_root_dir().expect("open root");

    // As in the short-name case, the walk only reaches the FAT once the
    // directory's entries fill a cluster. 128 entries per 4096-byte cluster.
    root.make_dir_in_dir("LFNDIR").expect("make LFNDIR");
    let directory = root.open_dir("LFNDIR").expect("open LFNDIR");
    for i in 0..130 {
        let name = format!("F{i:07}.TXT");
        directory
            .open_file_in_dir(name.as_str(), Mode::ReadWriteCreate)
            .expect("create file")
            .close()
            .expect("close file");
    }

    let absent = directory.open_long_name_file_in_dir("No Such Book.epub", Mode::ReadOnly);
    assert!(
        matches!(absent, Err(embedded_sdmmc::Error::NotFound)),
        "a long name that is not there must read as NotFound, got {:?}",
        absent.err()
    );

    manager.device(|d| d.region.set(Some((fat_start, fat_end))));
    let unknowable = directory.open_long_name_file_in_dir("No Such Book.epub", Mode::ReadOnly);
    manager.device(|d| d.region.set(None));

    assert!(
        manager.device(|d| d.injected.get()) > 0,
        "no read was intercepted, so this proves nothing about the failure path"
    );
    match unknowable {
        Err(embedded_sdmmc::Error::NotFound) => {
            panic!("a FAT read failure was reported as a missing file")
        }
        Err(embedded_sdmmc::Error::DeviceError(_)) => {}
        other => panic!("expected the read failure to surface, got {other:?}"),
    }
}

/// Fill a partition's FAT so that exactly one cluster is free.
///
/// Written through the block device before anything is mounted, so the driver
/// simply finds a volume with one cluster left.
fn leave_one_free_cluster<D: BlockDevice>(device: &D, partition: usize, free_cluster: u32) {
    let (fat_start, fat_end) = fat_region(device, partition);
    let mut full = [Block::new()];
    full[0].contents.fill(0xFF);
    for idx in fat_start..fat_end {
        device.write(&full, BlockIdx(idx)).ok();
    }
    // FAT16: two bytes per entry, so the one we spare sits in the first block.
    let mut first = [Block::new()];
    device.read(&mut first, BlockIdx(fat_start)).ok();
    let offset = (free_cluster * 2) as usize;
    first[0].contents[offset] = 0x00;
    first[0].contents[offset + 1] = 0x00;
    device.write(&first, BlockIdx(fat_start)).ok();
}

/// Claiming the last free cluster on a volume must still count as claiming it.
///
/// Once the FAT says the cluster is ours, the allocation has happened. What
/// follows is a search for where the *next* one should start looking, which is
/// a hint and nothing more -- and on a volume with one cluster left that search
/// necessarily finds nothing. Reporting that as failure would strand the
/// cluster just claimed: the caller never gets to record it in the directory
/// entry, so it is allocated and unreachable.
#[test]
fn claiming_the_last_free_cluster_still_succeeds() {
    let inner = utils::make_block_device(utils::DISK_SOURCE).expect("disk image");
    leave_one_free_cluster(&inner, 0, 3);

    let manager: VolumeManager<_, _, 4, 4, 1> =
        VolumeManager::new_with_limits(inner, utils::make_time_source(), 0xAA);
    let volume = manager.open_volume(VolumeIdx(0)).expect("open FAT16 volume");
    let root = volume.open_root_dir().expect("open root");

    let file = root
        .open_file_in_dir("LAST.TXT", Mode::ReadWriteCreate)
        .expect("create file");
    // The first write allocates the file's first cluster -- the only one left.
    file.write(b"the last cluster").expect("write into the last free cluster");
    file.close().expect("close");

    // The claim has to have been recorded, or the cluster is orphaned.
    let entry = root.find_directory_entry("LAST.TXT").expect("find entry");
    assert_ne!(
        entry.cluster,
        embedded_sdmmc::ClusterId::EMPTY,
        "the file was written but its entry records no cluster -- the claimed \
         cluster is allocated and unreachable"
    );
    assert_eq!(entry.size, 16);

    let reopened = root
        .open_file_in_dir("LAST.TXT", Mode::ReadOnly)
        .expect("reopen");
    let mut body = [0u8; 16];
    reopened.read(&mut body).expect("read back");
    assert_eq!(&body, b"the last cluster");
    reopened.close().ok();
}

/// The allocator starts looking from the FSInfo hint and, finding nothing,
/// wraps around to the start of the FAT. "Finding nothing" and "could not
/// read" are different answers: a range that could not be read has not been
/// searched, so wrapping past it lets a free cluster lower down hide the
/// failure completely and the allocation reports success.
#[test]
fn a_failed_first_scan_is_not_a_reason_to_wrap_around() {
    let inner = utils::make_block_device(utils::DISK_SOURCE).expect("disk image");

    // Point the volume's next-free hint high up the FAT, so the first scan
    // covers a different region from the wrapped one.
    const HINT: u32 = 40_000;
    let mut mbr = [Block::new()];
    inner.read(&mut mbr, BlockIdx(0)).expect("read MBR");
    let partition_start = u32_at(&mbr[0], 446 + 16 + 8);
    let mut bpb = [Block::new()];
    inner
        .read(&mut bpb, BlockIdx(partition_start))
        .expect("read BPB");
    let info_location = u16_at(&bpb[0], 48);

    let mut info = [Block::new()];
    let info_idx = BlockIdx(partition_start + info_location);
    inner.read(&mut info, info_idx).expect("read FSInfo");
    info[0].contents[492..496].copy_from_slice(&HINT.to_le_bytes());
    inner.write(&info, info_idx).expect("write FSInfo");

    let (fat_start, fat_end) = fat_region(&inner, 1);
    // FAT32 packs 128 entries into a 512-byte block.
    let hint_block = fat_start + HINT / 128;

    let device = FailRegion {
        inner,
        region: Cell::new(None),
        write_region: Cell::new(None),
        injected: Cell::new(0),
        writes_seen: Cell::new(0),
        fail_writes_from: Cell::new(None),
        fail_write_number: Cell::new(None),
    };
    let manager: VolumeManager<_, _, 4, 4, 1> =
        VolumeManager::new_with_limits(device, utils::make_time_source(), 0xAA);
    let volume = manager.open_volume(VolumeIdx(1)).expect("open FAT32 volume");
    let root = volume.open_root_dir().expect("open root");

    let file = root
        .open_file_in_dir("WRAPPED.TXT", Mode::ReadWriteCreate)
        .expect("create file");

    // Unreadable from the hint upwards, readable below it -- where there are
    // plenty of free clusters for a wrapped scan to find.
    manager.device(|d| d.region.set(Some((hint_block, fat_end))));
    let written = file.write(b"wrapped");
    manager.device(|d| d.region.set(None));

    assert!(
        manager.device(|d| d.injected.get()) > 0,
        "no read was intercepted, so this proves nothing about the failure path"
    );
    match written {
        Ok(()) => panic!(
            "the allocation reported success by wrapping around a range it \
             could not read -- the read failure was lost entirely"
        ),
        Err(embedded_sdmmc::Error::DeviceError(_)) => {}
        other => panic!("expected the read failure to surface, got {other:?}"),
    }
    file.close().ok();
}

/// A long name is several directory entries, written one at a time, and they
/// commonly share a 512-byte sector. The cache holds one sector, so a failed
/// write leaves the attempted bytes sitting in it: if the cleanup that follows
/// then modifies a different entry in that same sector, its own write-back
/// carries the failed one to disk along with it.
///
/// The caller is told the name was not installed. For the move primitive that
/// is the whole contract -- a caller that believes the destination name does
/// not exist will keep or retry the source -- so the name must really not be
/// there.
#[test]
fn a_failed_entry_write_is_not_committed_by_its_own_cleanup() {
    // The long name needs two entries plus its short one, and the directory is
    // small enough that all three land in the same sector -- which is the
    // situation that makes the cleanup dangerous.
    const LONG_NAME: &str = "A Real Book.epub";
    const ALIAS: &str = "AREALB~1.EPU";

    // First, learn how many writes a successful create makes to the directory
    // region, so the last one -- the short-name entry -- can be singled out.
    let writes_for_a_create = {
        let inner = utils::make_block_device(utils::DISK_SOURCE).expect("disk image");
        let (dir_from, dir_to) = fat32_root_dir_blocks(&inner);
        let device = FailRegion {
            inner,
            region: Cell::new(None),
            write_region: Cell::new(None),
            injected: Cell::new(0),
            writes_seen: Cell::new(0),
            // A very large number: nothing matches, so every write goes through
            // while still being counted.
            fail_writes_from: Cell::new(None),
            fail_write_number: Cell::new(Some(u32::MAX)),
        };
        let manager: VolumeManager<_, _, 4, 4, 1> =
            VolumeManager::new_with_limits(device, utils::make_time_source(), 0xAA);
        let volume = manager.open_volume(VolumeIdx(1)).expect("open volume");
        let root = volume.open_root_dir().expect("open root");
        manager.device(|d| d.write_region.set(Some((dir_from, dir_to))));
        root.create_file_in_dir_lfn(LONG_NAME)
            .expect("create should succeed with no fault injected")
            .close()
            .expect("close");
        manager.device(|d| d.write_region.set(None));
        manager.device(|d| d.writes_seen.get())
    };
    assert!(
        writes_for_a_create >= 2,
        "expected the long name and its alias to be separate writes, saw {writes_for_a_create}"
    );

    // Now fail exactly the last of those writes, once. Everything after it --
    // the cleanup -- succeeds, which is what lets a still-dirty cache carry
    // the failed entry to disk.
    let inner = utils::make_block_device(utils::DISK_SOURCE).expect("disk image");
    let (dir_from, dir_to) = fat32_root_dir_blocks(&inner);
    let device = FailRegion {
        inner,
        region: Cell::new(None),
        write_region: Cell::new(None),
        injected: Cell::new(0),
        writes_seen: Cell::new(0),
        fail_writes_from: Cell::new(None),
        fail_write_number: Cell::new(Some(writes_for_a_create)),
    };
    let manager: VolumeManager<_, _, 4, 4, 1> =
        VolumeManager::new_with_limits(device, utils::make_time_source(), 0xAA);
    let volume = manager.open_volume(VolumeIdx(1)).expect("open FAT32 volume");
    let root = volume.open_root_dir().expect("open root");

    manager.device(|d| d.write_region.set(Some((dir_from, dir_to))));
    let attempt = root.create_file_in_dir_lfn(LONG_NAME);
    manager.device(|d| d.write_region.set(None));

    assert_eq!(
        manager.device(|d| d.injected.get()),
        1,
        "exactly one write should have been failed"
    );
    assert!(
        attempt.is_err(),
        "the create reported success despite its final entry write failing"
    );

    // Whatever the caller was told, neither name may be reachable.
    assert!(
        matches!(
            root.find_directory_entry(ALIAS),
            Err(embedded_sdmmc::Error::NotFound)
        ),
        "the create failed but its short alias is on disk"
    );
    let mut storage = [0u8; 256];
    let mut lfn_buffer = embedded_sdmmc::LfnBuffer::new(&mut storage);
    let mut seen_long_name = false;
    root.iterate_dir_lfn(&mut lfn_buffer, |_entry, long_name| {
        seen_long_name |= long_name == Some(LONG_NAME);
        core::ops::ControlFlow::Continue(())
    })
    .expect("iterate");
    assert!(
        !seen_long_name,
        "the create failed but its long name is on disk"
    );
}

/// The blocks making up the FAT32 root directory's first cluster.
fn fat32_root_dir_blocks<D: BlockDevice>(device: &D) -> (u32, u32) {
    let mut mbr = [Block::new()];
    device.read(&mut mbr, BlockIdx(0)).expect("read MBR");
    let partition_start = u32_at(&mbr[0], 446 + 16 + 8);
    let mut bpb = [Block::new()];
    device
        .read(&mut bpb, BlockIdx(partition_start))
        .expect("read BPB");
    let reserved = u16_at(&bpb[0], 14);
    let fat_count = u32::from(bpb[0].contents[16]);
    let per_fat = u32_at(&bpb[0], 36);
    let sectors_per_cluster = u32::from(bpb[0].contents[13]);
    let root_cluster = u32_at(&bpb[0], 44);
    let data_start = partition_start + reserved + fat_count * per_fat;
    let first = data_start + (root_cluster - 2) * sectors_per_cluster;
    (first, first + sectors_per_cluster)
}

/// Retiring a long-named entry is several sector writes, and any of them can
/// fail. Whichever one does, the caller must be left with the entry wholly
/// there or wholly gone -- never a live file whose long name has been partly
/// erased, which answers to neither the name it had nor the name it did not.
///
/// The short entry is the commit point, so it is written first: fail on it and
/// nothing changed, fail after it and the file is gone. What can be left over
/// is long-name entries with no short entry behind them, which readers skip.
#[test]
fn a_failed_delete_leaves_the_entry_wholly_there_or_wholly_gone() {
    const LONG_NAME: &str = "A Real Book.epub";
    const ALIAS: &str = "AREALB~1.EPU";

    // How many writes does retiring it take when nothing fails?
    let writes_for_a_delete = {
        let device = utils::FailRegion::new(
            utils::make_block_device(utils::DISK_SOURCE).expect("disk image"),
        );
        let manager: VolumeManager<_, _, 4, 4, 1> =
            VolumeManager::new_with_limits(device, utils::make_time_source(), 0xAA);
        let volume = manager.open_volume(VolumeIdx(1)).expect("volume");
        let root = volume.open_root_dir().expect("root");
        root.create_file_in_dir_lfn(LONG_NAME)
            .expect("create")
            .close()
            .expect("close");
        let entry = root.find_directory_entry(ALIAS).expect("entry");
        let block = entry.entry_block.0;
        let cluster_start = block - (block % 8);
        manager.device(|d| {
            d.write_region.set(Some((cluster_start, cluster_start + 8)));
            d.fail_write_number.set(Some(u32::MAX));
        });
        root.delete_entry_in_dir(ALIAS).expect("delete");
        manager.device(|d| d.writes_seen.get())
    };
    assert!(
        writes_for_a_delete >= 2,
        "expected the long name and its short entry to be separate writes, saw {writes_for_a_delete}"
    );

    // Now fail each of those writes in turn.
    for failing_write in 1..=writes_for_a_delete {
        let device = utils::FailRegion::new(
            utils::make_block_device(utils::DISK_SOURCE).expect("disk image"),
        );
        let manager: VolumeManager<_, _, 4, 4, 1> =
            VolumeManager::new_with_limits(device, utils::make_time_source(), 0xAA);
        let volume = manager.open_volume(VolumeIdx(1)).expect("volume");
        let root = volume.open_root_dir().expect("root");
        root.create_file_in_dir_lfn(LONG_NAME)
            .expect("create")
            .close()
            .expect("close");
        let entry = root.find_directory_entry(ALIAS).expect("entry");
        let block = entry.entry_block.0;
        let cluster_start = block - (block % 8);

        manager.device(|d| {
            d.writes_seen.set(0);
            d.write_region.set(Some((cluster_start, cluster_start + 8)));
            d.fail_write_number.set(Some(failing_write));
        });
        let outcome = root.delete_entry_in_dir(ALIAS);
        manager.device(|d| {
            d.write_region.set(None);
            d.fail_write_number.set(None);
        });

        let short_name_gone = matches!(
            root.find_directory_entry(ALIAS),
            Err(embedded_sdmmc::Error::NotFound)
        );
        let mut storage = [0u8; 256];
        let mut lfn_buffer = embedded_sdmmc::LfnBuffer::new(&mut storage);
        let mut long_name_present = false;
        root.iterate_dir_lfn(&mut lfn_buffer, |_e, long| {
            long_name_present |= long == Some(LONG_NAME);
            core::ops::ControlFlow::Continue(())
        })
        .expect("iterate");

        match outcome {
            // Reported failure: nothing may have been touched.
            Err(_) => assert!(
                !short_name_gone && long_name_present,
                "write {failing_write} failed and reported failure, but the entry \
                 was already partly retired"
            ),
            // Reported success: the file must be gone by both its names. Left
            // over long-name entries with no short entry behind them are not
            // reachable as a name, which is what this checks.
            Ok(()) => assert!(
                short_name_gone && !long_name_present,
                "write {failing_write} reported success but the file is still \
                 reachable by one of its names"
            ),
        }
    }
}
