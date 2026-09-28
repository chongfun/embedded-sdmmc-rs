//! Reading related tests

use sha2::Digest;

mod utils;

static TEST_DAT_SHA256_SUM: &[u8] =
    b"\x59\xe3\x46\x8e\x3b\xef\x8b\xfe\x37\xe6\x0a\x82\x21\xa1\x89\x6e\x10\x5b\x80\xa6\x1a\x23\x63\x76\x12\xac\x8c\xd2\x4c\xa0\x4a\x75";

#[test]
fn read_file_512_blocks() {
    let time_source = utils::make_time_source();
    let disk = utils::make_block_device(utils::DISK_SOURCE).unwrap();
    let volume_mgr = embedded_sdmmc::VolumeManager::new(disk, time_source);

    let fat16_volume = volume_mgr
        .open_raw_volume(embedded_sdmmc::VolumeIdx(0))
        .expect("open volume 0");
    let root_dir = volume_mgr
        .open_root_dir(fat16_volume)
        .expect("open root dir");
    let test_dir = volume_mgr
        .open_dir(root_dir, "TEST")
        .expect("Open test dir");

    let test_file = volume_mgr
        .open_file_in_dir(test_dir, "TEST.DAT", embedded_sdmmc::Mode::ReadOnly)
        .expect("open test file");

    let mut contents = Vec::new();

    let mut partial = false;
    while !volume_mgr.file_eof(test_file).expect("check eof") {
        let mut buffer = [0u8; 512];
        let len = volume_mgr.read(test_file, &mut buffer).expect("read data");
        if len != buffer.len() {
            if partial {
                panic!("Two partial reads!");
            } else {
                partial = true;
            }
        }
        contents.extend(&buffer[0..len]);
    }

    let mut hasher = sha2::Sha256::new();
    hasher.update(contents);
    let hash = hasher.finalize();
    assert_eq!(&hash[..], TEST_DAT_SHA256_SUM);
}

#[test]
fn read_file_all() {
    let time_source = utils::make_time_source();
    let disk = utils::make_block_device(utils::DISK_SOURCE).unwrap();
    let volume_mgr = embedded_sdmmc::VolumeManager::new(disk, time_source);

    let fat16_volume = volume_mgr
        .open_raw_volume(embedded_sdmmc::VolumeIdx(0))
        .expect("open volume 0");
    let root_dir = volume_mgr
        .open_root_dir(fat16_volume)
        .expect("open root dir");
    let test_dir = volume_mgr
        .open_dir(root_dir, "TEST")
        .expect("Open test dir");

    let test_file = volume_mgr
        .open_file_in_dir(test_dir, "TEST.DAT", embedded_sdmmc::Mode::ReadOnly)
        .expect("open test file");

    let mut contents = vec![0u8; 4096];
    let len = volume_mgr
        .read(test_file, &mut contents)
        .expect("read data");
    if len != 3500 {
        panic!("Failed to read all of TEST.DAT");
    }

    let mut hasher = sha2::Sha256::new();
    hasher.update(&contents[0..3500]);
    let hash = hasher.finalize();
    assert_eq!(&hash[..], TEST_DAT_SHA256_SUM);
}

#[test]
fn read_file_prime_blocks() {
    let time_source = utils::make_time_source();
    let disk = utils::make_block_device(utils::DISK_SOURCE).unwrap();
    let volume_mgr = embedded_sdmmc::VolumeManager::new(disk, time_source);

    let fat16_volume = volume_mgr
        .open_raw_volume(embedded_sdmmc::VolumeIdx(0))
        .expect("open volume 0");
    let root_dir = volume_mgr
        .open_root_dir(fat16_volume)
        .expect("open root dir");
    let test_dir = volume_mgr
        .open_dir(root_dir, "TEST")
        .expect("Open test dir");

    let test_file = volume_mgr
        .open_file_in_dir(test_dir, "TEST.DAT", embedded_sdmmc::Mode::ReadOnly)
        .expect("open test file");

    let mut contents = Vec::new();

    let mut partial = false;
    while !volume_mgr.file_eof(test_file).expect("check eof") {
        // Exercise the alignment code by reading in chunks of 53 bytes
        let mut buffer = [0u8; 53];
        let len = volume_mgr.read(test_file, &mut buffer).expect("read data");
        if len != buffer.len() {
            if partial {
                panic!("Two partial reads!");
            } else {
                partial = true;
            }
        }
        contents.extend(&buffer[0..len]);
    }

    let mut hasher = sha2::Sha256::new();
    hasher.update(&contents[0..3500]);
    let hash = hasher.finalize();
    assert_eq!(&hash[..], TEST_DAT_SHA256_SUM);
}

#[test]
fn read_file_backwards() {
    let time_source = utils::make_time_source();
    let disk = utils::make_block_device(utils::DISK_SOURCE).unwrap();
    let volume_mgr = embedded_sdmmc::VolumeManager::new(disk, time_source);

    let fat16_volume = volume_mgr
        .open_raw_volume(embedded_sdmmc::VolumeIdx(0))
        .expect("open volume 0");
    let root_dir = volume_mgr
        .open_root_dir(fat16_volume)
        .expect("open root dir");
    let test_dir = volume_mgr
        .open_dir(root_dir, "TEST")
        .expect("Open test dir");

    let test_file = volume_mgr
        .open_file_in_dir(test_dir, "TEST.DAT", embedded_sdmmc::Mode::ReadOnly)
        .expect("open test file");

    let mut contents = std::collections::VecDeque::new();

    const CHUNK_SIZE: u32 = 100;
    let length = volume_mgr.file_length(test_file).expect("file length");
    let mut read = 0;

    // go to end
    volume_mgr.file_seek_from_end(test_file, 0).expect("seek");

    // We're going to read the file backwards in chunks of 100 bytes. This
    // checks we didn't make any assumptions about only going forwards.
    while read < length {
        // go to start of next chunk
        volume_mgr
            .file_seek_from_current(test_file, -(CHUNK_SIZE as i32))
            .expect("seek");
        // read chunk
        let mut buffer = [0u8; CHUNK_SIZE as usize];
        let len = volume_mgr.read(test_file, &mut buffer).expect("read");
        assert_eq!(len, CHUNK_SIZE as usize);
        contents.push_front(buffer.to_vec());
        read += CHUNK_SIZE;
        // go to start of chunk we just read
        volume_mgr
            .file_seek_from_current(test_file, -(CHUNK_SIZE as i32))
            .expect("seek");
    }

    assert_eq!(read, length);

    let flat: Vec<u8> = contents.iter().flatten().copied().collect();

    let mut hasher = sha2::Sha256::new();
    hasher.update(flat);
    let hash = hasher.finalize();
    assert_eq!(&hash[..], TEST_DAT_SHA256_SUM);
}

#[test]
fn read_file_with_odd_seek() {
    let time_source = utils::make_time_source();
    let disk = utils::make_block_device(utils::DISK_SOURCE).unwrap();
    let volume_mgr = embedded_sdmmc::VolumeManager::new(disk, time_source);

    let volume = volume_mgr
        .open_volume(embedded_sdmmc::VolumeIdx(0))
        .unwrap();
    let root_dir = volume.open_root_dir().unwrap();
    let f = root_dir
        .open_file_in_dir("64MB.DAT", embedded_sdmmc::Mode::ReadOnly)
        .unwrap();
    f.seek_from_start(0x2c).unwrap();
    while f.offset() < 1000000 {
        let mut buffer = [0u8; 2048];
        f.read(&mut buffer).unwrap();
        f.seek_from_current(-1024).unwrap();
    }
}

// ****************************************************************************
//
// End Of File
//
// ****************************************************************************

/// Counts how many device reads asked for more than one block, so a test can
/// tell a multi-block read from the same bytes fetched one block at a time.
struct CountingDisk<D> {
    inner: D,
    multi_block_reads: std::cell::Cell<usize>,
}

impl<D: embedded_sdmmc::BlockDevice> embedded_sdmmc::BlockDevice for CountingDisk<D> {
    type Error = D::Error;

    fn read(
        &self,
        blocks: &mut [embedded_sdmmc::Block],
        start_block_idx: embedded_sdmmc::BlockIdx,
    ) -> Result<(), Self::Error> {
        if blocks.len() > 1 {
            self.multi_block_reads.set(self.multi_block_reads.get() + 1);
        }
        self.inner.read(blocks, start_block_idx)
    }

    fn write(
        &self,
        blocks: &[embedded_sdmmc::Block],
        start_block_idx: embedded_sdmmc::BlockIdx,
    ) -> Result<(), Self::Error> {
        self.inner.write(blocks, start_block_idx)
    }

    fn num_blocks(&self) -> Result<embedded_sdmmc::BlockCount, Self::Error> {
        self.inner.num_blocks()
    }
}

/// Read a whole file with `read_blocks` into a buffer of `per_call` blocks,
/// keeping only the bytes each call says belong to the file.
fn read_all_blocks<D, T>(
    volume_mgr: &embedded_sdmmc::VolumeManager<D, T, 4, 4, 1>,
    file: embedded_sdmmc::RawFile,
    per_call: usize,
) -> Vec<u8>
where
    D: embedded_sdmmc::BlockDevice,
    D::Error: core::fmt::Debug,
    T: embedded_sdmmc::TimeSource,
{
    let mut contents = Vec::new();
    let mut blocks = vec![embedded_sdmmc::Block::new(); per_call];
    loop {
        let len = volume_mgr
            .read_blocks(file, &mut blocks)
            .expect("read blocks");
        let flat: Vec<u8> = blocks.iter().flat_map(|b| b.contents).collect();
        contents.extend(&flat[..len]);
        if len < per_call * 512 {
            break;
        }
    }
    contents
}

#[test]
fn read_file_whole_blocks_matches_the_file_at_every_run_length() {
    for per_call in [1usize, 2, 3, 7, 64] {
        let time_source = utils::make_time_source();
        let disk = utils::make_block_device(utils::DISK_SOURCE).unwrap();
        let volume_mgr: embedded_sdmmc::VolumeManager<_, _, 4, 4, 1> =
            embedded_sdmmc::VolumeManager::new_with_limits(disk, time_source, 0xAA00_0000);
        let volume = volume_mgr
            .open_raw_volume(embedded_sdmmc::VolumeIdx(0))
            .expect("open volume 0");
        let root_dir = volume_mgr.open_root_dir(volume).expect("open root dir");
        let test_dir = volume_mgr
            .open_dir(root_dir, "TEST")
            .expect("open test dir");
        let file = volume_mgr
            .open_file_in_dir(test_dir, "TEST.DAT", embedded_sdmmc::Mode::ReadOnly)
            .expect("open test file");
        let contents = read_all_blocks(&volume_mgr, file, per_call);
        assert_eq!(contents.len(), 3500, "{per_call} blocks a call");
        let mut hasher = sha2::Sha256::new();
        hasher.update(&contents);
        assert_eq!(
            &hasher.finalize()[..],
            TEST_DAT_SHA256_SUM,
            "{per_call} blocks a call"
        );
    }
}

/// A file over several clusters, written here so its length is known: every
/// run stops at a cluster boundary and the next resumes from the chain, and
/// the device is asked for more than one block at a time.
#[test]
fn read_file_whole_blocks_across_clusters_in_multi_block_reads() {
    let time_source = utils::make_time_source();
    let disk = CountingDisk {
        inner: utils::make_block_device(utils::DISK_SOURCE).unwrap(),
        multi_block_reads: std::cell::Cell::new(0),
    };
    let volume_mgr: embedded_sdmmc::VolumeManager<_, _, 4, 4, 1> =
        embedded_sdmmc::VolumeManager::new_with_limits(disk, time_source, 0xAA00_0000);
    let volume = volume_mgr
        .open_raw_volume(embedded_sdmmc::VolumeIdx(0))
        .expect("open volume 0");
    let root_dir = volume_mgr.open_root_dir(volume).expect("open root dir");
    let body: Vec<u8> = (0..70_001u32).map(|n| (n % 251) as u8).collect();
    let file = volume_mgr
        .open_file_in_dir(root_dir, "LONG.DAT", embedded_sdmmc::Mode::ReadWriteCreate)
        .expect("create");
    volume_mgr.write(file, &body).expect("write");
    volume_mgr.close_file(file).expect("close");

    let file = volume_mgr
        .open_file_in_dir(root_dir, "LONG.DAT", embedded_sdmmc::Mode::ReadOnly)
        .expect("open");
    let before = volume_mgr.device(|disk| disk.multi_block_reads.get());
    let contents = read_all_blocks(&volume_mgr, file, 16);
    assert_eq!(contents, body);
    assert!(
        volume_mgr.device(|disk| disk.multi_block_reads.get()) > before,
        "the runs reach the device as multi-block reads"
    );
}

#[test]
fn read_file_whole_blocks_refuses_a_position_inside_a_block() {
    let time_source = utils::make_time_source();
    let disk = utils::make_block_device(utils::DISK_SOURCE).unwrap();
    let volume_mgr: embedded_sdmmc::VolumeManager<_, _, 4, 4, 1> =
        embedded_sdmmc::VolumeManager::new_with_limits(disk, time_source, 0xAA00_0000);
    let volume = volume_mgr
        .open_raw_volume(embedded_sdmmc::VolumeIdx(0))
        .expect("open volume 0");
    let root_dir = volume_mgr.open_root_dir(volume).expect("open root dir");
    let test_dir = volume_mgr
        .open_dir(root_dir, "TEST")
        .expect("open test dir");
    let file = volume_mgr
        .open_file_in_dir(test_dir, "TEST.DAT", embedded_sdmmc::Mode::ReadOnly)
        .expect("open test file");
    let mut head = [0u8; 100];
    volume_mgr.read(file, &mut head).expect("read");
    let mut blocks = [embedded_sdmmc::Block::new(), embedded_sdmmc::Block::new()];
    assert!(matches!(
        volume_mgr.read_blocks(file, &mut blocks),
        Err(embedded_sdmmc::Error::InvalidOffset)
    ));
    // Nothing moved, and back on a boundary the rest reads as the file.
    volume_mgr.file_seek_from_start(file, 512).expect("seek");
    let len = volume_mgr
        .read_blocks(file, &mut blocks)
        .expect("read blocks");
    assert_eq!(len, 1024);
    let mut expect = [0u8; 1024];
    volume_mgr.file_seek_from_start(file, 512).expect("seek");
    volume_mgr.read(file, &mut expect).expect("read");
    let flat: Vec<u8> = blocks.iter().flat_map(|b| b.contents).collect();
    assert_eq!(&flat[..], &expect[..]);
}
