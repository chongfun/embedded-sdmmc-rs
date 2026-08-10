//! Directory related tests

use std::ops::ControlFlow;

use embedded_sdmmc::{LfnBuffer, Mode, ShortFileName};

mod utils;

#[derive(Debug, Clone)]
struct ExpectedDirEntry {
    name: String,
    mtime: String,
    ctime: String,
    size: u32,
    is_dir: bool,
}

impl PartialEq<embedded_sdmmc::DirEntry> for ExpectedDirEntry {
    fn eq(&self, other: &embedded_sdmmc::DirEntry) -> bool {
        if other.name.to_string() != self.name {
            return false;
        }
        if format!("{}", other.mtime) != self.mtime {
            return false;
        }
        if format!("{}", other.ctime) != self.ctime {
            return false;
        }
        if other.size != self.size {
            return false;
        }
        if other.attributes.is_directory() != self.is_dir {
            return false;
        }
        true
    }
}

#[test]
fn fat16_root_directory_listing() {
    let time_source = utils::make_time_source();
    let disk = utils::make_block_device(utils::DISK_SOURCE).unwrap();
    let volume_mgr = embedded_sdmmc::VolumeManager::new(disk, time_source);

    let fat16_volume = volume_mgr
        .open_raw_volume(embedded_sdmmc::VolumeIdx(0))
        .expect("open volume 0");
    let root_dir = volume_mgr
        .open_root_dir(fat16_volume)
        .expect("open root dir");

    let expected = [
        (
            ExpectedDirEntry {
                name: String::from("README.TXT"),
                mtime: String::from("2018-12-09 19:22:34"),
                ctime: String::from("2018-12-09 19:22:34"),
                size: 258,
                is_dir: false,
            },
            None,
        ),
        (
            ExpectedDirEntry {
                name: String::from("EMPTY.DAT"),
                mtime: String::from("2018-12-09 19:21:16"),
                ctime: String::from("2018-12-09 19:21:16"),
                size: 0,
                is_dir: false,
            },
            None,
        ),
        (
            ExpectedDirEntry {
                name: String::from("TEST"),
                mtime: String::from("2018-12-09 19:23:16"),
                ctime: String::from("2018-12-09 19:23:16"),
                size: 0,
                is_dir: true,
            },
            None,
        ),
        (
            ExpectedDirEntry {
                name: String::from("64MB.DAT"),
                mtime: String::from("2018-12-09 19:21:38"),
                ctime: String::from("2018-12-09 19:21:38"),
                size: 64 * 1024 * 1024,
                is_dir: false,
            },
            None,
        ),
        (
            ExpectedDirEntry {
                name: String::from("FSEVEN~4"),
                mtime: String::from("2024-10-25 16:30:42"),
                ctime: String::from("2024-10-25 16:30:42"),
                size: 0,
                is_dir: true,
            },
            Some(String::from(".fseventsd")),
        ),
        (
            ExpectedDirEntry {
                name: String::from("P-FAT16"),
                mtime: String::from("2024-10-30 18:43:12"),
                ctime: String::from("2024-10-30 18:43:12"),
                size: 0,
                is_dir: false,
            },
            None,
        ),
    ];

    let mut listing = Vec::new();
    let mut storage = [0u8; 128];
    let mut lfn_buffer: LfnBuffer = LfnBuffer::new(&mut storage);

    volume_mgr
        .iterate_dir_lfn(root_dir, &mut lfn_buffer, |d, opt_lfn| {
            listing.push((d.clone(), opt_lfn.map(String::from)));
            ControlFlow::Continue(())
        })
        .expect("iterate directory");

    for (expected_entry, given_entry) in expected.iter().zip(listing.iter()) {
        assert_eq!(
            expected_entry.0, given_entry.0,
            "{:#?} does not match {:#?}",
            given_entry, expected_entry
        );
        assert_eq!(
            expected_entry.1, given_entry.1,
            "{:#?} does not match {:#?}",
            given_entry, expected_entry
        );
    }
    assert_eq!(
        expected.len(),
        listing.len(),
        "{:#?} != {:#?}",
        expected,
        listing
    );
}

#[test]
fn fat16_sub_directory_listing() {
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
        .expect("open test dir");

    let expected = [
        ExpectedDirEntry {
            name: String::from("."),
            mtime: String::from("2018-12-09 19:21:02"),
            ctime: String::from("2018-12-09 19:21:02"),
            size: 0,
            is_dir: true,
        },
        ExpectedDirEntry {
            name: String::from(".."),
            mtime: String::from("2018-12-09 19:21:02"),
            ctime: String::from("2018-12-09 19:21:02"),
            size: 0,
            is_dir: true,
        },
        ExpectedDirEntry {
            name: String::from("TEST.DAT"),
            mtime: String::from("2018-12-09 19:22:12"),
            ctime: String::from("2018-12-09 19:22:12"),
            size: 3500,
            is_dir: false,
        },
    ];

    let mut listing = Vec::new();
    let mut count = 0;

    volume_mgr
        .iterate_dir(test_dir, |d| {
            if count == 0 {
                assert!(d.name == ShortFileName::this_dir());
            } else if count == 1 {
                assert!(d.name == ShortFileName::parent_dir());
            }
            count += 1;
            listing.push(d.clone());
            ControlFlow::Continue(())
        })
        .expect("iterate directory");

    for (expected_entry, given_entry) in expected.iter().zip(listing.iter()) {
        assert_eq!(
            expected_entry, given_entry,
            "{:#?} does not match {:#?}",
            given_entry, expected_entry
        );
    }
    assert_eq!(
        expected.len(),
        listing.len(),
        "{:#?} != {:#?}",
        expected,
        listing
    );
}

#[test]
fn fat32_root_directory_listing() {
    let time_source = utils::make_time_source();
    let disk = utils::make_block_device(utils::DISK_SOURCE).unwrap();
    let volume_mgr = embedded_sdmmc::VolumeManager::new(disk, time_source);

    let fat32_volume = volume_mgr
        .open_raw_volume(embedded_sdmmc::VolumeIdx(1))
        .expect("open volume 1");
    let root_dir = volume_mgr
        .open_root_dir(fat32_volume)
        .expect("open root dir");

    let expected = [
        (
            ExpectedDirEntry {
                name: String::from("64MB.DAT"),
                mtime: String::from("2018-12-09 19:22:56"),
                ctime: String::from("2018-12-09 19:22:56"),
                size: 64 * 1024 * 1024,
                is_dir: false,
            },
            None,
        ),
        (
            ExpectedDirEntry {
                name: String::from("EMPTY.DAT"),
                mtime: String::from("2018-12-09 19:22:56"),
                ctime: String::from("2018-12-09 19:22:56"),
                size: 0,
                is_dir: false,
            },
            None,
        ),
        (
            ExpectedDirEntry {
                name: String::from("README.TXT"),
                mtime: String::from("2023-09-21 09:48:06"),
                ctime: String::from("2018-12-09 19:22:56"),
                size: 258,
                is_dir: false,
            },
            None,
        ),
        (
            ExpectedDirEntry {
                name: String::from("TEST"),
                mtime: String::from("2018-12-09 19:23:20"),
                ctime: String::from("2018-12-09 19:23:20"),
                size: 0,
                is_dir: true,
            },
            None,
        ),
        (
            ExpectedDirEntry {
                name: String::from("FSEVEN~4"),
                mtime: String::from("2024-10-25 16:30:42"),
                ctime: String::from("2024-10-25 16:30:42"),
                size: 0,
                is_dir: true,
            },
            Some(String::from(".fseventsd")),
        ),
        (
            ExpectedDirEntry {
                name: String::from("P-FAT32"),
                mtime: String::from("2024-10-30 18:43:16"),
                ctime: String::from("2024-10-30 18:43:16"),
                size: 0,
                is_dir: false,
            },
            None,
        ),
        (
            ExpectedDirEntry {
                name: String::from("THISIS~9"),
                mtime: String::from("2024-10-25 16:30:54"),
                ctime: String::from("2024-10-25 16:30:50"),
                size: 0,
                is_dir: true,
            },
            Some(String::from("This is a long file name £99")),
        ),
        (
            ExpectedDirEntry {
                name: String::from("COPYO~13.TXT"),
                mtime: String::from("2024-10-25 16:31:14"),
                ctime: String::from("2018-12-09 19:22:56"),
                size: 258,
                is_dir: false,
            },
            Some(String::from("Copy of Readme.txt")),
        ),
    ];

    let mut listing = Vec::new();
    let mut storage = [0u8; 128];
    let mut lfn_buffer: LfnBuffer = LfnBuffer::new(&mut storage);

    volume_mgr
        .iterate_dir_lfn(root_dir, &mut lfn_buffer, |d, opt_lfn| {
            listing.push((d.clone(), opt_lfn.map(String::from)));
            ControlFlow::Continue(())
        })
        .expect("iterate directory");

    for (expected_entry, given_entry) in expected.iter().zip(listing.iter()) {
        assert_eq!(
            expected_entry.0, given_entry.0,
            "{:#?} does not match {:#?}",
            given_entry, expected_entry
        );
        assert_eq!(
            expected_entry.1, given_entry.1,
            "{:#?} does not match {:#?}",
            given_entry, expected_entry
        );
    }
    assert_eq!(
        expected.len(),
        listing.len(),
        "{:#?} != {:#?}",
        expected,
        listing
    );
}

#[test]
fn open_dir_twice() {
    let time_source = utils::make_time_source();
    let disk = utils::make_block_device(utils::DISK_SOURCE).unwrap();
    let volume_mgr = embedded_sdmmc::VolumeManager::new(disk, time_source);

    let fat32_volume = volume_mgr
        .open_raw_volume(embedded_sdmmc::VolumeIdx(1))
        .expect("open volume 1");

    let root_dir = volume_mgr
        .open_root_dir(fat32_volume)
        .expect("open root dir");

    let root_dir2 = volume_mgr
        .open_root_dir(fat32_volume)
        .expect("open it again");

    assert!(matches!(
        volume_mgr.open_dir(root_dir, "README.TXT"),
        Err(embedded_sdmmc::Error::OpenedFileAsDir)
    ));

    let test_dir = volume_mgr
        .open_dir(root_dir, "TEST")
        .expect("open test dir");

    let test_dir2 = volume_mgr.open_dir(root_dir, "TEST").unwrap();

    volume_mgr.close_dir(root_dir).expect("close root dir");
    volume_mgr.close_dir(test_dir).expect("close test dir");
    volume_mgr.close_dir(test_dir2).expect("close test dir");
    volume_mgr.close_dir(root_dir2).expect("close test dir");

    assert!(matches!(
        volume_mgr.close_dir(test_dir),
        Err(embedded_sdmmc::Error::BadHandle)
    ));
}

#[test]
fn open_too_many_dirs() {
    let time_source = utils::make_time_source();
    let disk = utils::make_block_device(utils::DISK_SOURCE).unwrap();
    let volume_mgr: embedded_sdmmc::VolumeManager<
        utils::RamDisk<Vec<u8>>,
        utils::TestTimeSource,
        1,
        4,
        2,
    > = embedded_sdmmc::VolumeManager::new_with_limits(disk, time_source, 0x1000_0000);

    let fat32_volume = volume_mgr
        .open_raw_volume(embedded_sdmmc::VolumeIdx(1))
        .expect("open volume 1");
    let root_dir = volume_mgr
        .open_root_dir(fat32_volume)
        .expect("open root dir");

    assert!(matches!(
        volume_mgr.open_dir(root_dir, "TEST"),
        Err(embedded_sdmmc::Error::TooManyOpenDirs)
    ));
}

#[test]
fn find_dir_entry() {
    let time_source = utils::make_time_source();
    let disk = utils::make_block_device(utils::DISK_SOURCE).unwrap();
    let volume_mgr = embedded_sdmmc::VolumeManager::new(disk, time_source);

    let fat32_volume = volume_mgr
        .open_raw_volume(embedded_sdmmc::VolumeIdx(1))
        .expect("open volume 1");

    let root_dir = volume_mgr
        .open_root_dir(fat32_volume)
        .expect("open root dir");

    let dir_entry = volume_mgr
        .find_directory_entry(root_dir, "README.TXT")
        .expect("Find directory entry");
    assert!(dir_entry.attributes.is_archive());
    assert!(!dir_entry.attributes.is_directory());
    assert!(!dir_entry.attributes.is_hidden());
    assert!(!dir_entry.attributes.is_lfn());
    assert!(!dir_entry.attributes.is_system());
    assert!(!dir_entry.attributes.is_volume());

    assert!(matches!(
        volume_mgr.find_directory_entry(root_dir, "README.TXS"),
        Err(embedded_sdmmc::Error::NotFound)
    ));
}

#[test]
fn delete_file() {
    let time_source = utils::make_time_source();
    let disk = utils::make_block_device(utils::DISK_SOURCE).unwrap();
    let volume_mgr = embedded_sdmmc::VolumeManager::new(disk, time_source);

    let fat32_volume = volume_mgr
        .open_raw_volume(embedded_sdmmc::VolumeIdx(1))
        .expect("open volume 1");

    let root_dir = volume_mgr
        .open_root_dir(fat32_volume)
        .expect("open root dir");

    let file = volume_mgr
        .open_file_in_dir(root_dir, "README.TXT", Mode::ReadOnly)
        .unwrap();

    assert!(matches!(
        volume_mgr.delete_entry_in_dir(root_dir, "README.TXT"),
        Err(embedded_sdmmc::Error::FileAlreadyOpen)
    ));

    assert!(matches!(
        volume_mgr.delete_entry_in_dir(root_dir, "README2.TXT"),
        Err(embedded_sdmmc::Error::NotFound)
    ));

    volume_mgr.close_file(file).unwrap();

    volume_mgr
        .delete_entry_in_dir(root_dir, "README.TXT")
        .unwrap();

    assert!(matches!(
        volume_mgr.delete_entry_in_dir(root_dir, "README.TXT"),
        Err(embedded_sdmmc::Error::NotFound)
    ));

    assert!(matches!(
        volume_mgr.open_file_in_dir(root_dir, "README.TXT", Mode::ReadOnly),
        Err(embedded_sdmmc::Error::NotFound)
    ));
}

#[test]
fn make_directory() {
    let time_source = utils::make_time_source();
    let disk = utils::make_block_device(utils::DISK_SOURCE).unwrap();
    let volume_mgr = embedded_sdmmc::VolumeManager::new(disk, time_source);

    let fat32_volume = volume_mgr
        .open_raw_volume(embedded_sdmmc::VolumeIdx(1))
        .expect("open volume 1");

    let root_dir = volume_mgr
        .open_root_dir(fat32_volume)
        .expect("open root dir");

    let test_dir_name = ShortFileName::create_from_str("12345678.ABC").unwrap();
    let test_file_name = ShortFileName::create_from_str("ABC.TXT").unwrap();

    volume_mgr
        .make_dir_in_dir(root_dir, &test_dir_name)
        .unwrap();

    let new_dir = volume_mgr.open_dir(root_dir, &test_dir_name).unwrap();

    let mut has_this = false;
    let mut has_parent = false;
    volume_mgr
        .iterate_dir(new_dir, |item| {
            if item.name == ShortFileName::parent_dir() {
                has_parent = true;
                assert!(item.attributes.is_directory());
                assert_eq!(item.size, 0);
                assert_eq!(item.mtime.to_string(), utils::get_time_source_string());
                assert_eq!(item.ctime.to_string(), utils::get_time_source_string());
            } else if item.name == ShortFileName::this_dir() {
                has_this = true;
                assert!(item.attributes.is_directory());
                assert_eq!(item.size, 0);
                assert_eq!(item.mtime.to_string(), utils::get_time_source_string());
                assert_eq!(item.ctime.to_string(), utils::get_time_source_string());
            } else {
                panic!("Unexpected item in new dir");
            }
            ControlFlow::Continue(())
        })
        .unwrap();
    assert!(has_this);
    assert!(has_parent);

    let new_file = volume_mgr
        .open_file_in_dir(
            new_dir,
            &test_file_name,
            embedded_sdmmc::Mode::ReadWriteCreate,
        )
        .expect("open new file");
    volume_mgr
        .write(new_file, b"Hello")
        .expect("write to new file");
    volume_mgr.close_file(new_file).expect("close new file");

    let mut has_this = false;
    let mut has_parent = false;
    let mut has_new_file = false;
    volume_mgr
        .iterate_dir(new_dir, |item| {
            if item.name == ShortFileName::parent_dir() {
                has_parent = true;
                assert!(item.attributes.is_directory());
                assert_eq!(item.size, 0);
                assert_eq!(item.mtime.to_string(), utils::get_time_source_string());
                assert_eq!(item.ctime.to_string(), utils::get_time_source_string());
            } else if item.name == ShortFileName::this_dir() {
                has_this = true;
                assert!(item.attributes.is_directory());
                assert_eq!(item.size, 0);
                assert_eq!(item.mtime.to_string(), utils::get_time_source_string());
                assert_eq!(item.ctime.to_string(), utils::get_time_source_string());
            } else if item.name == test_file_name {
                has_new_file = true;
                // We wrote "Hello" to it
                assert_eq!(item.size, 5);
                assert!(!item.attributes.is_directory());
                assert_eq!(item.mtime.to_string(), utils::get_time_source_string());
                assert_eq!(item.ctime.to_string(), utils::get_time_source_string());
            } else {
                panic!("Unexpected item in new dir");
            }
            ControlFlow::Continue(())
        })
        .unwrap();
    assert!(has_this);
    assert!(has_parent);
    assert!(has_new_file);

    // Close the root dir and look again
    volume_mgr.close_dir(root_dir).expect("close root");
    volume_mgr.close_dir(new_dir).expect("close new_dir");
    let root_dir = volume_mgr
        .open_root_dir(fat32_volume)
        .expect("open root dir");
    // Check we can't make it again now it exists
    assert!(
        volume_mgr
            .make_dir_in_dir(root_dir, &test_dir_name)
            .is_err()
    );
    let new_dir = volume_mgr
        .open_dir(root_dir, &test_dir_name)
        .expect("find new dir");
    let new_file = volume_mgr
        .open_file_in_dir(new_dir, &test_file_name, embedded_sdmmc::Mode::ReadOnly)
        .expect("re-open new file");
    volume_mgr.close_dir(root_dir).expect("close root");
    volume_mgr.close_dir(new_dir).expect("close new dir");
    volume_mgr.close_file(new_file).expect("close file");
}

#[test]
fn delete_directory() {
    let time_source = utils::make_time_source();
    let disk = utils::make_block_device(utils::DISK_SOURCE).unwrap();
    let volume_mgr = embedded_sdmmc::VolumeManager::new(disk, time_source);

    let fat32_volume = volume_mgr
        .open_raw_volume(embedded_sdmmc::VolumeIdx(1))
        .expect("open volume 1");

    let root_dir = volume_mgr
        .open_root_dir(fat32_volume)
        .expect("open root dir");

    volume_mgr.make_dir_in_dir(root_dir, "FOOBAR").unwrap();

    let dir = volume_mgr.open_dir(root_dir, "FOOBAR").unwrap();

    assert!(matches!(
        volume_mgr.delete_entry_in_dir(root_dir, "FOOBAR"),
        Err(embedded_sdmmc::Error::DirAlreadyOpen)
    ));

    assert!(matches!(
        volume_mgr.delete_entry_in_dir(root_dir, "FOO"),
        Err(embedded_sdmmc::Error::NotFound)
    ));

    volume_mgr.close_dir(dir).unwrap();

    volume_mgr.delete_entry_in_dir(root_dir, "FOOBAR").unwrap();

    assert!(matches!(
        volume_mgr.delete_entry_in_dir(root_dir, "FOOBAR"),
        Err(embedded_sdmmc::Error::NotFound)
    ));

    assert!(matches!(
        volume_mgr.open_dir(root_dir, "FOOBAR"),
        Err(embedded_sdmmc::Error::NotFound)
    ));
}

/// Verify that `iterate_dir` on a FAT32 volume stops calling the callback
/// after it returns `ControlFlow::Break`, even when directory entries span
/// multiple 512-byte blocks (i.e. more than 16 on-disk entries).
#[test]
fn fat32_iterate_dir_break_stops_immediately() {
    let time_source = utils::make_time_source();
    let disk = utils::make_block_device(utils::DISK_SOURCE).unwrap();
    let volume_mgr = embedded_sdmmc::VolumeManager::new(disk, time_source);

    let fat32_volume = volume_mgr
        .open_raw_volume(embedded_sdmmc::VolumeIdx(1))
        .expect("open volume 1");
    let root_dir = volume_mgr
        .open_root_dir(fat32_volume)
        .expect("open root dir");

    // Create a fresh subdirectory to work in.
    let dir_name = ShortFileName::create_from_str("BREAKDIR").unwrap();
    volume_mgr
        .make_dir_in_dir(root_dir, &dir_name)
        .expect("make BREAKDIR");
    let test_dir = volume_mgr
        .open_dir(root_dir, &dir_name)
        .expect("open BREAKDIR");

    // The subdirectory already has "." and ".." (2 entries). Create 15 files
    // so we have 17 on-disk entries total, which exceeds one 512-byte block
    // (512 / 32 = 16 entries per block).
    for i in 0..15 {
        let name = format!("F{:07}.TXT", i);
        let sfn = ShortFileName::create_from_str(&name).unwrap();
        let f = volume_mgr
            .open_file_in_dir(test_dir, &sfn, Mode::ReadWriteCreate)
            .expect("create file");
        volume_mgr.close_file(f).expect("close file");
    }

    // Now iterate with a callback that breaks immediately.
    let mut call_count = 0u32;
    volume_mgr
        .iterate_dir(test_dir, |_entry| {
            call_count += 1;
            ControlFlow::Break(())
        })
        .expect("iterate dir");

    assert_eq!(
        call_count, 1,
        "callback was invoked {call_count} times, expected exactly 1 after Break"
    );

    volume_mgr.close_dir(test_dir).expect("close BREAKDIR");
    volume_mgr.close_dir(root_dir).expect("close root");
}

#[test]
fn create_read_and_delete_long_named_files() {
    let time_source = utils::make_time_source();
    let disk = utils::make_block_device(utils::DISK_SOURCE).unwrap();
    let volume_mgr = embedded_sdmmc::VolumeManager::new(disk, time_source);

    // Exercise a FAT16 subdirectory.
    let fat16_volume = volume_mgr
        .open_raw_volume(embedded_sdmmc::VolumeIdx(0))
        .expect("open FAT16 volume");
    let fat16_root = volume_mgr
        .open_root_dir(fat16_volume)
        .expect("open FAT16 root");
    let fat16_dir = volume_mgr
        .open_dir(fat16_root, "TEST")
        .expect("open FAT16 test dir");
    let file = volume_mgr
        .create_file_in_dir_lfn(fat16_dir, "Wireless upload compatibility.epub")
        .expect("create FAT16 LFN file");
    volume_mgr.write(file, b"EPUB-FAT16").unwrap();
    volume_mgr.close_file(file).unwrap();

    let mut found = false;
    let mut storage = [0u8; 128];
    let mut lfn_buffer = LfnBuffer::new(&mut storage);
    volume_mgr
        .iterate_dir_lfn(fat16_dir, &mut lfn_buffer, |entry, long_name| {
            // Keyed on the long name: the alias is derived, and which one a
            // directory hands out is not part of the contract.
            if long_name == Some("Wireless upload compatibility.epub") {
                found = true;
                assert_eq!(entry.size, 10);
            }
            ControlFlow::Continue(())
        })
        .unwrap();
    assert!(found);
    volume_mgr.close_dir(fat16_dir).unwrap();
    volume_mgr.close_dir(fat16_root).unwrap();
    volume_mgr.close_volume(fat16_volume).unwrap();

    // Exercise a FAT32 directory with the LFN chain crossing a sector boundary.
    let fat32_volume = volume_mgr
        .open_raw_volume(embedded_sdmmc::VolumeIdx(1))
        .expect("open FAT32 volume");
    let fat32_root = volume_mgr
        .open_root_dir(fat32_volume)
        .expect("open FAT32 root");
    volume_mgr.make_dir_in_dir(fat32_root, "LFNTEST").unwrap();
    let fat32_dir = volume_mgr.open_dir(fat32_root, "LFNTEST").unwrap();
    for index in 0..12 {
        let name = format!("FILL{index:04}.TMP");
        let filler = volume_mgr
            .open_file_in_dir(fat32_dir, name.as_str(), Mode::ReadWriteCreate)
            .unwrap();
        volume_mgr.close_file(filler).unwrap();
    }

    let long_name = "Wireless upload 😀 cross-firmware compatibility.epub";
    let file = volume_mgr
        .create_file_in_dir_lfn(fat32_dir, long_name)
        .expect("create cross-sector FAT32 LFN file");
    volume_mgr.write(file, b"EPUB-FAT32").unwrap();
    volume_mgr.close_file(file).unwrap();

    // The alias is derived, so discover it rather than assuming one.
    let mut alias = None;
    let mut storage = [0u8; 192];
    let mut lfn_buffer = LfnBuffer::new(&mut storage);
    volume_mgr
        .iterate_dir_lfn(fat32_dir, &mut lfn_buffer, |entry, given_long_name| {
            if given_long_name == Some(long_name) {
                alias = Some(entry.name);
                assert_eq!(entry.size, 10);
            }
            ControlFlow::Continue(())
        })
        .unwrap();
    let alias = alias.expect("the created file should be listed under its long name");

    // It is reachable through the alias as well, which is what a reader that
    // predates long names would use.
    let file = volume_mgr
        .open_file_in_dir(fat32_dir, alias, Mode::ReadOnly)
        .unwrap();
    let mut contents = [0u8; 10];
    assert_eq!(
        volume_mgr.read(file, &mut contents).unwrap(),
        contents.len()
    );
    assert_eq!(&contents, b"EPUB-FAT32");
    volume_mgr.close_file(file).unwrap();

    // The same name again is refused; a different one is not, and gets its
    // own alias without the caller arranging anything.
    assert!(matches!(
        volume_mgr.create_file_in_dir_lfn(fat32_dir, long_name),
        Err(embedded_sdmmc::Error::FileAlreadyExists)
    ));
    volume_mgr
        .create_file_in_dir_lfn(fat32_dir, "Another.epub")
        .expect("an unused name is accepted")
        .to_file(&volume_mgr)
        .close()
        .unwrap();
    assert!(matches!(
        volume_mgr.create_file_in_dir_lfn(fat32_dir, "Invalid?.epub"),
        Err(embedded_sdmmc::Error::FilenameError(
            embedded_sdmmc::FilenameError::InvalidCharacter
        ))
    ));

    volume_mgr.delete_entry_in_dir(fat32_dir, alias).unwrap();
    let mut found_after_delete = false;
    let mut storage = [0u8; 192];
    let mut lfn_buffer = LfnBuffer::new(&mut storage);
    volume_mgr
        .iterate_dir_lfn(fat32_dir, &mut lfn_buffer, |entry, long_name| {
            found_after_delete |= entry.name == alias
                || long_name == Some("Wireless upload 😀 cross-firmware compatibility.epub");
            ControlFlow::Continue(())
        })
        .unwrap();
    assert!(!found_after_delete);
}

// ****************************************************************************
//
// End Of File
//
// ****************************************************************************

/// A long name can be created with any character Rust can represent, which
/// includes everything above U+FFFF -- emoji among them. UTF-16 stores those
/// as a surrogate pair, and the lookup that backs `open_long_name_file_in_dir`
/// used to treat each 16-bit word as a whole character and give up when it met
/// half of one. So a name could be written and listed correctly and then never
/// found again by the name it was written under.
///
/// The pair is deliberately placed so it straddles the 13-code-unit boundary
/// between two directory entries, which is the case a per-entry decoder misses.
#[test]
fn a_long_name_with_an_emoji_can_be_reopened_by_that_name() {
    let time_source = utils::make_time_source();
    let disk = utils::make_block_device(utils::DISK_SOURCE).unwrap();
    let volume_mgr = embedded_sdmmc::VolumeManager::new(disk, time_source);
    let volume = volume_mgr
        .open_raw_volume(embedded_sdmmc::VolumeIdx(1))
        .expect("open FAT32 volume");
    let root = volume_mgr.open_root_dir(volume).expect("open root");

    // 12 characters, then the emoji: its high half lands in code unit 12 (the
    // last of the first entry) and its low half in unit 13 (the first of the
    // next).
    let long_name = "ABCDEFGHIJKL😀 unicode roundtrip.epub";
    assert_eq!(
        long_name.encode_utf16().take(12).count(),
        12,
        "the first 12 code units should be the plain ASCII prefix"
    );

    let file = volume_mgr
        .create_file_in_dir_lfn(root, long_name)
        .expect("create");
    volume_mgr.write(file, b"round trip").expect("write");
    volume_mgr.close_file(file).expect("close");

    // The name must be findable by exactly the string it was created with.
    let reopened = volume_mgr.open_long_name_file_in_dir(root, long_name, Mode::ReadOnly);
    let reopened = match reopened {
        Ok(f) => f,
        Err(e) => panic!("a name created with an emoji could not be reopened by that name: {e:?}"),
    };
    let mut body = [0u8; 10];
    volume_mgr.read(reopened, &mut body).expect("read");
    assert_eq!(&body, b"round trip");
    volume_mgr.close_file(reopened).expect("close");

    // A name that differs only past the emoji must still miss.
    assert!(matches!(
        volume_mgr.open_long_name_file_in_dir(
            root,
            "ABCDEFGHIJKL😀 unicode roundtrip.txt",
            Mode::ReadOnly
        ),
        Err(embedded_sdmmc::Error::NotFound)
    ));

    volume_mgr.close_dir(root).unwrap();
    volume_mgr.close_volume(volume).unwrap();
}

/// FAT gives a directory one namespace, not two: every entry's long name and
/// short name live in it together, and names that differ only by case are the
/// same name. A second entry answering to a name that is already there makes
/// lookup depend on which one happens to come first on disk.
#[test]
fn a_long_name_already_in_the_directory_is_refused() {
    let volume_mgr = embedded_sdmmc::VolumeManager::new(
        utils::make_block_device(utils::DISK_SOURCE).unwrap(),
        utils::make_time_source(),
    );
    let volume = volume_mgr
        .open_raw_volume(embedded_sdmmc::VolumeIdx(1))
        .expect("open FAT32 volume");
    let root = volume_mgr.open_root_dir(volume).expect("open root");

    volume_mgr
        .create_file_in_dir_lfn(root, "Book.epub")
        .expect("create the first one")
        .to_file(&volume_mgr)
        .close()
        .expect("close");

    // The same long name under a different alias: the alias is free, the name
    // is not.
    assert!(
        matches!(
            volume_mgr.create_file_in_dir_lfn(root, "Book.epub"),
            Err(embedded_sdmmc::Error::FileAlreadyExists)
        ),
        "a long name already in the directory must be refused"
    );

    // Differing only by case is the same name.
    assert!(
        matches!(
            volume_mgr.create_file_in_dir_lfn(root, "BOOK.EPUB"),
            Err(embedded_sdmmc::Error::FileAlreadyExists)
        ),
        "a long name differing only by case must be refused"
    );

    // A long name that collides with an existing *short* name is also taken.
    volume_mgr
        .open_file_in_dir(root, "PLAIN.TXT", Mode::ReadWriteCreate)
        .expect("create a short-named file")
        .to_file(&volume_mgr)
        .close()
        .expect("close");
    assert!(
        matches!(
            volume_mgr.create_file_in_dir_lfn(root, "plain.txt"),
            Err(embedded_sdmmc::Error::FileAlreadyExists)
        ),
        "a long name colliding with an existing short name must be refused"
    );

    // A genuinely new name is still fine.
    volume_mgr
        .create_file_in_dir_lfn(root, "Another Book.epub")
        .expect("an unused name must still be accepted")
        .to_file(&volume_mgr)
        .close()
        .expect("close");

    volume_mgr.close_dir(root).unwrap();
    volume_mgr.close_volume(volume).unwrap();
}

/// 0xFFFF is the padding that fills the unused tail of a long-name entry, and
/// a name that ends exactly on a 13-code-unit boundary is stored with no NUL
/// and no padding at all. A trailing U+FFFF there would be indistinguishable
/// from padding, so the name is refused at creation rather than being written
/// and then never found again.
#[test]
fn a_long_name_containing_the_padding_sentinel_is_refused() {
    let volume_mgr = embedded_sdmmc::VolumeManager::new(
        utils::make_block_device(utils::DISK_SOURCE).unwrap(),
        utils::make_time_source(),
    );
    let volume = volume_mgr
        .open_raw_volume(embedded_sdmmc::VolumeIdx(1))
        .expect("open FAT32 volume");
    let root = volume_mgr.open_root_dir(volume).expect("open root");

    // Exactly 13 UTF-16 code units, the last of them the padding sentinel.
    let name = "ABCDEFGHIJKL\u{FFFF}";
    assert_eq!(name.encode_utf16().count(), 13);

    assert!(
        matches!(
            volume_mgr.create_file_in_dir_lfn(root, name),
            Err(embedded_sdmmc::Error::FilenameError(
                embedded_sdmmc::FilenameError::InvalidCharacter
            ))
        ),
        "a name using the padding sentinel must be refused"
    );

    volume_mgr.close_dir(root).unwrap();
    volume_mgr.close_volume(volume).unwrap();
}

/// The namespace check folds case, and case is not an ASCII-only idea. An
/// accented name differing only in case is the same name.
#[test]
fn a_long_name_differing_only_by_non_ascii_case_is_refused() {
    let volume_mgr = embedded_sdmmc::VolumeManager::new(
        utils::make_block_device(utils::DISK_SOURCE).unwrap(),
        utils::make_time_source(),
    );
    let volume = volume_mgr
        .open_raw_volume(embedded_sdmmc::VolumeIdx(1))
        .expect("open FAT32 volume");
    let root = volume_mgr.open_root_dir(volume).expect("open root");

    volume_mgr
        .create_file_in_dir_lfn(root, "Résumé.epub")
        .expect("create the occupant")
        .to_file(&volume_mgr)
        .close()
        .expect("close");

    assert!(
        matches!(
            volume_mgr.create_file_in_dir_lfn(root, "RÉSUMÉ.EPUB"),
            Err(embedded_sdmmc::Error::FileAlreadyExists)
        ),
        "a long name differing only by non-ASCII case must be refused"
    );

    volume_mgr.close_dir(root).unwrap();
    volume_mgr.close_volume(volume).unwrap();
}

/// Validation allows a long name up to the VFAT maximum of 255 UTF-16 code
/// units, which needs 20 directory entries. The listing parser used to stop at
/// 19, so creation could write a name that listing then refused to reassemble.
/// Whatever creation accepts, listing has to be able to read back.
#[test]
fn the_longest_permitted_long_name_survives_a_round_trip() {
    let volume_mgr = embedded_sdmmc::VolumeManager::new(
        utils::make_block_device(utils::DISK_SOURCE).unwrap(),
        utils::make_time_source(),
    );
    let volume = volume_mgr
        .open_raw_volume(embedded_sdmmc::VolumeIdx(1))
        .expect("open FAT32 volume");
    let root = volume_mgr.open_root_dir(volume).expect("open root");

    // 255 UTF-16 code units: ceil(255 / 13) == 20 entries, the maximum.
    let mut long_name = "a".repeat(251);
    long_name.push_str(".txt");
    assert_eq!(long_name.encode_utf16().count(), 255);

    volume_mgr
        .create_file_in_dir_lfn(root, &long_name)
        .expect("create the longest permitted name")
        .to_file(&volume_mgr)
        .close()
        .expect("close");

    // Listing must give back exactly what was written.
    let mut storage = [0u8; 1024];
    let mut lfn_buffer = embedded_sdmmc::LfnBuffer::new(&mut storage);
    let mut seen = None;
    volume_mgr
        .iterate_dir_lfn(root, &mut lfn_buffer, |entry, long| {
            if long.is_some_and(|l| l.ends_with(".txt") && l.len() > 200) {
                seen = long.map(String::from);
                assert!(!entry.name.to_string().is_empty(), "it has an alias too");
            }
            ControlFlow::Continue(())
        })
        .expect("iterate");
    assert_eq!(
        seen.as_deref(),
        Some(long_name.as_str()),
        "a name creation accepted could not be listed back"
    );

    volume_mgr.close_dir(root).unwrap();
    volume_mgr.close_volume(volume).unwrap();
}

/// R3: the 8.3 alias is derived from the long name, not asked for. It should
/// be recognisable rather than arbitrary, and unique within its directory.
#[test]
fn a_derived_alias_is_recognisable_and_unique() {
    let volume_mgr = embedded_sdmmc::VolumeManager::new(
        utils::make_block_device(utils::DISK_SOURCE).unwrap(),
        utils::make_time_source(),
    );
    let volume = volume_mgr
        .open_raw_volume(embedded_sdmmc::VolumeIdx(1))
        .expect("open FAT32 volume");
    let root = volume_mgr.open_root_dir(volume).expect("open root");

    let alias_of = |name: &str| -> String {
        volume_mgr
            .create_file_in_dir_lfn(root, name)
            .expect("create")
            .to_file(&volume_mgr)
            .close()
            .expect("close");
        let mut storage = [0u8; 512];
        let mut lfn_buffer = LfnBuffer::new(&mut storage);
        let mut found = None;
        volume_mgr
            .iterate_dir_lfn(root, &mut lfn_buffer, |entry, long| {
                if long == Some(name) {
                    found = Some(entry.name.to_string());
                }
                ControlFlow::Continue(())
            })
            .expect("iterate");
        found.expect("the file should be listed under its long name")
    };

    // Spaces go, case is raised, the extension is kept, and the base is cut
    // short to leave room for the tail.
    assert_eq!(alias_of("A Real Book.epub"), "AREALB~1.EPU");
    // A second name reducing to the same base gets the next tail along.
    assert_eq!(alias_of("A Real Bookshelf.epub"), "AREALB~2.EPU");
    // A character a long name allows but 8.3 does not becomes an underscore
    // rather than vanishing, so the alias still resembles the name.
    assert_eq!(alias_of("Plan+draft.md"), "PLAN_D~1.MD");
    // Anything outside ASCII goes the same way. A short name is eleven raw
    // bytes with no encoding attached, and one of them -- 0xE5, which is what
    // a leading "å" would produce -- means "deleted entry".
    assert_eq!(alias_of("\u{e5}land.epub"), "_LAND~1.EPU");
    assert_eq!(alias_of("M\u{e4}rchen \u{1f600}.epub"), "M_RCHE~1.EPU");
    // No extension is fine.
    assert_eq!(alias_of("Notes to self"), "NOTEST~1");

    volume_mgr.close_dir(root).unwrap();
    volume_mgr.close_volume(volume).unwrap();
}

/// R4: a folder can be named the way a file is.
#[test]
fn a_directory_can_be_created_with_a_long_name() {
    let volume_mgr = embedded_sdmmc::VolumeManager::new(
        utils::make_block_device(utils::DISK_SOURCE).unwrap(),
        utils::make_time_source(),
    );
    let volume = volume_mgr
        .open_raw_volume(embedded_sdmmc::VolumeIdx(1))
        .expect("open FAT32 volume");
    let root = volume_mgr.open_root_dir(volume).expect("open root");

    const FOLDER: &str = "Books I Have Not Read Yet";
    volume_mgr
        .make_dir_in_dir_lfn(root, FOLDER)
        .expect("make the folder");

    // It lists under its long name, and is a directory.
    let mut storage = [0u8; 256];
    let mut lfn_buffer = LfnBuffer::new(&mut storage);
    let mut alias = None;
    volume_mgr
        .iterate_dir_lfn(root, &mut lfn_buffer, |entry, long| {
            if long == Some(FOLDER) {
                assert!(entry.attributes.is_directory(), "it must be a directory");
                alias = Some(entry.name);
            }
            ControlFlow::Continue(())
        })
        .expect("iterate");
    let alias = alias.expect("the folder should be listed under its long name");

    // And it works as a directory: openable, and a file put in it comes back.
    let folder = volume_mgr.open_dir(root, alias).expect("open the folder");
    volume_mgr
        .create_file_in_dir_lfn(folder, "A Book In It.epub")
        .expect("create inside")
        .to_file(&volume_mgr)
        .close()
        .expect("close");
    let mut names = Vec::new();
    volume_mgr
        .iterate_dir_lfn(folder, &mut lfn_buffer, |_e, long| {
            if let Some(l) = long {
                names.push(String::from(l));
            }
            ControlFlow::Continue(())
        })
        .expect("iterate inside");
    assert!(names.iter().any(|n| n == "A Book In It.epub"));

    // The same folder name twice is refused.
    assert!(matches!(
        volume_mgr.make_dir_in_dir_lfn(root, FOLDER),
        Err(embedded_sdmmc::Error::FileAlreadyExists)
    ));

    volume_mgr.close_dir(folder).unwrap();
    volume_mgr.close_dir(root).unwrap();
    volume_mgr.close_volume(volume).unwrap();
}

/// A long name may hold characters a short name cannot, and the alias derived
/// from it must not smuggle them through. A short name is eleven raw bytes
/// with no encoding attached; `0xE5` in the first of them is how FAT marks an
/// entry deleted, so an alias beginning `å` used to produce a file that was
/// created, written and closed successfully and then was not in the directory
/// at all.
#[test]
fn a_name_outside_ascii_survives_creation_and_can_be_reopened() {
    let volume_mgr = embedded_sdmmc::VolumeManager::new(
        utils::make_block_device(utils::DISK_SOURCE).unwrap(),
        utils::make_time_source(),
    );
    let volume = volume_mgr
        .open_raw_volume(embedded_sdmmc::VolumeIdx(1))
        .expect("open FAT32 volume");
    let root = volume_mgr.open_root_dir(volume).expect("open root");

    // The first begins with the deleted-entry byte; the rest are accented or
    // non-BMP throughout.
    for name in [
        "\u{e5}land.epub",
        "\u{e4}\u{f6}\u{fc}\u{df} everywhere.epub",
        "M\u{e4}rchen \u{1f600}.epub",
    ] {
        let file = volume_mgr
            .create_file_in_dir_lfn(root, name)
            .unwrap_or_else(|e| panic!("create {name:?}: {e:?}"));
        volume_mgr.write(file, b"body").expect("write");
        volume_mgr.close_file(file).expect("close");

        // It is in the listing, under exactly the name it was created with,
        // with an alias that is plain ASCII.
        let mut storage = [0u8; 512];
        let mut lfn_buffer = LfnBuffer::new(&mut storage);
        let mut alias = None;
        volume_mgr
            .iterate_dir_lfn(root, &mut lfn_buffer, |entry, long| {
                if long == Some(name) {
                    alias = Some(entry.name.to_string());
                }
                ControlFlow::Continue(())
            })
            .expect("iterate");
        let alias = alias.unwrap_or_else(|| panic!("{name:?} is not in the directory"));
        assert!(
            alias.is_ascii(),
            "the alias for {name:?} is not ASCII: {alias:?}"
        );
        assert!(
            !alias.as_bytes().starts_with(&[0xE5]),
            "the alias for {name:?} begins with the deleted-entry byte"
        );

        // And it opens again by the name it was given.
        let reopened = volume_mgr
            .open_long_name_file_in_dir(root, name, Mode::ReadOnly)
            .unwrap_or_else(|e| panic!("reopen {name:?}: {e:?}"));
        let mut body = [0u8; 4];
        volume_mgr.read(reopened, &mut body).expect("read");
        assert_eq!(&body, b"body");
        volume_mgr.close_file(reopened).expect("close");
    }

    volume_mgr.close_dir(root).unwrap();
    volume_mgr.close_volume(volume).unwrap();
}

/// R4 begins the upload flow, so a folder that could not be created must not
/// be left on the card. The name used to be published before the directory it
/// names existed, and an entry whose stored cluster is zero reads back as the
/// root -- so a failed `make_dir_in_dir_lfn` could leave a folder that claimed
/// to be the root directory.
#[test]
fn a_directory_that_cannot_be_created_is_not_left_behind() {
    const FOLDER: &str = "Science Fiction";
    let device = utils::FailRegion::new(
        utils::make_block_device(utils::DISK_SOURCE).expect("disk image"),
    );
    let volume_mgr = embedded_sdmmc::VolumeManager::new(device, utils::make_time_source());
    let volume = volume_mgr
        .open_raw_volume(embedded_sdmmc::VolumeIdx(1))
        .expect("open FAT32 volume");
    let root = volume_mgr.open_root_dir(volume).expect("open root");

    // Fail the writes that claim the new directory's cluster. The parent
    // directory is writable throughout, so nothing stops the name being
    // published except the ordering.
    let (fat_start, fat_end) = volume_mgr.device(|d| fat32_fat_region(&d.inner));
    volume_mgr.device(|d| d.write_region.set(Some((fat_start, fat_end))));
    let attempt = volume_mgr.make_dir_in_dir_lfn(root, FOLDER);
    volume_mgr.device(|d| d.write_region.set(None));

    assert!(attempt.is_err(), "the folder creation should have failed");

    let mut storage = [0u8; 256];
    let mut lfn_buffer = LfnBuffer::new(&mut storage);
    let mut found = false;
    volume_mgr
        .iterate_dir_lfn(root, &mut lfn_buffer, |entry, long| {
            found |= long == Some(FOLDER);
            // Nothing may claim to be the root either.
            if long == Some(FOLDER) {
                panic!(
                    "a folder that could not be created is on the card, cluster {:?}",
                    entry.cluster
                );
            }
            ControlFlow::Continue(())
        })
        .expect("iterate");
    assert!(!found, "a folder that could not be created is on the card");

    volume_mgr.close_dir(root).unwrap();
    volume_mgr.close_volume(volume).unwrap();
}

/// The blocks holding the FAT of the image's FAT32 partition.
fn fat32_fat_region<D: embedded_sdmmc::BlockDevice>(device: &D) -> (u32, u32) {
    use embedded_sdmmc::{Block, BlockIdx};
    let at = |b: &Block, o: usize| -> u32 {
        u32::from_le_bytes([b.contents[o], b.contents[o + 1], b.contents[o + 2], b.contents[o + 3]])
    };
    let mut mbr = [Block::new()];
    device.read(&mut mbr, BlockIdx(0)).ok();
    let start = at(&mbr[0], 446 + 16 + 8);
    let mut bpb = [Block::new()];
    device.read(&mut bpb, BlockIdx(start)).ok();
    let reserved = u32::from(u16::from_le_bytes([bpb[0].contents[14], bpb[0].contents[15]]));
    let fats = u32::from(bpb[0].contents[16]);
    let per_fat = at(&bpb[0], 36);
    let fat_start = start + reserved;
    (fat_start, fat_start + fats * per_fat)
}

/// A thousand books whose names share an opening is an ordinary library, not
/// an error. The alias search used to stop after `~999` and refuse to create
/// anything more, which imposed a directory limit far below FAT's own.
#[test]
fn many_names_sharing_a_basis_all_get_aliases() {
    let volume_mgr = embedded_sdmmc::VolumeManager::new(
        utils::make_block_device(utils::DISK_SOURCE).unwrap(),
        utils::make_time_source(),
    );
    let volume = volume_mgr
        .open_raw_volume(embedded_sdmmc::VolumeIdx(1))
        .expect("open FAT32 volume");
    let root = volume_mgr.open_root_dir(volume).expect("open root");
    volume_mgr
        .make_dir_in_dir_lfn(root, "The Series")
        .expect("make folder");
    let folder = volume_mgr.open_dir(root, "THESER~1").expect("open folder");

    // Every one of these reduces to the same eight characters.
    for volume_number in 1..=1010 {
        let name = format!("The Chronicles of Somewhere, Volume {volume_number}.epub");
        volume_mgr
            .create_file_in_dir_lfn(folder, &name)
            .unwrap_or_else(|e| panic!("create #{volume_number}: {e:?}"))
            .to_file(&volume_mgr)
            .close()
            .expect("close");
    }

    // All present, and no two share an alias.
    let mut aliases: Vec<String> = Vec::new();
    let mut storage = [0u8; 512];
    let mut lfn_buffer = LfnBuffer::new(&mut storage);
    volume_mgr
        .iterate_dir_lfn(folder, &mut lfn_buffer, |entry, long| {
            if long.is_some_and(|l| l.starts_with("The Chronicles")) {
                aliases.push(entry.name.to_string());
            }
            ControlFlow::Continue(())
        })
        .expect("iterate");
    assert_eq!(aliases.len(), 1010, "every book should be in the directory");
    aliases.sort();
    let before = aliases.len();
    aliases.dedup();
    assert_eq!(aliases.len(), before, "two books were given the same alias");

    volume_mgr.close_dir(folder).unwrap();
    volume_mgr.close_dir(root).unwrap();
    volume_mgr.close_volume(volume).unwrap();
}

/// The cluster a directory is built in is allocated before anything names it,
/// which is what stops a half-made folder appearing on the card. The other
/// half of that bargain is that a failure while laying the cluster out has to
/// give it back: nothing refers to it, so if it stays allocated it is lost for
/// the life of the filesystem.
///
/// R4 makes this the first step of every upload into a new folder, so a card
/// that is merely flaky would bleed a cluster per attempt.
#[test]
fn a_directory_that_fails_while_being_laid_out_frees_its_cluster() {
    // What cluster does the folder get when nothing goes wrong?
    let baseline = {
        let volume_mgr = embedded_sdmmc::VolumeManager::new(
            utils::make_block_device(utils::DISK_SOURCE).unwrap(),
            utils::make_time_source(),
        );
        let volume = volume_mgr
            .open_raw_volume(embedded_sdmmc::VolumeIdx(1))
            .expect("open volume");
        let root = volume_mgr.open_root_dir(volume).expect("open root");
        volume_mgr
            .make_dir_in_dir_lfn(root, "Science Fiction")
            .expect("make folder");
        volume_mgr
            .find_directory_entry(root, "SCIENC~1")
            .expect("find folder")
            .cluster
    };

    let device = utils::FailRegion::new(
        utils::make_block_device(utils::DISK_SOURCE).expect("disk image"),
    );
    let volume_mgr = embedded_sdmmc::VolumeManager::new(device, utils::make_time_source());
    let volume = volume_mgr
        .open_raw_volume(embedded_sdmmc::VolumeIdx(1))
        .expect("open volume");
    let root = volume_mgr.open_root_dir(volume).expect("open root");

    // Fail writes to the data area only. The FAT is left writable, so the
    // allocation succeeds and so does the release afterwards; what fails is
    // laying out the new directory's own blocks.
    let (_, fat_end) = volume_mgr.device(|d| fat32_fat_region(&d.inner));
    volume_mgr.device(|d| d.write_region.set(Some((fat_end, u32::MAX))));
    let attempt = volume_mgr.make_dir_in_dir_lfn(root, "Doomed Folder");
    volume_mgr.device(|d| d.write_region.set(None));

    assert!(
        volume_mgr.device(|d| d.injected.get()) > 0,
        "no write was intercepted, so this proves nothing about the failure path"
    );
    assert!(
        matches!(attempt, Err(embedded_sdmmc::Error::DeviceError(_))),
        "expected the write failure to surface, got {:?}",
        attempt.err()
    );

    // Nothing was published.
    assert!(matches!(
        volume_mgr.find_directory_entry(root, "DOOMED~1"),
        Err(embedded_sdmmc::Error::NotFound)
    ));

    // And the cluster came back: the next folder gets the one the failed
    // attempt had taken, rather than the one after it.
    volume_mgr
        .make_dir_in_dir_lfn(root, "Science Fiction")
        .expect("make folder after the failure");
    let after = volume_mgr
        .find_directory_entry(root, "SCIENC~1")
        .expect("find folder")
        .cluster;
    assert_eq!(
        after, baseline,
        "the failed attempt kept its cluster; it is allocated and unreachable"
    );

    volume_mgr.close_dir(root).unwrap();
    volume_mgr.close_volume(volume).unwrap();
}
