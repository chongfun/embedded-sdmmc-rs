//! Freeing a chain one named cluster at a time.
//!
//! `truncate_cluster_chain` frees a chain by following it, which works only
//! while the links are intact — and freeing is what destroys them. A caller
//! that has to survive being interrupted mid-reclaim cannot use it: after a
//! reset, part of the chain is gone and there is nothing left to follow to
//! find the rest. These primitives let such a caller read the chain out
//! first, record the cluster numbers somewhere durable, and then free them
//! by name, replaying the list as often as it takes.

use core::cell::Cell;

use embedded_sdmmc::{
    Block, BlockDevice, BlockIdx, ClusterId, Error, Mode, VolumeIdx, VolumeManager,
};

use utils::FailRegion;

mod utils;

type Manager = VolumeManager<utils::RamDisk<Vec<u8>>, utils::TestTimeSource, 4, 4, 1>;

fn manager() -> Manager {
    let device = utils::make_block_device(utils::DISK_SOURCE).expect("disk image");
    VolumeManager::new_with_limits(device, utils::make_time_source(), 0xAA)
}

/// Every cluster of a file's chain, read out before anything is freed.
fn chain_of(
    manager: &Manager,
    volume: embedded_sdmmc::RawVolume,
    first: ClusterId,
) -> Vec<ClusterId> {
    let mut chain = vec![first];
    let mut at = first;
    while let Some(next) = manager
        .next_cluster_in_chain(volume, at)
        .expect("walk the chain")
    {
        chain.push(next);
        at = next;
    }
    chain
}

/// A file big enough to occupy several clusters, and the clusters it holds.
/// Volume 0 of the test image is FAT16 and volume 1 is FAT32, which matters
/// here: the two disagree about how a free FAT entry reads.
fn multi_cluster_file_on(
    manager: &Manager,
    volume_idx: VolumeIdx,
) -> (embedded_sdmmc::RawVolume, Vec<ClusterId>) {
    let volume = manager.open_raw_volume(volume_idx).expect("volume");
    let root = manager.open_root_dir(volume).expect("root");
    let directory = root.to_directory(manager);
    let file = directory
        .open_file_in_dir("BIG.DAT", Mode::ReadWriteCreateOrTruncate)
        .expect("create");
    // Several clusters' worth, whatever the image's cluster size is.
    file.write(&[0xA5; 40_000]).expect("write");
    file.close().expect("close");
    let first = directory
        .find_directory_entry("BIG.DAT")
        .expect("entry")
        .cluster;
    let chain = chain_of(manager, volume, first);
    assert!(
        chain.len() > 1,
        "the test needs a file spanning several clusters, got {}",
        chain.len()
    );
    (volume, chain)
}

fn multi_cluster_file(manager: &Manager) -> (embedded_sdmmc::RawVolume, Vec<ClusterId>) {
    multi_cluster_file_on(manager, VolumeIdx(0))
}

#[test]
fn a_chain_can_be_read_out_before_it_is_taken_apart() {
    let manager = manager();
    let (volume, chain) = multi_cluster_file(&manager);
    // Consecutive or not, every cluster is a real one and they are distinct.
    let mut seen = chain.clone();
    seen.sort_by_key(|cluster| cluster.value());
    seen.dedup_by_key(|cluster| cluster.value());
    assert_eq!(seen.len(), chain.len(), "a chain must not repeat a cluster");
    assert!(chain.iter().all(|cluster| cluster.value() >= 2));
}

#[test]
fn freeing_the_same_cluster_twice_is_not_an_error() {
    // The property the whole replayable reclaim rests on: a list of clusters
    // can be re-run after an interruption without knowing how far the
    // previous attempt got.
    let manager = manager();
    let (volume, chain) = multi_cluster_file(&manager);
    for cluster in &chain {
        manager.free_cluster(volume, *cluster).expect("first free");
    }
    for cluster in &chain {
        manager.free_cluster(volume, *cluster).expect("second free");
    }
    for cluster in &chain {
        manager.free_cluster(volume, *cluster).expect("third free");
    }
}

#[test]
fn a_freed_cluster_no_longer_says_what_followed_it() {
    // Why the cluster list has to be recorded in advance, and the whole
    // reason this pair of primitives exists.
    //
    // A cluster's own FAT entry is what names its successor, and freeing the
    // cluster is precisely overwriting that entry. So the moment a reclaim
    // frees one, the rest of the chain past it is unreachable: there is
    // nothing left on the card that says where it went. A reclaim
    // interrupted here could not resume from the file's first cluster,
    // however carefully that was recorded -- which is why the caller reads
    // the chain out and writes the numbers down before freeing any of them.
    let manager = manager();
    let (volume, chain) = multi_cluster_file(&manager);
    assert!(
        chain.len() >= 3,
        "need a middle cluster, got {}",
        chain.len()
    );

    // Before: the middle cluster names the one after it.
    assert_eq!(
        manager
            .next_cluster_in_chain(volume, chain[1])
            .expect("walk")
            .map(|cluster| cluster.value()),
        Some(chain[2].value()),
    );

    manager.free_cluster(volume, chain[1]).expect("free");

    // After: it names nothing, and says so rather than reporting an end that
    // would leak everything past it -- or, as FAT16 would left to itself,
    // cluster zero dressed up as a successor.
    assert!(matches!(
        manager.next_cluster_in_chain(volume, chain[1]),
        Err(Error::UnterminatedFatChain)
    ));
}

#[test]
fn the_walker_refuses_a_cluster_that_is_not_one() {
    // A number out of a journal that no longer matches the card looks
    // exactly like this. Refused rather than turned into a read at whatever
    // offset it computes to -- and the underlying walker panics outright on
    // large values, which in firmware that aborts on panic is the whole
    // device rather than one failed reclaim.
    let manager = manager();
    let volume = manager.open_raw_volume(VolumeIdx(0)).expect("volume");
    for not_a_cluster in [0u32, 1, u32::MAX / 8, u32::MAX / 2, u32::MAX] {
        assert!(
            matches!(
                manager.next_cluster_in_chain(volume, ClusterId::new(not_a_cluster)),
                Err(Error::BadCluster)
            ),
            "cluster {not_a_cluster:#x} should be refused"
        );
    }
}

#[test]
fn every_successor_handed_out_is_one_the_caller_may_free() {
    // The invariant the journal leans on: whatever the walk returns can be
    // written down and later freed, without the caller re-checking it.
    let manager = manager();
    let (volume, chain) = multi_cluster_file(&manager);
    for cluster in &chain {
        manager.free_cluster(volume, *cluster).expect("freeable");
    }
}

#[test]
fn the_reserved_entries_are_refused() {
    // Entries 0 and 1 are the media descriptor and the end-of-chain marker.
    // Writing them corrupts the FAT for every file on the volume, so a
    // caller replaying a list that somehow contains one is stopped rather
    // than obeyed.
    let manager = manager();
    let volume = manager.open_raw_volume(VolumeIdx(0)).expect("volume");
    for reserved in [0u32, 1] {
        assert!(
            matches!(
                manager.free_cluster(volume, ClusterId::new(reserved)),
                Err(Error::BadCluster)
            ),
            "cluster {reserved} should be refused"
        );
    }
}

#[test]
fn a_cluster_past_the_end_of_the_volume_is_refused() {
    let manager = manager();
    let volume = manager.open_raw_volume(VolumeIdx(0)).expect("volume");
    assert!(matches!(
        manager.free_cluster(volume, ClusterId::new(u32::MAX / 8)),
        Err(Error::BadCluster)
    ));
}

#[test]
fn freed_clusters_come_back_for_the_next_file() {
    // The point of reclaiming: the space is usable again afterwards, and the
    // volume's own free-space accounting agrees.
    let manager = manager();
    let (volume, chain) = multi_cluster_file(&manager);
    let root = manager.open_root_dir(volume).expect("root");
    let directory = root.to_directory(&manager);
    // Take the name away first, then the clusters: the ordering this
    // primitive exists to make possible.
    directory.delete_entry_in_dir("BIG.DAT").expect("unlink");
    for cluster in &chain {
        manager.free_cluster(volume, *cluster).expect("free");
    }
    let file = directory
        .open_file_in_dir("AFTER.DAT", Mode::ReadWriteCreateOrTruncate)
        .expect("create");
    file.write(&[0x5A; 40_000]).expect("write");
    file.close().expect("close");
    let mut body = vec![0u8; 40_000];
    let reopened = directory
        .open_file_in_dir("AFTER.DAT", Mode::ReadOnly)
        .expect("reopen");
    reopened.read(&mut body).expect("read");
    assert!(body.iter().all(|byte| *byte == 0x5A));
}

#[test]
fn fat16_and_fat32_report_a_freed_cluster_the_same_way() {
    // The reason the public walker does not simply delegate. FAT32 stores a
    // free entry as zero and the walker calls that an unterminated chain;
    // FAT16 stores the same thing and would hand back cluster zero as though
    // it were a successor -- which a caller would write into its journal and
    // then wedge on, trying to free a cluster that is not one.
    for volume_idx in [VolumeIdx(0), VolumeIdx(1)] {
        let manager = manager();
        let (volume, chain) = multi_cluster_file_on(&manager, volume_idx);
        manager.free_cluster(volume, chain[0]).expect("free");
        assert!(
            matches!(
                manager.next_cluster_in_chain(volume, chain[0]),
                Err(Error::UnterminatedFatChain)
            ),
            "volume {volume_idx:?} disagreed about a freed cluster",
        );
    }
}

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

/// `(first FAT block, blocks per FAT, number of FATs)` from the partition's
/// BPB, so a test can look at each FAT copy separately.
fn fat_layout<D: BlockDevice>(device: &D, partition: usize) -> (u32, u32, u32) {
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
    (partition_start + reserved, per_fat, fat_count)
}

#[test]
fn a_replay_repairs_a_half_mirrored_free() {
    // The failure this primitive is most likely to be interrupted by, and
    // the one a naive idempotence check hides.
    //
    // Clearing a FAT entry is two device writes: the primary FAT, then the
    // mirror. A reset between them leaves the copies disagreeing, with the
    // primary already reading free. A replay that asked "is it free?" and
    // stopped there would call the cluster reclaimed, clear its journal, and
    // leave the second FAT holding the old chain link permanently -- a
    // structural inconsistency surviving a *successful* recovery.
    let partition = 1; // FAT32, which the device's cards are.
    let inner = utils::make_block_device(utils::DISK_SOURCE).expect("disk image");
    let (fat_start, per_fat, fat_count) = fat_layout(&inner, partition);
    assert_eq!(fat_count, 2, "the test needs a mirrored FAT");

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
        .open_raw_volume(VolumeIdx(partition))
        .expect("volume");
    let root = manager.open_root_dir(volume).expect("root");
    let directory = root.to_directory(&manager);
    let file = directory
        .open_file_in_dir("MIRROR.DAT", Mode::ReadWriteCreateOrTruncate)
        .expect("create");
    file.write(&[0xC3; 40_000]).expect("write");
    file.close().expect("close");
    let first = directory
        .find_directory_entry("MIRROR.DAT")
        .expect("entry")
        .cluster;
    directory.delete_entry_in_dir("MIRROR.DAT").expect("unlink");

    // Cut the mirrored write in half: the primary FAT write lands, the
    // second one does not.
    manager.device(|d| {
        d.write_region
            .set(Some((fat_start, fat_start + fat_count * per_fat)));
        d.writes_seen.set(0);
        d.fail_write_number.set(Some(2));
    });
    let interrupted = manager.free_cluster(volume, first);
    assert!(
        interrupted.is_err(),
        "the mirror write was supposed to fail"
    );

    // Power comes back: no more injected failures, and the reclaim replays
    // its recorded list.
    manager.device(|d| {
        d.fail_write_number.set(None);
        d.write_region.set(None);
    });
    manager.free_cluster(volume, first).expect("replay");

    // Both copies of that FAT entry now say the same thing.
    let entry_offset = first.value() * 4;
    let entry_block = entry_offset / Block::LEN_U32;
    let within = (entry_offset % Block::LEN_U32) as usize;
    let mut primary = [Block::new()];
    let mut mirror = [Block::new()];
    manager
        .device(|d| {
            d.inner
                .read(&mut primary, BlockIdx(fat_start + entry_block))?;
            d.inner
                .read(&mut mirror, BlockIdx(fat_start + per_fat + entry_block))
        })
        .expect("read both FATs");
    let primary_entry = u32_at(&primary[0], within) & 0x0FFF_FFFF;
    let mirror_entry = u32_at(&mirror[0], within) & 0x0FFF_FFFF;
    assert_eq!(
        primary_entry, 0,
        "the primary FAT should show the cluster free"
    );
    assert_eq!(
        mirror_entry, primary_entry,
        "the two FAT copies disagree after a replay: primary {primary_entry:#x}, mirror {mirror_entry:#x}",
    );
}

#[test]
fn the_same_operations_are_reachable_from_a_directory() {
    // The shape the reclaim driver actually uses. It holds the directory the
    // entry stood in; making it carry a RawVolume alongside every directory
    // would put the plumbing in every signature it touches, for a device
    // that only ever has one volume open.
    let manager = manager();
    let (volume, chain) = multi_cluster_file(&manager);
    let root = manager.open_root_dir(volume).expect("root");
    let directory = root.to_directory(&manager);

    // Walking, and the same guarantee about what comes back.
    let mut walked = vec![chain[0]];
    let mut at = chain[0];
    while let Some(next) = directory.next_cluster_in_chain(at).expect("walk") {
        walked.push(next);
        at = next;
    }
    assert_eq!(
        walked.iter().map(|c| c.value()).collect::<Vec<_>>(),
        chain.iter().map(|c| c.value()).collect::<Vec<_>>(),
    );

    // And freeing, idempotently.
    directory.delete_entry_in_dir("BIG.DAT").expect("unlink");
    for cluster in &chain {
        directory.free_cluster(*cluster).expect("free");
        directory.free_cluster(*cluster).expect("free again");
    }
    assert!(matches!(
        directory.next_cluster_in_chain(chain[0]),
        Err(Error::UnterminatedFatChain)
    ));
}

#[test]
fn a_directory_refuses_the_same_clusters_the_volume_does() {
    // The validation is the volume's, not something the convenience layer
    // gets to skip.
    let manager = manager();
    let volume = manager.open_raw_volume(VolumeIdx(0)).expect("volume");
    let root = manager.open_root_dir(volume).expect("root");
    let directory = root.to_directory(&manager);
    for not_a_cluster in [0u32, 1, u32::MAX / 8] {
        assert!(matches!(
            directory.free_cluster(ClusterId::new(not_a_cluster)),
            Err(Error::BadCluster)
        ));
        assert!(matches!(
            directory.next_cluster_in_chain(ClusterId::new(not_a_cluster)),
            Err(Error::BadCluster)
        ));
    }
}
