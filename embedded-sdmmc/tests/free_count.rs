//! The FSInfo free-cluster count, as allocation and truncation keep it.

use embedded_sdmmc::{Block, BlockDevice, BlockIdx, Mode, VolumeIdx, VolumeManager};

mod utils;

fn u16_at(block: &Block, offset: usize) -> u32 {
    u32::from(u16::from_le_bytes([block[offset], block[offset + 1]]))
}

fn u32_at(block: &Block, offset: usize) -> u32 {
    u32::from_le_bytes(block[offset..offset + 4].try_into().unwrap())
}

/// The FAT32 volume's FSInfo sector and its cluster size in bytes.
fn fsinfo_and_cluster_bytes<D: BlockDevice>(device: &D) -> (BlockIdx, usize)
where
    D::Error: core::fmt::Debug,
{
    let mut mbr = [Block::new()];
    device.read(&mut mbr, BlockIdx(0)).expect("read MBR");
    let partition_start = u32_at(&mbr[0], 446 + 16 + 8);
    let mut bpb = [Block::new()];
    device
        .read(&mut bpb, BlockIdx(partition_start))
        .expect("read BPB");
    let info = BlockIdx(partition_start + u16_at(&bpb[0], 48));
    (info, bpb[0][13] as usize * Block::LEN)
}

fn set_free_count<D: BlockDevice>(device: &D, info: BlockIdx, count: u32)
where
    D::Error: core::fmt::Debug,
{
    let mut block = [Block::new()];
    device.read(&mut block, info).expect("read FSInfo");
    block[0][488..492].copy_from_slice(&count.to_le_bytes());
    device.write(&block, info).expect("write FSInfo");
}

fn free_count<D: BlockDevice>(device: &D, info: BlockIdx) -> u32
where
    D::Error: core::fmt::Debug,
{
    let mut block = [Block::new()];
    device.read(&mut block, info).expect("read FSInfo");
    u32_at(&block[0], 488)
}

/// Truncating a file down to its first cluster frees every other cluster of
/// the chain, and the count goes up by exactly that many: the last cluster
/// of the chain is as free as the rest.
#[test]
fn truncation_counts_every_cluster_it_frees() {
    let disk = utils::make_block_device(utils::DISK_SOURCE).expect("disk image");
    let (info, cluster_bytes) = fsinfo_and_cluster_bytes(&disk);
    set_free_count(&disk, info, 1000);
    let manager: VolumeManager<_, _, 4, 4, 1> =
        VolumeManager::new_with_limits(disk, utils::make_time_source(), 0xAA00_0000);
    {
        let volume = manager
            .open_volume(VolumeIdx(1))
            .expect("open FAT32 volume");
        let root = volume.open_root_dir().expect("open root");
        let file = root
            .open_file_in_dir("THREE.DAT", Mode::ReadWriteCreate)
            .expect("create");
        file.write(&vec![7u8; cluster_bytes * 3]).expect("write");
        file.close().expect("close");
        let file = root
            .open_file_in_dir("THREE.DAT", Mode::ReadWriteTruncate)
            .expect("truncate");
        file.close().expect("close");
        root.close().expect("close root");
        volume.close().expect("close volume");
    }
    let after = manager.device(|disk| free_count(disk, info));
    assert_eq!(
        after,
        1000 - 3 + 2,
        "three allocated, then the two past the first freed"
    );
}

/// A count that reads zero while clusters are free is pessimistic, not
/// wrong, and an allocation the FAT scan can satisfy does not take it below
/// zero.
#[test]
fn allocating_under_a_count_of_zero_leaves_it_at_zero() {
    let disk = utils::make_block_device(utils::DISK_SOURCE).expect("disk image");
    let (info, _) = fsinfo_and_cluster_bytes(&disk);
    set_free_count(&disk, info, 0);
    let manager: VolumeManager<_, _, 4, 4, 1> =
        VolumeManager::new_with_limits(disk, utils::make_time_source(), 0xAA00_0000);
    {
        let volume = manager
            .open_volume(VolumeIdx(1))
            .expect("open FAT32 volume");
        let root = volume.open_root_dir().expect("open root");
        let file = root
            .open_file_in_dir("ONE.DAT", Mode::ReadWriteCreate)
            .expect("create");
        file.write(b"a cluster the scan finds").expect("write");
        file.close().expect("close");
        root.close().expect("close root");
        volume.close().expect("close volume");
    }
    assert_eq!(manager.device(|disk| free_count(disk, info)), 0);
}
