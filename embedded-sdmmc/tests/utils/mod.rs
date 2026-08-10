// `mod utils` is compiled into each test binary separately, so anything only
// some of them use looks unused to the rest.
#![allow(dead_code)]

//! Useful library code for tests

use std::io::prelude::*;

use embedded_sdmmc::{Block, BlockCount, BlockDevice, BlockIdx};

/// This file contains:
///
/// ```console
/// $ fdisk ./disk.img
/// Disk: ./disk.img    geometry: 520/32/63 [1048576 sectors]
/// Signature: 0xAA55
///          Starting       Ending
///  #: id  cyl  hd sec -  cyl  hd sec [     start -       size]
/// ------------------------------------------------------------------------
///  1: 0E    0  32  33 -   16 113  33 [      2048 -     262144] DOS FAT-16
///  2: 0C   16 113  34 -   65  69   4 [    264192 -     784384] Win95 FAT32L
///  3: 00    0   0   0 -    0   0   0 [         0 -          0] unused
///  4: 00    0   0   0 -    0   0   0 [         0 -          0] unused
/// $ ls -l /Volumes/P-FAT16
/// total 131080
/// -rwxrwxrwx  1 jonathan  staff  67108864  9 Dec  2018 64MB.DAT
/// -rwxrwxrwx  1 jonathan  staff         0  9 Dec  2018 EMPTY.DAT
/// -rwxrwxrwx@ 1 jonathan  staff       258  9 Dec  2018 README.TXT
/// drwxrwxrwx  1 jonathan  staff      2048  9 Dec  2018 TEST
/// $ ls -l /Volumes/P-FAT16/TEST
/// total 8
/// -rwxrwxrwx  1 jonathan  staff  3500  9 Dec  2018 TEST.DAT
/// $ ls -l /Volumes/P-FAT32
/// total 131088
/// -rwxrwxrwx  1 jonathan  staff  67108864  9 Dec  2018 64MB.DAT
/// -rwxrwxrwx  1 jonathan  staff         0  9 Dec  2018 EMPTY.DAT
/// -rwxrwxrwx@ 1 jonathan  staff       258 21 Sep 09:48 README.TXT
/// drwxrwxrwx  1 jonathan  staff      4096  9 Dec  2018 TEST
/// $ ls -l /Volumes/P-FAT32/TEST
/// total 8
/// -rwxrwxrwx  1 jonathan  staff  3500  9 Dec  2018 TEST.DAT
/// ```
///
/// It will unpack to a Vec that is 1048576 * 512 = 512 MiB in size.
pub static DISK_SOURCE: &[u8] = include_bytes!("../disk.img.gz");

#[derive(Debug, thiserror::Error)]
#[allow(dead_code)]
pub enum Error {
    /// Failed to read the source image
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// Failed to unzip the source image
    #[error("flate2 decode error: {0}")]
    Decode(#[from] flate2::DecompressError),
    /// Asked for a block we don't have
    #[error("out of bounds block index: {0:?}")]
    OutOfBounds(BlockIdx),
}

/// Implements the block device traits for a chunk of bytes in RAM.
///
/// The slice should be a multiple of `embedded_sdmmc::Block::LEN` bytes in
/// length. If it isn't the trailing data is discarded.
use std::cell::Cell;

/// Wraps a device and fails reads that land in a chosen block range.
///
/// `write_region` does the same for writes, which is the only way to reach
/// some sites: allocating a cluster reads the FAT to find a free one and then
/// writes it to claim it, and the read is usually served from the block cache
/// the preceding chain walk already filled.
pub struct FailRegion<D> {
    pub inner: D,
    pub region: Cell<Option<(u32, u32)>>,
    pub write_region: Cell<Option<(u32, u32)>>,
    pub injected: Cell<u32>,
    /// Counts writes landing in `write_region`. Used both to find which write
    /// is the interesting one and, via `fail_write_number`, to fail it.
    pub writes_seen: Cell<u32>,
    /// Fail every write to `write_region` from this number onward. Used to
    /// cut a multi-write operation off partway through, the way losing power
    /// would.
    pub fail_writes_from: Cell<Option<u32>>,
    /// Fail exactly one write to `write_region`: the one with this number.
    /// Everything before and after it goes through, which is what a single
    /// bad write looks like and what the cleanup path needs in order to run.
    pub fail_write_number: Cell<Option<u32>>,
}

#[derive(Debug)]
pub enum FailError {
    Injected,
    /// Carried so a real device failure is distinguishable in test output.
    Inner(#[allow(dead_code)] Error),
}

// `Error::DeviceError` requires the device's error type to implement
// `core::error::Error` since the thiserror migration in 0.10.
impl core::fmt::Display for FailError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{self:?}")
    }
}

impl core::error::Error for FailError {}

impl<D> BlockDevice for FailRegion<D>
where
    D: BlockDevice<Error = Error>,
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
        if let Some((from, to)) = self.write_region.get() {
            let last = start.0 + blocks.len() as u32;
            if start.0 < to && last > from {
                self.writes_seen.set(self.writes_seen.get() + 1);
                // `fail_writes_from`, when set, is the whole rule: everything
                // before the cut-off goes through so a multi-write operation
                // can be stopped partway rather than at its first step.
                let fail = match self.fail_writes_from.get() {
                    Some(from) => self.writes_seen.get() >= from,
                    None => match self.fail_write_number.get() {
                        None => true,
                        Some(n) => n == self.writes_seen.get(),
                    },
                };
                if fail {
                    self.injected.set(self.injected.get() + 1);
                    return Err(FailError::Injected);
                }
            }
        }
        self.inner.write(blocks, start).map_err(FailError::Inner)
    }

    fn num_blocks(&self) -> Result<BlockCount, Self::Error> {
        self.inner.num_blocks().map_err(FailError::Inner)
    }
}


impl<D> FailRegion<D> {
    /// A device that passes everything through until it is armed.
    pub fn new(inner: D) -> Self {
        FailRegion {
            inner,
            region: Cell::new(None),
            write_region: Cell::new(None),
            injected: Cell::new(0),
            writes_seen: Cell::new(0),
            fail_writes_from: Cell::new(None),
            fail_write_number: Cell::new(None),
        }
    }
}

pub struct RamDisk<T> {
    contents: std::cell::RefCell<T>,
}

impl<T> RamDisk<T> {
    fn new(contents: T) -> RamDisk<T> {
        RamDisk {
            contents: std::cell::RefCell::new(contents),
        }
    }
}

impl<T> BlockDevice for RamDisk<T>
where
    T: AsMut<[u8]> + AsRef<[u8]>,
{
    type Error = Error;

    fn read(&self, blocks: &mut [Block], start_block_idx: BlockIdx) -> Result<(), Self::Error> {
        let borrow = self.contents.borrow();
        let contents: &[u8] = borrow.as_ref();
        let mut block_idx = start_block_idx;
        for block in blocks.iter_mut() {
            let start_offset = block_idx.0 as usize * embedded_sdmmc::Block::LEN;
            let end_offset = start_offset + embedded_sdmmc::Block::LEN;
            if end_offset > contents.len() {
                return Err(Error::OutOfBounds(block_idx));
            }
            block
                .as_mut_slice()
                .copy_from_slice(&contents[start_offset..end_offset]);
            block_idx.0 += 1;
        }
        Ok(())
    }

    fn write(&self, blocks: &[Block], start_block_idx: BlockIdx) -> Result<(), Self::Error> {
        let mut borrow = self.contents.borrow_mut();
        let contents: &mut [u8] = borrow.as_mut();
        let mut block_idx = start_block_idx;
        for block in blocks.iter() {
            let start_offset = block_idx.0 as usize * embedded_sdmmc::Block::LEN;
            let end_offset = start_offset + embedded_sdmmc::Block::LEN;
            if end_offset > contents.len() {
                return Err(Error::OutOfBounds(block_idx));
            }
            contents[start_offset..end_offset].copy_from_slice(block.as_slice());
            block_idx.0 += 1;
        }
        Ok(())
    }

    fn num_blocks(&self) -> Result<BlockCount, Self::Error> {
        let borrow = self.contents.borrow();
        let contents: &[u8] = borrow.as_ref();
        let len_blocks = contents.len() / embedded_sdmmc::Block::LEN;
        if len_blocks > u32::MAX as usize {
            panic!("Test disk too large! Only 2**32 blocks allowed");
        }
        Ok(BlockCount(len_blocks as u32))
    }
}

/// Unpack the fixed, static, disk image.
fn unpack_disk(gzip_bytes: &[u8]) -> Result<Vec<u8>, Error> {
    let disk_cursor = std::io::Cursor::new(gzip_bytes);
    let mut gz_decoder = flate2::read::GzDecoder::new(disk_cursor);
    let mut output = Vec::with_capacity(512 * 1024 * 1024);
    gz_decoder.read_to_end(&mut output)?;
    Ok(output)
}

/// Turn some gzipped bytes into a block device,
pub fn make_block_device(gzip_bytes: &[u8]) -> Result<RamDisk<Vec<u8>>, Error> {
    let data = unpack_disk(gzip_bytes)?;
    Ok(RamDisk::new(data))
}

pub struct TestTimeSource {
    fixed: embedded_sdmmc::Timestamp,
}

impl embedded_sdmmc::TimeSource for TestTimeSource {
    fn get_timestamp(&self) -> embedded_sdmmc::Timestamp {
        self.fixed
    }
}

/// Make a time source that gives a fixed time.
///
/// It always claims to be 4 April 2003, at 13:30:05.
///
/// This is an interesting time, because FAT will round it down to 13:30:04 due
/// to only have two-second resolution. Hey, Real Time Clocks were optional back
/// in 1981.
pub fn make_time_source() -> TestTimeSource {
    TestTimeSource {
        fixed: embedded_sdmmc::Timestamp {
            year_since_1970: 33,
            zero_indexed_month: 3,
            zero_indexed_day: 3,
            hours: 13,
            minutes: 30,
            seconds: 5,
        },
    }
}

/// Get the test time source time, as a string.
///
/// We apply the FAT 2-second rounding here.
#[allow(unused)]
pub fn get_time_source_string() -> &'static str {
    "2003-04-04 13:30:04"
}

// ****************************************************************************
//
// End Of File
//
// ****************************************************************************
