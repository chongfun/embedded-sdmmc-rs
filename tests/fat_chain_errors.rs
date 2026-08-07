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

use embedded_sdmmc::{Block, BlockCount, BlockDevice, BlockIdx, Mode, VolumeIdx, VolumeManager};
use std::cell::Cell;

mod utils;

/// Wraps a device and fails reads that land in a chosen block range.
struct FailRegion<D> {
    inner: D,
    region: Cell<Option<(u32, u32)>>,
    injected: Cell<u32>,
}

#[derive(Debug)]
enum FailError {
    Injected,
    /// Carried so a real device failure is distinguishable in test output.
    Inner(#[allow(dead_code)] utils::Error),
}

impl<D> BlockDevice for FailRegion<D>
where
    D: BlockDevice<Error = utils::Error>,
{
    type Error = FailError;

    fn read(&self, blocks: &mut [Block], start: BlockIdx) -> Result<(), Self::Error> {
        if let Some((from, to)) = self.region.get() {
            let last = start.0 + blocks.len() as u32;
            if start.0 < to && last > from {
                self.injected.set(self.injected.get() + 1);
                return Err(FailError::Injected);
            }
        }
        self.inner.read(blocks, start).map_err(FailError::Inner)
    }

    fn write(&self, blocks: &[Block], start: BlockIdx) -> Result<(), Self::Error> {
        self.inner.write(blocks, start).map_err(FailError::Inner)
    }

    fn num_blocks(&self) -> Result<BlockCount, Self::Error> {
        self.inner.num_blocks().map_err(FailError::Inner)
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

/// The blocks holding the FAT itself, read out of the partition's BPB.
fn fat_region<D: BlockDevice>(device: &D) -> (u32, u32) {
    let mut block = [Block::new()];
    device.read(&mut block, BlockIdx(0)).ok();
    let partition_start = u32_at(&block[0], 446 + 8);

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
    let (fat_start, fat_end) = fat_region(&inner);
    let device = FailRegion {
        inner,
        region: Cell::new(None),
        injected: Cell::new(0),
    };

    let manager: VolumeManager<_, _, 4, 4, 1> =
        VolumeManager::new_with_limits(device, utils::make_time_source(), 0xAA);
    let volume = manager.open_volume(VolumeIdx(0)).expect("open volume");
    let root = volume.open_root_dir().expect("open root");
    let directory = root.open_dir("TEST").expect("open TEST");

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
