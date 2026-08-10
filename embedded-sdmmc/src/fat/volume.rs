//! FAT-specific volume support.

use core::convert::TryFrom;
use core::ops::ControlFlow;

use byteorder::{ByteOrder, LittleEndian};

use crate::{
    Attributes, Block, BlockCache, BlockCount, BlockDevice, BlockIdx, ClusterId, DirEntry,
    DirectoryInfo, Error, LfnBuffer, ShortFileName, TimeSource, Timestamp, VolumeType, debug,
    fat::{
        Bpb, Fat16Info, Fat32Info, FatSpecificInfo, FatType, InfoSector, OnDiskDirEntry,
        RESERVED_ENTRIES,
    },
    filesystem::{FilenameError, validate_long_filename},
    trace, warn,
};
use heapless::Vec;

const MAX_LFN_ENTRIES: usize = 20;
const MAX_LFN_SLOTS: usize = MAX_LFN_ENTRIES + 1;
const LFN_CHARS_PER_ENTRY: usize = 13;

#[derive(Clone, Copy, Debug)]
struct DirectorySlot {
    block: BlockIdx,
    offset: u32,
}

struct PendingLfnSlots {
    slots: Vec<DirectorySlot, MAX_LFN_ENTRIES>,
    checksum: u8,
    next_sequence: u8,
    active: bool,
}

/// Map one character of a long name into what an 8.3 alias can hold.
///
/// The result is always ASCII. A short name is eleven raw bytes with no
/// encoding attached, so anything outside ASCII would be guesswork -- and one
/// byte in particular, `0xE5`, means "this entry is deleted", which a name
/// beginning with `å` would otherwise produce. Nothing is lost by flattening:
/// the long name keeps the user's spelling, and the alias exists only so the
/// entry has a short name at all.
fn basis_byte(c: char) -> u8 {
    match c {
        c if c.is_ascii_lowercase() => c.to_ascii_uppercase() as u8,
        '\u{0000}'..='\u{001F}' | '"' | '*' | '+' | ',' | '/' | ':' | ';' | '<' | '=' | '>'
        | '?' | '[' | '\\' | ']' | '|' => b'_',
        c if !c.is_ascii() => b'_',
        c => c as u8,
    }
}

/// Split a long name into the stem and extension an alias is built from.
///
/// Spaces and interior dots go: FAT allows neither, and dropping them is what
/// makes `A Real Book.epub` read as `AREALBO` rather than `A_REAL_`.
fn short_name_basis(long_name: &str) -> (Vec<u8, 8>, Vec<u8, 3>) {
    let (stem, ext) = match long_name.rfind('.') {
        Some(i) if i > 0 => (&long_name[..i], &long_name[i + 1..]),
        _ => (long_name, ""),
    };
    let mut base = Vec::<u8, 8>::new();
    for c in stem.chars().filter(|c| *c != '.' && *c != ' ') {
        if base.push(basis_byte(c)).is_err() {
            break;
        }
    }
    if base.is_empty() {
        let _ = base.push(b'_');
    }
    let mut extension = Vec::<u8, 3>::new();
    for c in ext.chars().filter(|c| *c != '.' && *c != ' ') {
        if extension.push(basis_byte(c)).is_err() {
            break;
        }
    }
    (base, extension)
}

/// A 16-bit digest of the long name, used to spread aliases out once the
/// readable `~N` forms are gone. Only determinism matters here.
fn long_name_digest(long_name: &str) -> u16 {
    let mut hash: u16 = 0x1505;
    for b in long_name.as_bytes() {
        hash = hash.rotate_left(5) ^ u16::from(*b);
    }
    hash
}

/// Assemble `BASE.EXT` from parts that are already ASCII.
///
/// Eight characters of base, a dot and three of extension is twelve, which is
/// the buffer, so nothing here can overflow.
fn assemble_alias(base: &[u8], ext: &[u8]) -> heapless::String<12> {
    let mut out = heapless::String::<12>::new();
    for &b in base.iter().take(8) {
        let _ = out.push(b as char);
    }
    if !ext.is_empty() {
        let _ = out.push('.');
        for &b in ext.iter().take(3) {
            let _ = out.push(b as char);
        }
    }
    out
}

/// `BASE~N`, with the base shortened to leave room for the tail.
fn compose_alias(base: &[u8], ext: &[u8], tail: u32) -> heapless::String<12> {
    use core::fmt::Write;
    let mut suffix = heapless::String::<8>::new();
    let _ = write!(suffix, "~{tail}");
    let keep = 8usize.saturating_sub(suffix.len()).max(1);

    let mut stem = Vec::<u8, 8>::new();
    for &b in base.iter().take(keep) {
        let _ = stem.push(b);
    }
    for b in suffix.bytes() {
        let _ = stem.push(b);
    }
    assemble_alias(&stem, ext)
}

/// `BBHHHH~1`: two characters of base, four hex digits of digest, and a tail.
///
/// Once the readable forms are taken this is what the search moves to. It
/// gives 65536 candidates for one directory rather than the 999 a decimal tail
/// allows, which matters for a library that expects to hold thousands of books
/// whose names may share an opening.
fn compose_hashed_alias(base: &[u8], ext: &[u8], digest: u16) -> heapless::String<12> {
    use core::fmt::Write;
    let mut stem = Vec::<u8, 8>::new();
    for &b in base.iter().take(2) {
        let _ = stem.push(b);
    }
    let mut hex = heapless::String::<8>::new();
    let _ = write!(hex, "{digest:04X}~1");
    for b in hex.bytes() {
        let _ = stem.push(b);
    }
    assemble_alias(&stem, ext)
}

/// Match a long-name entry's code units off the end of `remaining`.
///
/// `words` must arrive in reverse order, because a long name is compared from
/// its end: the entries come off the disk last-chunk-first, and within a chunk
/// the last code unit is the one furthest along the name.
///
/// Anything above U+FFFF is stored as a high/low surrogate pair, so walking
/// backwards we meet the low half first and have to hold it until its high
/// half turns up. That may not happen until the next entry -- 13 code units
/// per entry does not respect character boundaries -- which is why the pending
/// half is owned by the caller and lives across calls.
/// Take `c` off the end of `s`, optionally ignoring ASCII case.
fn strip_last_char(s: &str, c: char, fold_case: bool) -> Option<&str> {
    let last = s.chars().next_back()?;
    let same = if fold_case {
        // Simple case *mapping*, not Unicode case folding: two scalars are the
        // same name if lowercasing them agrees. That covers the accented
        // Latin, Greek and Cyrillic pairs an ASCII fold misses, and costs a
        // table already in core rather than a dependency.
        //
        // It is not the whole of Unicode. Characters that fold together
        // without lowercasing to the same scalar still compare as different --
        // Greek final sigma is the standard example, since lowercase sigma and
        // final sigma are both already lowercase. Closing that needs real
        // case-folding tables, which is a bigger thing than this crate should
        // carry; what is documented is what is done.
        last == c || last.to_lowercase().eq(c.to_lowercase())
    } else {
        last == c
    };
    same.then(|| &s[..s.len() - last.len_utf8()])
}

/// With `fold_case`, ASCII letters compare equal regardless of case. FAT
/// treats a directory's long and short names as one namespace in which case
/// differences are collisions rather than distinctions, so creating a name
/// needs that comparison even though looking one up does not.
///
/// Returns `false` if this is not the name we are looking for.
fn strip_lfn_words(
    words: impl Iterator<Item = u16>,
    remaining: &mut &str,
    pending_low: &mut Option<u16>,
    fold_case: bool,
) -> bool {
    for word in words {
        let c = if (0xDC00..=0xDFFF).contains(&word) {
            // Low half. Its high half is the next word we will see.
            *pending_low = Some(word);
            continue;
        } else if (0xD800..=0xDBFF).contains(&word) {
            let Some(low) = pending_low.take() else {
                // A high half with nothing to pair it with: not a name we can
                // have written.
                return false;
            };
            let code_point = 0x1_0000u32
                + (((u32::from(word) - 0xD800) << 10) | (u32::from(low) - 0xDC00));
            match char::from_u32(code_point) {
                Some(c) => c,
                None => return false,
            }
        } else {
            if pending_low.is_some() {
                // A low half followed by something that cannot complete it.
                return false;
            }
            match char::from_u32(u32::from(word)) {
                Some(c) => c,
                None => return false,
            }
        };
        let Some(r) = strip_last_char(remaining, c, fold_case) else {
            return false;
        };
        *remaining = r;
    }
    true
}

impl PendingLfnSlots {
    fn new() -> Self {
        Self {
            slots: Vec::new(),
            checksum: 0,
            next_sequence: 0,
            active: false,
        }
    }

    fn clear(&mut self) {
        self.slots.clear();
        self.active = false;
        self.next_sequence = 0;
    }

    fn observe(&mut self, entry: &OnDiskDirEntry<'_>, slot: DirectorySlot) {
        let Some((is_start, sequence, checksum, _)) = entry.lfn_contents() else {
            self.clear();
            return;
        };
        if is_start && (1..=MAX_LFN_ENTRIES as u8).contains(&sequence) {
            self.clear();
            self.active = true;
            self.checksum = checksum;
            self.next_sequence = sequence - 1;
            let _ = self.slots.push(slot);
        } else if self.active
            && checksum == self.checksum
            && sequence != 0
            && sequence == self.next_sequence
        {
            self.next_sequence -= 1;
            let _ = self.slots.push(slot);
        } else {
            self.clear();
        }
    }

    fn belongs_to(&self, short_name: &ShortFileName) -> bool {
        self.active && self.next_sequence == 0 && self.checksum == short_name.csum()
    }
}

/// An MS-DOS 11 character volume label.
///
/// ISO-8859-1 encoding is assumed. Trailing spaces are trimmed. Reserved
/// characters are not allowed. There is no file extension, unlike with a
/// filename.
///
/// Volume labels can be found in the BIOS Parameter Block, and in a root
/// directory entry with the 'Volume Label' bit set. Both places should have the
/// same contents, but they can get out of sync.
///
/// MS-DOS FDISK would show you the one in the BPB, but DIR would show you the
/// one in the root directory.
#[cfg_attr(feature = "defmt-log", derive(defmt::Format))]
#[derive(PartialEq, Eq, Clone)]
pub struct VolumeName {
    pub(crate) contents: [u8; Self::TOTAL_LEN],
}

impl VolumeName {
    const TOTAL_LEN: usize = 11;

    /// Get name
    pub fn name(&self) -> &[u8] {
        let mut bytes = &self.contents[..];
        while let [rest @ .., last] = bytes {
            if last.is_ascii_whitespace() {
                bytes = rest;
            } else {
                break;
            }
        }
        bytes
    }

    /// Create a new MS-DOS volume label.
    pub fn create_from_str(name: &str) -> Result<VolumeName, FilenameError> {
        let mut sfn = VolumeName {
            contents: [b' '; Self::TOTAL_LEN],
        };

        let mut idx = 0;
        for ch in name.chars() {
            match ch {
                // Microsoft say these are the invalid characters
                '\u{0000}'..='\u{001F}'
                | '"'
                | '*'
                | '+'
                | ','
                | '/'
                | ':'
                | ';'
                | '<'
                | '='
                | '>'
                | '?'
                | '['
                | '\\'
                | ']'
                | '.'
                | '|' => {
                    return Err(FilenameError::InvalidCharacter);
                }
                x if x > '\u{00FF}' => {
                    // We only handle ISO-8859-1 which is Unicode Code Points
                    // \U+0000 to \U+00FF. This is above that.
                    return Err(FilenameError::InvalidCharacter);
                }
                _ => {
                    let b = ch as u8;
                    if idx < Self::TOTAL_LEN {
                        sfn.contents[idx] = b;
                    } else {
                        return Err(FilenameError::NameTooLong);
                    }
                    idx += 1;
                }
            }
        }
        if idx == 0 {
            return Err(FilenameError::FilenameEmpty);
        }
        Ok(sfn)
    }

    /// Convert to a Short File Name
    ///
    /// # Safety
    ///
    /// Volume Labels can contain things that Short File Names cannot, so only
    /// do this conversion if you are creating the name of a directory entry
    /// with the 'Volume Label' attribute.
    pub unsafe fn to_short_filename(self) -> ShortFileName {
        ShortFileName {
            contents: self.contents,
        }
    }
}

impl core::fmt::Display for VolumeName {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        let mut printed = 0;
        for &c in self.name().iter() {
            // converting a byte to a codepoint means you are assuming
            // ISO-8859-1 encoding, because that's how Unicode was designed.
            write!(f, "{}", c as char)?;
            printed += 1;
        }
        if let Some(mut width) = f.width() {
            if width > printed {
                width -= printed;
                for _ in 0..width {
                    write!(f, "{}", f.fill())?;
                }
            }
        }
        Ok(())
    }
}

impl core::fmt::Debug for VolumeName {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        write!(f, "VolumeName(\"{}\")", self)
    }
}

fn serialize_lfn_entry(sequence: u8, is_last: bool, checksum: u8, name: &[u16]) -> [u8; 32] {
    debug_assert!(!name.is_empty());
    debug_assert!(name.len() <= LFN_CHARS_PER_ENTRY);

    let mut units = [0xFFFFu16; LFN_CHARS_PER_ENTRY];
    units[..name.len()].copy_from_slice(name);
    if name.len() < units.len() {
        units[name.len()] = 0;
    }

    let mut raw = [0u8; 32];
    raw[0] = sequence | if is_last { 0x40 } else { 0 };
    raw[11] = Attributes::LFN;
    raw[12] = 0;
    raw[13] = checksum;
    raw[26] = 0;
    raw[27] = 0;
    for (unit, offsets) in units
        .iter()
        .zip([1usize, 3, 5, 7, 9, 14, 16, 18, 20, 22, 24, 28, 30])
    {
        raw[offsets..offsets + 2].copy_from_slice(&unit.to_le_bytes());
    }
    raw
}

fn write_directory_slot<D>(
    block_cache: &mut BlockCache<D>,
    slot: DirectorySlot,
    raw: &[u8; 32],
) -> Result<(), Error<D::Error>>
where
    D: BlockDevice,
{
    let block = block_cache
        .read_mut(slot.block)
        .map_err(Error::DeviceError)?;
    let start = usize::try_from(slot.offset).map_err(|_| Error::ConversionError)?;
    block[start..start + OnDiskDirEntry::LEN].copy_from_slice(raw);
    block_cache.write_back().map_err(Error::DeviceError)
}

/// Retire a directory entry by marking each of its slots deleted.
///
/// `slots` arrives as it sits on the disk: the long-name entries first, then
/// the short entry they belong to. They are marked in the opposite order,
/// because these are separate sector writes and any of them can fail.
///
/// The short entry is what makes the file exist. Marking it first makes it the
/// commit point: fail before it and nothing has changed, so the entry is
/// wholly intact; fail after it and the file is wholly gone, which is what the
/// caller asked for. Marking the long-name entries first would instead put the
/// failure in the middle -- a live file whose long name had been partly erased,
/// findable by neither name it used to answer to.
///
/// A failure after the commit point leaves long-name entries with no short
/// entry behind them. Readers already skip those, so nothing is misread; they
/// are wasted directory slots until something reclaims them. That is the
/// lesser of the two failures, and the reason the delete still reports success:
/// the name the caller asked to remove is gone.
fn mark_directory_slots_deleted<D>(
    block_cache: &mut BlockCache<D>,
    slots: &[DirectorySlot],
) -> Result<(), Error<D::Error>>
where
    D: BlockDevice,
{
    let Some((short_slot, lfn_slots)) = slots.split_last() else {
        return Ok(());
    };

    // The commit point. Its failure is the caller's failure.
    mark_one_slot_deleted(block_cache, short_slot)?;

    // Past here the entry is gone whatever happens; what is left is tidying.
    for slot in lfn_slots {
        if mark_one_slot_deleted(block_cache, slot).is_err() {
            break;
        }
    }
    Ok(())
}

fn mark_one_slot_deleted<D>(
    block_cache: &mut BlockCache<D>,
    slot: &DirectorySlot,
) -> Result<(), Error<D::Error>>
where
    D: BlockDevice,
{
    {
        let block = block_cache
            .read_mut(slot.block)
            .map_err(Error::DeviceError)?;
        let start = usize::try_from(slot.offset).map_err(|_| Error::ConversionError)?;
        block[start] = 0xE5;
        block_cache.write_back().map_err(Error::DeviceError)?;
    }
    Ok(())
}

/// Identifies a FAT16 or FAT32 Volume on the disk.
#[cfg_attr(feature = "defmt-log", derive(defmt::Format))]
#[derive(Debug, PartialEq, Eq)]
pub struct FatVolume {
    /// The block number of the start of the partition. All other BlockIdx values are relative to this.
    pub(crate) lba_start: BlockIdx,
    /// The number of blocks in this volume
    pub(crate) num_blocks: BlockCount,
    /// The name of this volume
    pub(crate) name: VolumeName,
    /// Number of 512 byte blocks (or Blocks) in a cluster
    pub(crate) blocks_per_cluster: u8,
    /// The block the data starts in. Relative to start of partition (so add
    /// `self.lba_offset` before passing to volume manager)
    pub(crate) first_data_block: BlockCount,
    /// The block the FAT starts in. Relative to start of partition (so add
    /// `self.lba_offset` before passing to volume manager)
    pub(crate) fat_start: BlockCount,
    /// The block the second FAT starts in. Relative to start of partition (so add
    /// `self.lba_offset` before passing to volume manager)
    pub(crate) second_fat_start: Option<BlockCount>,
    /// Expected number of free clusters
    pub(crate) free_clusters_count: Option<u32>,
    /// Number of the next expected free cluster
    pub(crate) next_free_cluster: Option<ClusterId>,
    /// Total number of clusters
    pub(crate) cluster_count: u32,
    /// Type of FAT
    pub(crate) fat_specific_info: FatSpecificInfo,
}

impl FatVolume {
    /// Write a new entry in the FAT
    pub fn update_info_sector<D>(
        &mut self,
        block_cache: &mut BlockCache<D>,
    ) -> Result<(), Error<D::Error>>
    where
        D: BlockDevice,
    {
        match &self.fat_specific_info {
            FatSpecificInfo::Fat16(_) => {
                // FAT16 volumes don't have an info sector
            }
            FatSpecificInfo::Fat32(fat32_info) => {
                if self.free_clusters_count.is_none() && self.next_free_cluster.is_none() {
                    return Ok(());
                }
                trace!("Reading info sector");
                let block = block_cache
                    .read_mut(fat32_info.info_location)
                    .map_err(Error::DeviceError)?;
                if let Some(count) = self.free_clusters_count {
                    block[488..492].copy_from_slice(&count.to_le_bytes());
                }
                if let Some(next_free_cluster) = self.next_free_cluster {
                    block[492..496].copy_from_slice(&next_free_cluster.0.to_le_bytes());
                }
                trace!("Writing info sector");
                block_cache.write_back()?;
            }
        }
        Ok(())
    }

    /// Get the type of FAT this volume is
    pub(crate) fn get_fat_type(&self) -> FatType {
        match &self.fat_specific_info {
            FatSpecificInfo::Fat16(_) => FatType::Fat16,
            FatSpecificInfo::Fat32(_) => FatType::Fat32,
        }
    }

    /// Write a new entry in the FAT
    fn update_fat<D>(
        &mut self,
        block_cache: &mut BlockCache<D>,
        cluster: ClusterId,
        new_value: ClusterId,
    ) -> Result<(), Error<D::Error>>
    where
        D: BlockDevice,
    {
        let mut second_fat_block_num = None;
        match &self.fat_specific_info {
            FatSpecificInfo::Fat16(_fat16_info) => {
                let fat_offset = cluster.0 * 2;
                let this_fat_block_num = self.lba_start + self.fat_start.offset_bytes(fat_offset);
                if let Some(second_fat_start) = self.second_fat_start {
                    second_fat_block_num =
                        Some(self.lba_start + second_fat_start.offset_bytes(fat_offset));
                }
                let this_fat_ent_offset = (fat_offset % Block::LEN_U32) as usize;
                trace!("Reading FAT for update");
                let block = block_cache
                    .read_mut(this_fat_block_num)
                    .map_err(Error::DeviceError)?;
                // See <https://en.wikipedia.org/wiki/Design_of_the_FAT_file_system>
                let entry = match new_value {
                    ClusterId::INVALID => 0xFFF6,
                    ClusterId::BAD => 0xFFF7,
                    ClusterId::EMPTY => 0x0000,
                    ClusterId::END_OF_FILE => 0xFFFF,
                    _ => new_value.0 as u16,
                };
                LittleEndian::write_u16(
                    &mut block[this_fat_ent_offset..=this_fat_ent_offset + 1],
                    entry,
                );
            }
            FatSpecificInfo::Fat32(_fat32_info) => {
                // FAT32 => 4 bytes per entry
                let fat_offset = cluster.0 * 4;
                let this_fat_block_num = self.lba_start + self.fat_start.offset_bytes(fat_offset);
                if let Some(second_fat_start) = self.second_fat_start {
                    second_fat_block_num =
                        Some(self.lba_start + second_fat_start.offset_bytes(fat_offset));
                }
                let this_fat_ent_offset = (fat_offset % Block::LEN_U32) as usize;
                trace!("Reading FAT for update");
                let block = block_cache
                    .read_mut(this_fat_block_num)
                    .map_err(Error::DeviceError)?;
                let entry = match new_value {
                    ClusterId::INVALID => 0x0FFF_FFF6,
                    ClusterId::BAD => 0x0FFF_FFF7,
                    ClusterId::EMPTY => 0x0000_0000,
                    _ => new_value.0,
                };
                let existing =
                    LittleEndian::read_u32(&block[this_fat_ent_offset..=this_fat_ent_offset + 3]);
                let new = (existing & 0xF000_0000) | (entry & 0x0FFF_FFFF);
                LittleEndian::write_u32(
                    &mut block[this_fat_ent_offset..=this_fat_ent_offset + 3],
                    new,
                );
            }
        }
        trace!("Updating FAT");
        if let Some(duplicate) = second_fat_block_num {
            block_cache.write_back_with_duplicate(duplicate)?;
        } else {
            block_cache.write_back()?;
        }
        Ok(())
    }

    /// Look in the FAT to see which cluster comes next.
    pub(crate) fn next_cluster<D>(
        &self,
        block_cache: &mut BlockCache<D>,
        cluster: ClusterId,
    ) -> Result<ClusterId, Error<D::Error>>
    where
        D: BlockDevice,
    {
        if cluster.0 > (u32::MAX / 4) {
            panic!("next_cluster called on invalid cluster {:x?}", cluster);
        }
        match &self.fat_specific_info {
            FatSpecificInfo::Fat16(_fat16_info) => {
                let fat_offset = cluster.0 * 2;
                let this_fat_block_num = self.lba_start + self.fat_start.offset_bytes(fat_offset);
                let this_fat_ent_offset = (fat_offset % Block::LEN_U32) as usize;
                trace!("Walking FAT");
                let block = block_cache.read(this_fat_block_num)?;
                let fat_entry =
                    LittleEndian::read_u16(&block[this_fat_ent_offset..=this_fat_ent_offset + 1]);
                match fat_entry {
                    0xFFF7 => {
                        // Bad cluster
                        Err(Error::BadCluster)
                    }
                    0xFFF8..=0xFFFF => {
                        // There is no next cluster
                        Err(Error::EndOfFile)
                    }
                    f => {
                        // Seems legit
                        Ok(ClusterId(u32::from(f)))
                    }
                }
            }
            FatSpecificInfo::Fat32(_fat32_info) => {
                let fat_offset = cluster.0 * 4;
                let this_fat_block_num = self.lba_start + self.fat_start.offset_bytes(fat_offset);
                let this_fat_ent_offset = (fat_offset % Block::LEN_U32) as usize;
                trace!("Walking FAT");
                let block = block_cache.read(this_fat_block_num)?;
                let fat_entry =
                    LittleEndian::read_u32(&block[this_fat_ent_offset..=this_fat_ent_offset + 3])
                        & 0x0FFF_FFFF;
                match fat_entry {
                    0x0000_0000 => {
                        // Jumped to free space
                        Err(Error::UnterminatedFatChain)
                    }
                    0x0FFF_FFF7 => {
                        // Bad cluster
                        Err(Error::BadCluster)
                    }
                    0x0000_0001 | 0x0FFF_FFF8..=0x0FFF_FFFF => {
                        // There is no next cluster
                        Err(Error::EndOfFile)
                    }
                    f => {
                        // Seems legit
                        Ok(ClusterId(f))
                    }
                }
            }
        }
    }

    /// Number of bytes in a cluster.
    pub(crate) fn bytes_per_cluster(&self) -> u32 {
        u32::from(self.blocks_per_cluster) * Block::LEN_U32
    }

    /// Converts a cluster number (or `Cluster`) to a block number (or
    /// `BlockIdx`). Gives an absolute `BlockIdx` you can pass to the
    /// volume manager.
    pub(crate) fn cluster_to_block(&self, cluster: ClusterId) -> BlockIdx {
        match &self.fat_specific_info {
            FatSpecificInfo::Fat16(fat16_info) => {
                let block_num = match cluster {
                    ClusterId::ROOT_DIR => fat16_info.first_root_dir_block,
                    ClusterId(c) => {
                        // FirstSectorofCluster = ((N – 2) * BPB_SecPerClus) + FirstDataSector;
                        let first_block_of_cluster =
                            BlockCount((c - 2) * u32::from(self.blocks_per_cluster));
                        self.first_data_block + first_block_of_cluster
                    }
                };
                self.lba_start + block_num
            }
            FatSpecificInfo::Fat32(fat32_info) => {
                let cluster_num = match cluster {
                    ClusterId::ROOT_DIR => fat32_info.first_root_dir_cluster.0,
                    c => c.0,
                };
                // FirstSectorofCluster = ((N – 2) * BPB_SecPerClus) + FirstDataSector;
                let first_block_of_cluster =
                    BlockCount((cluster_num - 2) * u32::from(self.blocks_per_cluster));
                self.lba_start + self.first_data_block + first_block_of_cluster
            }
        }
    }

    /// Finds a empty entry space and writes the new entry to it, allocates a new cluster if it's
    /// needed
    pub(crate) fn write_new_directory_entry<D, T>(
        &mut self,
        block_cache: &mut BlockCache<D>,
        time_source: &T,
        dir_cluster: ClusterId,
        name: ShortFileName,
        attributes: Attributes,
        first_cluster: ClusterId,
    ) -> Result<DirEntry, Error<D::Error>>
    where
        D: BlockDevice,
        T: TimeSource,
    {
        match &self.fat_specific_info {
            FatSpecificInfo::Fat16(fat16_info) => {
                // Root directories on FAT16 have a fixed size, because they use
                // a specially reserved space on disk (see
                // `first_root_dir_block`). Other directories can have any size
                // as they are made of regular clusters.
                let mut current_cluster = Some(dir_cluster);
                let mut first_dir_block_num = match dir_cluster {
                    ClusterId::ROOT_DIR => self.lba_start + fat16_info.first_root_dir_block,
                    _ => self.cluster_to_block(dir_cluster),
                };
                let dir_size = match dir_cluster {
                    ClusterId::ROOT_DIR => {
                        let len_bytes =
                            u32::from(fat16_info.root_entries_count) * OnDiskDirEntry::LEN_U32;
                        BlockCount::from_bytes(len_bytes)
                    }
                    _ => BlockCount(u32::from(self.blocks_per_cluster)),
                };

                // Walk the directory
                while let Some(cluster) = current_cluster {
                    for block_idx in first_dir_block_num.range(dir_size) {
                        trace!("Reading directory");
                        let block = block_cache
                            .read_mut(block_idx)
                            .map_err(Error::DeviceError)?;
                        for (i, dir_entry_bytes) in
                            block.chunks_exact_mut(OnDiskDirEntry::LEN).enumerate()
                        {
                            let dir_entry = OnDiskDirEntry::new(dir_entry_bytes);
                            // 0x00 or 0xE5 represents a free entry
                            if !dir_entry.is_valid() {
                                let ctime = time_source.get_timestamp();
                                let entry = DirEntry::new(
                                    name,
                                    attributes,
                                    first_cluster,
                                    ctime,
                                    block_idx,
                                    (i * OnDiskDirEntry::LEN) as u32,
                                );
                                dir_entry_bytes
                                    .copy_from_slice(&entry.serialize(FatType::Fat16)[..]);
                                trace!("Updating directory");
                                block_cache.write_back()?;
                                return Ok(entry);
                            }
                        }
                    }
                    if cluster != ClusterId::ROOT_DIR {
                        current_cluster = match self.next_cluster(block_cache, cluster) {
                            Ok(n) => {
                                first_dir_block_num = self.cluster_to_block(n);
                                Some(n)
                            }
                            Err(Error::EndOfFile) => {
                                let c = self.alloc_cluster(block_cache, Some(cluster), true)?;
                                first_dir_block_num = self.cluster_to_block(c);
                                Some(c)
                            }
                            // Running out of chain is what "the disk is full"
                            // means here. A chain we could not read says
                            // nothing about free space, and must not be
                            // reported as though it did.
                            Err(error) => return Err(error),
                        };
                    } else {
                        current_cluster = None;
                    }
                }
                Err(Error::NotEnoughSpace)
            }
            FatSpecificInfo::Fat32(fat32_info) => {
                // All directories on FAT32 have a cluster chain but the root
                // dir starts in a specified cluster.
                let mut current_cluster = match dir_cluster {
                    ClusterId::ROOT_DIR => Some(fat32_info.first_root_dir_cluster),
                    _ => Some(dir_cluster),
                };
                let mut first_dir_block_num = self.cluster_to_block(dir_cluster);

                let dir_size = BlockCount(u32::from(self.blocks_per_cluster));
                // Walk the cluster chain until we run out of clusters
                while let Some(cluster) = current_cluster {
                    // Loop through the blocks in the cluster
                    for block_idx in first_dir_block_num.range(dir_size) {
                        // Read a block of directory entries
                        trace!("Reading directory");
                        let block = block_cache
                            .read_mut(block_idx)
                            .map_err(Error::DeviceError)?;
                        // Are any entries in the block we just loaded blank? If so
                        // we can use them.
                        for (i, dir_entry_bytes) in
                            block.chunks_exact_mut(OnDiskDirEntry::LEN).enumerate()
                        {
                            let dir_entry = OnDiskDirEntry::new(dir_entry_bytes);
                            // 0x00 or 0xE5 represents a free entry
                            if !dir_entry.is_valid() {
                                let ctime = time_source.get_timestamp();
                                let entry = DirEntry::new(
                                    name,
                                    attributes,
                                    first_cluster,
                                    ctime,
                                    block_idx,
                                    (i * OnDiskDirEntry::LEN) as u32,
                                );
                                dir_entry_bytes
                                    .copy_from_slice(&entry.serialize(FatType::Fat32)[..]);
                                trace!("Updating directory");
                                block_cache.write_back()?;
                                return Ok(entry);
                            }
                        }
                    }
                    // Well none of the blocks in that cluster had any space in
                    // them, let's fetch another one.
                    current_cluster = match self.next_cluster(block_cache, cluster) {
                        Ok(n) => {
                            first_dir_block_num = self.cluster_to_block(n);
                            Some(n)
                        }
                        Err(Error::EndOfFile) => {
                            let c = self.alloc_cluster(block_cache, Some(cluster), true)?;
                            first_dir_block_num = self.cluster_to_block(c);
                            Some(c)
                        }
                        Err(error) => return Err(error),
                    };
                }
                // We ran out of clusters in the chain, and apparently we weren't
                // able to make the chain longer, so the disk must be full.
                Err(Error::NotEnoughSpace)
            }
        }
    }

    /// Write a VFAT long-file-name chain followed by its short-name entry.
    ///
    /// The caller supplies a unique 8.3 alias. Long-name entries are written
    /// first so an interrupted create cannot expose an SFN without its LFN.
    pub(crate) fn write_new_directory_entry_lfn<D, T>(
        &mut self,
        block_cache: &mut BlockCache<D>,
        time_source: &T,
        dir_cluster: ClusterId,
        long_name: &str,
        short_name: ShortFileName,
        attributes: Attributes,
    ) -> Result<DirEntry, Error<D::Error>>
    where
        D: BlockDevice,
        T: TimeSource,
    {
        let ctime = time_source.get_timestamp();
        self.write_directory_entry_lfn(
            block_cache,
            dir_cluster,
            long_name,
            short_name,
            attributes,
            ClusterId::EMPTY,
            0,
            ctime,
            ctime,
        )
    }

    /// Write a long-named directory entry describing a cluster chain that
    /// already exists.
    ///
    /// This is half of a same-volume move: it gives an existing chain a second
    /// name, and [`Self::delete_directory_entry`] then takes the first one
    /// away without touching the chain itself. Between those two writes the
    /// chain has two names, and a caller that crashes there must unlink one of
    /// them rather than delete it -- deleting reclaims clusters that both
    /// names still point at.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn write_linked_directory_entry_lfn<D>(
        &mut self,
        block_cache: &mut BlockCache<D>,
        dir_cluster: ClusterId,
        long_name: &str,
        short_name: ShortFileName,
        source: &DirEntry,
    ) -> Result<DirEntry, Error<D::Error>>
    where
        D: BlockDevice,
    {
        self.write_directory_entry_lfn(
            block_cache,
            dir_cluster,
            long_name,
            short_name,
            source.attributes,
            source.cluster,
            source.size,
            source.ctime,
            source.mtime,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn write_directory_entry_lfn<D>(
        &mut self,
        block_cache: &mut BlockCache<D>,
        dir_cluster: ClusterId,
        long_name: &str,
        short_name: ShortFileName,
        attributes: Attributes,
        cluster: ClusterId,
        size: u32,
        ctime: Timestamp,
        mtime: Timestamp,
    ) -> Result<DirEntry, Error<D::Error>>
    where
        D: BlockDevice,
    {
        let utf16_len = validate_long_filename(long_name).map_err(Error::FilenameError)?;
        let lfn_count = utf16_len.div_ceil(LFN_CHARS_PER_ENTRY);
        let slots_needed = lfn_count + 1;
        let slots = self.find_free_directory_slots(block_cache, dir_cluster, slots_needed)?;

        let mut utf16 = [0u16; 255];
        for (dst, unit) in utf16.iter_mut().zip(long_name.encode_utf16()) {
            *dst = unit;
        }

        let checksum = short_name.csum();
        let mut written = 0;
        for disk_index in 0..lfn_count {
            let sequence = lfn_count - disk_index;
            let start = (sequence - 1) * LFN_CHARS_PER_ENTRY;
            let end = (start + LFN_CHARS_PER_ENTRY).min(utf16_len);
            let raw = serialize_lfn_entry(
                sequence as u8,
                disk_index == 0,
                checksum,
                &utf16[start..end],
            );
            if let Err(error) = write_directory_slot(block_cache, slots[disk_index], &raw) {
                let _ = mark_directory_slots_deleted(block_cache, &slots[..written]);
                return Err(error);
            }
            written += 1;
        }

        let short_slot = slots[lfn_count];
        let mut entry = DirEntry::new(
            short_name,
            attributes,
            cluster,
            ctime,
            short_slot.block,
            short_slot.offset,
        );
        entry.mtime = mtime;
        entry.size = size;
        let raw = entry.serialize(self.get_fat_type());
        if let Err(error) = write_directory_slot(block_cache, short_slot, &raw) {
            let _ = mark_directory_slots_deleted(block_cache, &slots[..written]);
            return Err(error);
        }
        Ok(entry)
    }

    fn find_free_directory_slots<D>(
        &mut self,
        block_cache: &mut BlockCache<D>,
        dir_cluster: ClusterId,
        slots_needed: usize,
    ) -> Result<Vec<DirectorySlot, MAX_LFN_SLOTS>, Error<D::Error>>
    where
        D: BlockDevice,
    {
        if slots_needed == 0 || slots_needed > MAX_LFN_SLOTS {
            return Err(Error::NotEnoughSpace);
        }

        let (mut current_cluster, root_block, blocks_per_step, fixed_root) =
            match &self.fat_specific_info {
                FatSpecificInfo::Fat16(info) if dir_cluster == ClusterId::ROOT_DIR => {
                    let bytes = u32::from(info.root_entries_count) * OnDiskDirEntry::LEN_U32;
                    (
                        ClusterId::ROOT_DIR,
                        Some(self.lba_start + info.first_root_dir_block),
                        BlockCount::from_bytes(bytes),
                        true,
                    )
                }
                FatSpecificInfo::Fat16(_) => (
                    dir_cluster,
                    None,
                    BlockCount(u32::from(self.blocks_per_cluster)),
                    false,
                ),
                FatSpecificInfo::Fat32(info) => (
                    if dir_cluster == ClusterId::ROOT_DIR {
                        info.first_root_dir_cluster
                    } else {
                        dir_cluster
                    },
                    None,
                    BlockCount(u32::from(self.blocks_per_cluster)),
                    false,
                ),
            };

        let mut slots = Vec::<DirectorySlot, MAX_LFN_SLOTS>::new();
        loop {
            let first_block = root_block.unwrap_or_else(|| self.cluster_to_block(current_cluster));
            for block_idx in first_block.range(blocks_per_step) {
                let block = block_cache.read(block_idx).map_err(Error::DeviceError)?;
                for (index, bytes) in block.chunks_exact(OnDiskDirEntry::LEN).enumerate() {
                    let entry = OnDiskDirEntry::new(bytes);
                    if entry.is_valid() {
                        slots.clear();
                    } else {
                        let _ = slots.push(DirectorySlot {
                            block: block_idx,
                            offset: (index * OnDiskDirEntry::LEN) as u32,
                        });
                        if slots.len() == slots_needed {
                            return Ok(slots);
                        }
                    }
                }
            }

            if fixed_root {
                return Err(Error::NotEnoughSpace);
            }
            current_cluster = match self.next_cluster(block_cache, current_cluster) {
                Ok(next) => next,
                Err(Error::EndOfFile) => {
                    self.alloc_cluster(block_cache, Some(current_cluster), true)?
                }
                Err(error) => return Err(error),
            };
        }
    }

    /// Calls callback `func` with every valid entry in the given directory.
    /// Useful for performing directory listings.
    pub(crate) fn iterate_dir<D, F>(
        &self,
        block_cache: &mut BlockCache<D>,
        dir_info: &DirectoryInfo,
        mut func: F,
    ) -> Result<(), Error<D::Error>>
    where
        F: FnMut(&DirEntry) -> ControlFlow<()>,
        D: BlockDevice,
    {
        match &self.fat_specific_info {
            FatSpecificInfo::Fat16(fat16_info) => {
                self.iterate_fat16(dir_info, fat16_info, block_cache, |de, _| func(de))
            }
            FatSpecificInfo::Fat32(fat32_info) => {
                self.iterate_fat32(dir_info, fat32_info, block_cache, |de, _| func(de))
            }
        }
    }

    /// Calls callback `func` with every valid entry in the given directory, plus its ODDE.
    fn iterate_dir_internal<D, F>(
        &self,
        block_cache: &mut BlockCache<D>,
        dir_info: &DirectoryInfo,
        func: F,
    ) -> Result<(), Error<D::Error>>
    where
        F: FnMut(&DirEntry, &OnDiskDirEntry) -> ControlFlow<()>,
        D: BlockDevice,
    {
        match &self.fat_specific_info {
            FatSpecificInfo::Fat16(fat16_info) => {
                self.iterate_fat16(dir_info, fat16_info, block_cache, func)
            }
            FatSpecificInfo::Fat32(fat32_info) => {
                self.iterate_fat32(dir_info, fat32_info, block_cache, func)
            }
        }
    }

    /// Calls callback `func` with every valid entry in the given directory,
    /// including the Long File Name.
    ///
    /// Useful for performing directory listings.
    pub(crate) fn iterate_dir_lfn<D, F>(
        &self,
        block_cache: &mut BlockCache<D>,
        lfn_buffer: &mut LfnBuffer<'_>,
        dir_info: &DirectoryInfo,
        mut func: F,
    ) -> Result<(), Error<D::Error>>
    where
        F: FnMut(&DirEntry, Option<&str>) -> ControlFlow<()>,
        D: BlockDevice,
    {
        #[derive(Clone, Copy)]
        enum SeqState {
            Waiting,
            Remaining { csum: u8, next: u8 },
            Complete { csum: u8 },
        }

        impl SeqState {
            fn update(
                self,
                lfn_buffer: &mut LfnBuffer<'_>,
                start: bool,
                sequence: u8,
                csum: u8,
                buffer: [u16; 13],
            ) -> Self {
                #[cfg(feature = "log")]
                debug!("LFN Contents {start} {sequence} {csum:02x} {buffer:04x?}");
                #[cfg(feature = "defmt-log")]
                debug!(
                    "LFN Contents {=bool} {=u8} {=u8:02x} {=[?; 13]:#04x}",
                    start, sequence, csum, buffer
                );
                match (start, sequence, self) {
                    (true, 0x01, _) => {
                        lfn_buffer.clear();
                        lfn_buffer.push(&buffer);
                        SeqState::Complete { csum }
                    }
                    // The first entry of a chain carries the highest sequence
                    // number, so this bound is what limits how long a name can
                    // be listed -- it has to reach whatever creation is willing
                    // to write.
                    (true, sequence, _)
                        if (0x02..=MAX_LFN_ENTRIES as u8).contains(&sequence) =>
                    {
                        lfn_buffer.clear();
                        lfn_buffer.push(&buffer);
                        SeqState::Remaining {
                            csum,
                            next: sequence - 1,
                        }
                    }
                    (false, 0x01, SeqState::Remaining { csum, next }) if next == sequence => {
                        lfn_buffer.push(&buffer);
                        SeqState::Complete { csum }
                    }
                    (false, sequence, SeqState::Remaining { csum, next })
                        if (0x01..MAX_LFN_ENTRIES as u8).contains(&sequence)
                            && next == sequence =>
                    {
                        lfn_buffer.push(&buffer);
                        SeqState::Remaining {
                            csum,
                            next: sequence - 1,
                        }
                    }
                    _ => {
                        // this seems wrong
                        lfn_buffer.clear();
                        SeqState::Waiting
                    }
                }
            }
        }

        let mut seq_state = SeqState::Waiting;
        self.iterate_dir_internal(block_cache, dir_info, |de, odde| {
            if let Some((start, this_seqno, csum, buffer)) = odde.lfn_contents() {
                seq_state = seq_state.update(lfn_buffer, start, this_seqno, csum, buffer);
                ControlFlow::Continue(())
            } else if let SeqState::Complete { csum } = seq_state {
                if csum == de.name.csum() {
                    // Checksum is good, and all the pieces are there
                    func(de, Some(lfn_buffer.as_str()))
                } else {
                    // Checksum was bad
                    func(de, None)
                }
            } else {
                func(de, None)
            }
        })
    }

    /// Calls callback `func` with every valid entry in the given FAT16 directory.
    ///
    /// Useful for performing directory listings.
    fn iterate_fat16<D, F>(
        &self,
        dir_info: &DirectoryInfo,
        fat16_info: &Fat16Info,
        block_cache: &mut BlockCache<D>,
        mut func: F,
    ) -> Result<(), Error<D::Error>>
    where
        F: for<'odde> FnMut(&DirEntry, &OnDiskDirEntry<'odde>) -> ControlFlow<()>,
        D: BlockDevice,
    {
        // Root directories on FAT16 have a fixed size, because they use
        // a specially reserved space on disk (see
        // `first_root_dir_block`). Other directories can have any size
        // as they are made of regular clusters.
        let mut current_cluster = Some(dir_info.cluster);
        let mut first_dir_block_num = match dir_info.cluster {
            ClusterId::ROOT_DIR => self.lba_start + fat16_info.first_root_dir_block,
            _ => self.cluster_to_block(dir_info.cluster),
        };
        let dir_size = match dir_info.cluster {
            ClusterId::ROOT_DIR => {
                let len_bytes = u32::from(fat16_info.root_entries_count) * OnDiskDirEntry::LEN_U32;
                BlockCount::from_bytes(len_bytes)
            }
            _ => BlockCount(u32::from(self.blocks_per_cluster)),
        };

        'outer: while let Some(cluster) = current_cluster {
            for block_idx in first_dir_block_num.range(dir_size) {
                trace!("Reading FAT");
                let block = block_cache.read(block_idx)?;
                for (i, dir_entry_bytes) in block.chunks_exact(OnDiskDirEntry::LEN).enumerate() {
                    let dir_entry = OnDiskDirEntry::new(dir_entry_bytes);
                    if dir_entry.is_end() {
                        // Can quit early
                        break 'outer;
                    } else if dir_entry.is_valid() {
                        // Safe, since Block::LEN always fits on a u32
                        let start = (i * OnDiskDirEntry::LEN) as u32;
                        let entry = dir_entry.get_entry(FatType::Fat16, block_idx, start);
                        if func(&entry, &dir_entry) == ControlFlow::Break(()) {
                            break 'outer;
                        }
                    }
                }
            }
            if cluster != ClusterId::ROOT_DIR {
                current_cluster = match self.next_cluster(block_cache, cluster) {
                    Ok(n) => {
                        first_dir_block_num = self.cluster_to_block(n);
                        Some(n)
                    }
                    // The chain ending is what "no more entries" means; a
                    // chain we could not follow is not the same answer, and
                    // callers that treat NotFound as proof of absence need
                    // to be able to tell those apart.
                    Err(Error::EndOfFile) => None,
                    Err(error) => return Err(error),
                };
            } else {
                current_cluster = None;
            }
        }
        Ok(())
    }

    /// Calls callback `func` with every valid entry in the given FAT32 directory.
    ///
    /// Useful for performing directory listings.
    fn iterate_fat32<D, F>(
        &self,
        dir_info: &DirectoryInfo,
        fat32_info: &Fat32Info,
        block_cache: &mut BlockCache<D>,
        mut func: F,
    ) -> Result<(), Error<D::Error>>
    where
        F: for<'odde> FnMut(&DirEntry, &OnDiskDirEntry<'odde>) -> ControlFlow<()>,
        D: BlockDevice,
    {
        // All directories on FAT32 have a cluster chain but the root
        // dir starts in a specified cluster.
        let mut current_cluster = match dir_info.cluster {
            ClusterId::ROOT_DIR => Some(fat32_info.first_root_dir_cluster),
            _ => Some(dir_info.cluster),
        };
        'outer: while let Some(cluster) = current_cluster {
            let start_block_idx = self.cluster_to_block(cluster);
            for block_idx in start_block_idx.range(BlockCount(u32::from(self.blocks_per_cluster))) {
                trace!("Reading FAT");
                let block = block_cache.read(block_idx).map_err(Error::DeviceError)?;
                for (i, dir_entry_bytes) in block.chunks_exact(OnDiskDirEntry::LEN).enumerate() {
                    let dir_entry = OnDiskDirEntry::new(dir_entry_bytes);
                    if dir_entry.is_end() {
                        // Can quit early
                        break 'outer;
                    } else if dir_entry.is_valid() {
                        // Safe, since Block::LEN always fits on a u32
                        let start = (i * OnDiskDirEntry::LEN) as u32;
                        let entry = dir_entry.get_entry(FatType::Fat32, block_idx, start);
                        if let ControlFlow::Break(_) = func(&entry, &dir_entry) {
                            // Can quit early
                            break 'outer;
                        }
                    }
                }
            }
            current_cluster = match self.next_cluster(block_cache, cluster) {
                Ok(n) => Some(n),
                Err(Error::EndOfFile) => None,
                Err(error) => return Err(error),
            };
        }
        Ok(())
    }

    /// Get an entry from the given directory
    pub(crate) fn find_directory_entry<D>(
        &self,
        block_cache: &mut BlockCache<D>,
        dir_info: &DirectoryInfo,
        match_name: &ShortFileName,
    ) -> Result<DirEntry, Error<D::Error>>
    where
        D: BlockDevice,
    {
        let mut result = Err(Error::NotFound);
        self.iterate_dir(block_cache, dir_info, |de| {
            if de.name == *match_name {
                result = Ok(de.clone());
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        })?;
        result
    }

    /// Get an entry from the given directory
    pub(crate) fn find_directory_entry_by_lfn<D>(
        &self,
        block_cache: &mut BlockCache<D>,
        dir_info: &DirectoryInfo,
        match_name: &str,
    ) -> Result<DirEntry, Error<D::Error>>
    where
        D: BlockDevice,
    {
        self.find_directory_entry_by_lfn_inner(block_cache, dir_info, match_name, false)
    }

    /// Work out the 8.3 alias a long name will be filed under.
    ///
    /// Every FAT entry has a short name whether or not it has a long one, and
    /// a reader that predates long names sees only the short one. It is not
    /// part of what the caller asked for, so the caller does not supply it: it
    /// has to be unique within the directory, and the directory is the only
    /// thing that can decide that.
    ///
    /// The alias is derived from the long name so it stays recognisable --
    /// upper-cased, spaces and interior dots removed, anything outside ASCII
    /// or outside what 8.3 permits replaced with `_` -- and then given a `~1`
    /// to `~4` tail until nothing in the directory answers to it. The base is
    /// shortened to make room, so `A Real Book.epub` becomes `AREALB~1.EPU`.
    ///
    /// Past `~4` readability stops being worth the search, and the tail
    /// becomes two characters of base followed by four hex digits of a digest
    /// of the long name. That is 65536 candidates rather than the 999 a
    /// decimal tail allows, and it almost always settles on the first, which
    /// matters for a directory expected to hold thousands of files whose names
    /// share an opening.
    ///
    /// Gives up with [`Error::FileAlreadyExists`] only once that space is
    /// exhausted too.
    pub(crate) fn generate_short_alias<D>(
        &self,
        block_cache: &mut BlockCache<D>,
        dir_info: &DirectoryInfo,
        long_name: &str,
    ) -> Result<ShortFileName, Error<D::Error>>
    where
        D: BlockDevice,
    {
        let (base, ext) = short_name_basis(long_name);

        // The readable forms first, so an alias glanced at on a computer still
        // resembles the file it belongs to.
        for tail in 1..=4u32 {
            let candidate = compose_alias(&base, &ext, tail);
            if let Some(sfn) = self.claim_alias(block_cache, dir_info, &candidate)? {
                return Ok(sfn);
            }
        }

        // Then spread out. Starting from the name's own digest means two
        // different names rarely probe the same candidate first.
        let digest = long_name_digest(long_name);
        for probe in 0..=u16::MAX {
            let candidate = compose_hashed_alias(&base, &ext, digest.wrapping_add(probe));
            if let Some(sfn) = self.claim_alias(block_cache, dir_info, &candidate)? {
                return Ok(sfn);
            }
        }
        Err(Error::FileAlreadyExists)
    }

    /// Take `candidate` as an alias if the directory has nothing by that name.
    fn claim_alias<D>(
        &self,
        block_cache: &mut BlockCache<D>,
        dir_info: &DirectoryInfo,
        candidate: &str,
    ) -> Result<Option<ShortFileName>, Error<D::Error>>
    where
        D: BlockDevice,
    {
        // Built from ASCII by construction, so a parse failure would be a bug
        // here rather than anything the caller did.
        let Ok(sfn) = ShortFileName::create_from_str(candidate) else {
            return Err(Error::FilenameError(FilenameError::InvalidCharacter));
        };
        if self.name_is_taken(block_cache, dir_info, candidate)? {
            return Ok(None);
        }
        Ok(Some(sfn))
    }

    /// Is `name` already taken in this directory?
    ///
    /// FAT gives a directory one namespace, not two: an entry's long name and
    /// its short name both live in it, and names that differ only by case are
    /// the same name. Every name a new entry will answer to has to be checked
    /// against both halves of it -- the long name *and* the short alias, since
    /// an alias can equally well collide with some existing entry's long name.
    /// Otherwise a directory ends up with two entries answering to one name,
    /// and a lookup resolves to whichever comes first on disk.
    pub(crate) fn name_is_taken<D>(
        &self,
        block_cache: &mut BlockCache<D>,
        dir_info: &DirectoryInfo,
        name: &str,
    ) -> Result<bool, Error<D::Error>>
    where
        D: BlockDevice,
    {
        // Against existing long names, ignoring case.
        match self.find_directory_entry_by_lfn_inner(block_cache, dir_info, name, true) {
            Ok(_) => return Ok(true),
            Err(Error::NotFound) => {}
            Err(error) => return Err(error),
        }

        // Against existing short names. Only a name that is itself 8.3-shaped
        // can collide with one, and ShortFileName comparison is already
        // case-insensitive because it stores the upper-cased form.
        if let Ok(as_short) = ShortFileName::create_from_str(name) {
            match self.find_directory_entry(block_cache, dir_info, &as_short) {
                Ok(_) => return Ok(true),
                Err(Error::NotFound) => {}
                Err(error) => return Err(error),
            }
        }

        Ok(false)
    }

    fn find_directory_entry_by_lfn_inner<D>(
        &self,
        block_cache: &mut BlockCache<D>,
        dir_info: &DirectoryInfo,
        match_name: &str,
        fold_case: bool,
    ) -> Result<DirEntry, Error<D::Error>>
    where
        D: BlockDevice,
    {
        let mut result = Err(Error::NotFound);
        enum SeqState<'a> {
            /// Looking for the first entry in an LFN sequence
            Waiting,
            /// Scanning through an LFN sequence
            Scanning {
                remaining: &'a str,
                sequence: u8,
                csum: u8,
            },
            /// Found an entry we like
            Found { csum: u8 },
        }

        let mut state = SeqState::Waiting;
        // Half of a surrogate pair that straddles the boundary between two
        // entries, waiting for the half that completes it.
        let mut pending_low: Option<u16> = None;
        self.iterate_dir_internal(block_cache, dir_info, |de, odde| {
            match state {
                SeqState::Waiting => {
                    debug!("Am waiting for LFN start");
                    let mut remaining = match_name;
                    if let Some((true, sequence, csum, buffer)) = odde.lfn_contents() {
                        #[cfg(feature = "defmt-log")]
                        debug!("{:02x} {:02x} {:04x}", sequence, csum, buffer);
                        #[cfg(feature = "log")]
                        debug!("{:02x} {:02x} {:04x?}", sequence, csum, buffer);
                        // trim padding and NUL words off the end of the file name (which is the part that comes first)
                        pending_low = None;
                        if !strip_lfn_words(
                            buffer
                                .iter()
                                .rev()
                                .skip_while(|b| **b == 0xFFFF)
                                .skip_while(|b| **b == 0x0000)
                                .copied(),
                            &mut remaining,
                            &mut pending_low,
                            fold_case,
                        ) {
                            debug!("No, didn't want that");
                            return ControlFlow::Continue(());
                        }
                        if sequence == 1 {
                            // last piece
                            if remaining.is_empty() {
                                // found it
                                state = SeqState::Found { csum }
                            } else {
                                // no, we have characters left over
                                state = SeqState::Waiting
                            }
                        } else {
                            // keep looking
                            state = SeqState::Scanning {
                                remaining,
                                sequence: sequence - 1,
                                csum,
                            };
                        }
                    }
                }
                SeqState::Scanning {
                    remaining,
                    sequence,
                    csum,
                } => {
                    debug!(
                        "Am waiting for more LFN sequence={:02x}, csum={:02x}",
                        sequence, csum
                    );
                    let mut remaining = remaining;
                    if let Some((false, this_sequence, this_csum, buffer)) = odde.lfn_contents() {
                        #[cfg(feature = "defmt-log")]
                        debug!("{:02x} {:02x} {:04x}", sequence, csum, buffer);
                        #[cfg(feature = "log")]
                        debug!("{:02x} {:02x} {:04x?}", sequence, csum, buffer);
                        if (this_sequence != sequence) || (this_csum != csum) {
                            // not what we wanted
                            debug!(
                                "No! Got sequence={:02x}, csum={:02x}",
                                this_sequence, this_csum
                            );
                            state = SeqState::Waiting;
                            return ControlFlow::Continue(());
                        }
                        if !strip_lfn_words(
                            buffer.iter().rev().copied(),
                            &mut remaining,
                            &mut pending_low,
                            fold_case,
                        ) {
                            debug!("No, didn't want that");
                            return ControlFlow::Continue(());
                        }
                        if sequence == 1 {
                            // last piece
                            if remaining.is_empty() {
                                // found it
                                state = SeqState::Found { csum }
                            } else {
                                // no, we have characters left over
                                state = SeqState::Waiting
                            }
                        } else {
                            // keep looking
                            state = SeqState::Scanning {
                                remaining,
                                sequence: sequence - 1,
                                csum,
                            };
                        }
                    }
                }
                SeqState::Found { csum } => {
                    let calc_csum = de.name.csum();
                    if calc_csum == csum {
                        result = Ok(de.clone());
                        return ControlFlow::Break(());
                    } else {
                        debug!("Bad csum {:02x} != {:02x}", calc_csum, csum);
                    }
                }
            }
            ControlFlow::Continue(())
        })?;
        result
    }

    /// Delete an entry from the given directory
    pub(crate) fn delete_directory_entry<D>(
        &self,
        block_cache: &mut BlockCache<D>,
        dir_info: &DirectoryInfo,
        match_name: &ShortFileName,
    ) -> Result<(), Error<D::Error>>
    where
        D: BlockDevice,
    {
        let slots = self.find_directory_entry_slots(block_cache, dir_info, match_name)?;
        mark_directory_slots_deleted(block_cache, &slots)
    }

    fn find_directory_entry_slots<D>(
        &self,
        block_cache: &mut BlockCache<D>,
        dir_info: &DirectoryInfo,
        match_name: &ShortFileName,
    ) -> Result<Vec<DirectorySlot, MAX_LFN_SLOTS>, Error<D::Error>>
    where
        D: BlockDevice,
    {
        let (mut current_cluster, root_block, blocks_per_step, fixed_root) =
            match &self.fat_specific_info {
                FatSpecificInfo::Fat16(info) if dir_info.cluster == ClusterId::ROOT_DIR => {
                    let bytes = u32::from(info.root_entries_count) * OnDiskDirEntry::LEN_U32;
                    (
                        ClusterId::ROOT_DIR,
                        Some(self.lba_start + info.first_root_dir_block),
                        BlockCount::from_bytes(bytes),
                        true,
                    )
                }
                FatSpecificInfo::Fat16(_) => (
                    dir_info.cluster,
                    None,
                    BlockCount(u32::from(self.blocks_per_cluster)),
                    false,
                ),
                FatSpecificInfo::Fat32(info) => (
                    if dir_info.cluster == ClusterId::ROOT_DIR {
                        info.first_root_dir_cluster
                    } else {
                        dir_info.cluster
                    },
                    None,
                    BlockCount(u32::from(self.blocks_per_cluster)),
                    false,
                ),
            };

        let mut pending = PendingLfnSlots::new();
        loop {
            let first_block = root_block.unwrap_or_else(|| self.cluster_to_block(current_cluster));
            for block_idx in first_block.range(blocks_per_step) {
                let block = block_cache.read(block_idx).map_err(Error::DeviceError)?;
                for (index, bytes) in block.chunks_exact(OnDiskDirEntry::LEN).enumerate() {
                    let entry = OnDiskDirEntry::new(bytes);
                    if entry.is_end() {
                        return Err(Error::NotFound);
                    }
                    if !entry.is_valid() {
                        pending.clear();
                        continue;
                    }
                    let slot = DirectorySlot {
                        block: block_idx,
                        offset: (index * OnDiskDirEntry::LEN) as u32,
                    };
                    if entry.is_lfn() {
                        pending.observe(&entry, slot);
                        continue;
                    }
                    if entry.matches(match_name) {
                        let mut slots = Vec::<DirectorySlot, MAX_LFN_SLOTS>::new();
                        if pending.belongs_to(match_name) {
                            for lfn_slot in pending.slots.iter().copied() {
                                let _ = slots.push(lfn_slot);
                            }
                        }
                        let _ = slots.push(slot);
                        return Ok(slots);
                    }
                    pending.clear();
                }
            }

            if fixed_root {
                return Err(Error::NotFound);
            }
            current_cluster = match self.next_cluster(block_cache, current_cluster) {
                Ok(next) => next,
                Err(Error::EndOfFile) => return Err(Error::NotFound),
                Err(error) => return Err(error),
            };
        }
    }

    /// Finds the next free cluster after the start_cluster and before end_cluster
    pub(crate) fn find_next_free_cluster<D>(
        &self,
        block_cache: &mut BlockCache<D>,
        start_cluster: ClusterId,
        end_cluster: ClusterId,
    ) -> Result<ClusterId, Error<D::Error>>
    where
        D: BlockDevice,
    {
        let mut current_cluster = start_cluster;
        match &self.fat_specific_info {
            FatSpecificInfo::Fat16(_fat16_info) => {
                while current_cluster.0 < end_cluster.0 {
                    trace!(
                        "current_cluster={:?}, end_cluster={:?}",
                        current_cluster, end_cluster
                    );
                    let fat_offset = current_cluster.0 * 2;
                    trace!("fat_offset = {:?}", fat_offset);
                    let this_fat_block_num =
                        self.lba_start + self.fat_start.offset_bytes(fat_offset);
                    trace!("this_fat_block_num = {:?}", this_fat_block_num);
                    let mut this_fat_ent_offset = usize::try_from(fat_offset % Block::LEN_U32)
                        .map_err(|_| Error::ConversionError)?;
                    trace!("Reading block {:?}", this_fat_block_num);
                    let block = block_cache
                        .read(this_fat_block_num)
                        .map_err(Error::DeviceError)?;
                    while this_fat_ent_offset <= Block::LEN - 2 {
                        let fat_entry = LittleEndian::read_u16(
                            &block[this_fat_ent_offset..=this_fat_ent_offset + 1],
                        );
                        if fat_entry == 0 {
                            return Ok(current_cluster);
                        }
                        this_fat_ent_offset += 2;
                        current_cluster += 1;
                    }
                }
            }
            FatSpecificInfo::Fat32(_fat32_info) => {
                while current_cluster.0 < end_cluster.0 {
                    trace!(
                        "current_cluster={:?}, end_cluster={:?}",
                        current_cluster, end_cluster
                    );
                    let fat_offset = current_cluster.0 * 4;
                    trace!("fat_offset = {:?}", fat_offset);
                    let this_fat_block_num =
                        self.lba_start + self.fat_start.offset_bytes(fat_offset);
                    trace!("this_fat_block_num = {:?}", this_fat_block_num);
                    let mut this_fat_ent_offset = usize::try_from(fat_offset % Block::LEN_U32)
                        .map_err(|_| Error::ConversionError)?;
                    trace!("Reading block {:?}", this_fat_block_num);
                    let block = block_cache
                        .read(this_fat_block_num)
                        .map_err(Error::DeviceError)?;
                    while this_fat_ent_offset <= Block::LEN - 4 {
                        let fat_entry = LittleEndian::read_u32(
                            &block[this_fat_ent_offset..=this_fat_ent_offset + 3],
                        ) & 0x0FFF_FFFF;
                        if fat_entry == 0 {
                            return Ok(current_cluster);
                        }
                        this_fat_ent_offset += 4;
                        current_cluster += 1;
                    }
                }
            }
        }
        warn!("Out of space...");
        Err(Error::NotEnoughSpace)
    }

    /// Tries to allocate a cluster
    pub(crate) fn alloc_cluster<D>(
        &mut self,
        block_cache: &mut BlockCache<D>,
        prev_cluster: Option<ClusterId>,
        zero: bool,
    ) -> Result<ClusterId, Error<D::Error>>
    where
        D: BlockDevice,
    {
        debug!("Allocating new cluster, prev_cluster={:?}", prev_cluster);
        let end_cluster = ClusterId(self.cluster_count + RESERVED_ENTRIES);
        let start_cluster = match self.next_free_cluster {
            Some(cluster) if cluster.0 < end_cluster.0 => cluster,
            _ => ClusterId(RESERVED_ENTRIES),
        };
        trace!(
            "Finding next free between {:?}..={:?}",
            start_cluster, end_cluster
        );
        let new_cluster = match self.find_next_free_cluster(block_cache, start_cluster, end_cluster)
        {
            Ok(cluster) => cluster,
            // Only "nothing free above the hint" is a reason to wrap around and
            // look below it. A read that failed has not searched that range, so
            // wrapping on it would let a working lower range hide the failure
            // completely.
            Err(Error::NotEnoughSpace) if start_cluster.0 > RESERVED_ENTRIES => {
                debug!(
                    "Retrying, finding next free between {:?}..={:?}",
                    ClusterId(RESERVED_ENTRIES),
                    end_cluster
                );
                self.find_next_free_cluster(block_cache, ClusterId(RESERVED_ENTRIES), end_cluster)?
            }
            Err(e) => return Err(e),
        };
        // This new cluster is the end of the file's chain
        self.update_fat(block_cache, new_cluster, ClusterId::END_OF_FILE)?;
        // If there's something before this new one, update the FAT to point it at us
        if let Some(cluster) = prev_cluster {
            trace!(
                "Updating old cluster {:?} to {:?} in FAT",
                cluster, new_cluster
            );
            self.update_fat(block_cache, cluster, new_cluster)?;
        }
        trace!(
            "Finding next free between {:?}..={:?}",
            new_cluster, end_cluster
        );
        // The cluster is now claimed in the FAT and linked to its predecessor,
        // so the allocation has happened whatever follows. What is left is
        // finding where the *next* search should start, which is a hint and
        // nothing more.
        //
        // Failing that search must not turn a completed allocation into an
        // error. Claiming the last free cluster on the volume makes it fail
        // every time -- there is genuinely nothing after it -- and reporting
        // that would strand the cluster we just linked: callers allocating a
        // file's first cluster never get to record it in the directory entry,
        // and a directory being extended would keep a cluster that the zeroing
        // below never reached, leaving whatever used to be there readable as
        // directory entries.
        //
        // So the hint is best-effort. `None` means "start from the beginning
        // next time", which is always safe, just slower. A device error here
        // is dropped rather than reported, because the operation the caller
        // asked for did succeed; anything genuinely wrong with the card will
        // resurface on the next read or write that depends on it.
        self.next_free_cluster =
            match self.find_next_free_cluster(block_cache, new_cluster, end_cluster) {
                Ok(cluster) => Some(cluster),
                Err(_) if new_cluster.0 > RESERVED_ENTRIES => self
                    .find_next_free_cluster(block_cache, ClusterId(RESERVED_ENTRIES), end_cluster)
                    .ok(),
                Err(_) => None,
            };
        debug!("Next free cluster is {:?}", self.next_free_cluster);
        // Record that we've allocated a cluster
        if let Some(ref mut number_free_cluster) = self.free_clusters_count {
            *number_free_cluster -= 1;
        };
        if zero {
            let start_block_idx = self.cluster_to_block(new_cluster);
            let num_blocks = BlockCount(u32::from(self.blocks_per_cluster));
            for block_idx in start_block_idx.range(num_blocks) {
                trace!("Zeroing cluster {:?}", block_idx);
                let _block = block_cache.blank_mut(block_idx);
                block_cache.write_back()?;
            }
        }
        debug!("All done, returning {:?}", new_cluster);
        Ok(new_cluster)
    }

    /// Marks the input cluster as an EOF and all the subsequent clusters in the chain as free
    pub(crate) fn truncate_cluster_chain<D>(
        &mut self,
        block_cache: &mut BlockCache<D>,
        cluster: ClusterId,
    ) -> Result<(), Error<D::Error>>
    where
        D: BlockDevice,
    {
        if cluster.0 < RESERVED_ENTRIES {
            // file doesn't have any valid cluster allocated, there is nothing to do
            return Ok(());
        }
        let mut next = {
            match self.next_cluster(block_cache, cluster) {
                Ok(n) => n,
                Err(Error::EndOfFile) => return Ok(()),
                Err(e) => return Err(e),
            }
        };
        if let Some(ref mut next_free_cluster) = self.next_free_cluster {
            if next_free_cluster.0 > next.0 {
                *next_free_cluster = next;
            }
        } else {
            self.next_free_cluster = Some(next);
        }
        self.update_fat(block_cache, cluster, ClusterId::END_OF_FILE)?;
        loop {
            match self.next_cluster(block_cache, next) {
                Ok(n) => {
                    self.update_fat(block_cache, next, ClusterId::EMPTY)?;
                    next = n;
                }
                Err(Error::EndOfFile) => {
                    self.update_fat(block_cache, next, ClusterId::EMPTY)?;
                    break;
                }
                Err(e) => return Err(e),
            }
            if let Some(ref mut number_free_cluster) = self.free_clusters_count {
                *number_free_cluster += 1;
            };
        }
        Ok(())
    }

    /// Writes a Directory Entry to the disk
    pub(crate) fn write_entry_to_disk<D>(
        &self,
        block_cache: &mut BlockCache<D>,
        entry: &DirEntry,
    ) -> Result<(), Error<D::Error>>
    where
        D: BlockDevice,
    {
        let fat_type = match self.fat_specific_info {
            FatSpecificInfo::Fat16(_) => FatType::Fat16,
            FatSpecificInfo::Fat32(_) => FatType::Fat32,
        };
        trace!("Reading directory for update");
        let block = block_cache
            .read_mut(entry.entry_block)
            .map_err(Error::DeviceError)?;

        let start = usize::try_from(entry.entry_offset).map_err(|_| Error::ConversionError)?;
        block[start..start + 32].copy_from_slice(&entry.serialize(fat_type)[..]);

        trace!("Updating directory");
        block_cache.write_back().map_err(Error::DeviceError)?;
        Ok(())
    }

    /// Create a new directory.
    ///
    /// 1) Allocates a cluster to hold the new directory
    /// 2) Writes out its `.` and `..` entries and blanks the rest
    /// 3) Only then creates the entry naming it in the parent
    ///
    /// That order matters: an entry whose stored cluster is zero reads back as
    /// the root directory, so publishing the name first would let a failure
    /// leave a folder on the card claiming to be root. If anything fails the
    /// cluster is released, since nothing refers to it.
    ///
    /// With `long_name`, the entry gets a long-name chain as well, so a folder
    /// can be named the way a file can. `sfn` is its alias either way.
    pub(crate) fn make_dir<D, T>(
        &mut self,
        block_cache: &mut BlockCache<D>,
        time_source: &T,
        parent: ClusterId,
        sfn: ShortFileName,
        long_name: Option<&str>,
        att: Attributes,
    ) -> Result<(), Error<D::Error>>
    where
        D: BlockDevice,
        T: TimeSource,
    {
        // Everything the new directory is made of comes first, and only then
        // does anything in the parent name it. Publishing the name first and
        // filling it in afterwards means a failure in between leaves an entry
        // whose stored cluster is zero -- which is read back as the root
        // directory, so a folder that could not be created appears on the card
        // claiming to be the root.
        //
        // Nothing between the allocation and the end of this is reachable by
        // any name, so if any part of it fails the cluster is handed back.
        let new_cluster = self.alloc_cluster(block_cache, None, false)?;
        match self.build_and_publish_dir(
            block_cache,
            time_source,
            parent,
            sfn,
            long_name,
            att,
            new_cluster,
        ) {
            Ok(entry) => {
                debug!("Made new dir entry {:?}", entry);
                Ok(())
            }
            Err(error) => {
                self.release_unpublished_cluster(block_cache, new_cluster);
                Err(error)
            }
        }
    }

    /// Lay out a freshly allocated directory cluster and then name it in its
    /// parent.
    ///
    /// Every failure in here is one the caller undoes the same way, by
    /// releasing the cluster, which is why none of it is split out.
    #[allow(clippy::too_many_arguments)]
    fn build_and_publish_dir<D, T>(
        &mut self,
        block_cache: &mut BlockCache<D>,
        time_source: &T,
        parent: ClusterId,
        sfn: ShortFileName,
        long_name: Option<&str>,
        att: Attributes,
        new_cluster: ClusterId,
    ) -> Result<DirEntry, Error<D::Error>>
    where
        D: BlockDevice,
        T: TimeSource,
    {
        let new_dir_start_block = self.cluster_to_block(new_cluster);
        let now = time_source.get_timestamp();
        let fat_type = self.get_fat_type();
        // A blank block
        let block = block_cache.blank_mut(new_dir_start_block);
        // make the "." entry
        let dot_entry_in_child = DirEntry {
            name: crate::ShortFileName::this_dir(),
            mtime: now,
            ctime: now,
            attributes: att,
            // point at ourselves
            cluster: new_cluster,
            size: 0,
            entry_block: new_dir_start_block,
            entry_offset: 0,
        };
        debug!("New dir has {:?}", dot_entry_in_child);
        let mut offset = 0;
        block[offset..offset + OnDiskDirEntry::LEN]
            .copy_from_slice(&dot_entry_in_child.serialize(fat_type)[..]);
        offset += OnDiskDirEntry::LEN;
        // make the ".." entry
        let dot_dot_entry_in_child = DirEntry {
            name: crate::ShortFileName::parent_dir(),
            mtime: now,
            ctime: now,
            attributes: att,
            // point at our parent
            cluster: if parent == ClusterId::ROOT_DIR {
                // indicate parent is root using Cluster(0)
                ClusterId::EMPTY
            } else {
                parent
            },
            size: 0,
            entry_block: new_dir_start_block,
            entry_offset: OnDiskDirEntry::LEN_U32,
        };
        debug!("New dir has {:?}", dot_dot_entry_in_child);
        block[offset..offset + OnDiskDirEntry::LEN]
            .copy_from_slice(&dot_dot_entry_in_child.serialize(fat_type)[..]);

        block_cache.write_back()?;

        for block_idx in new_dir_start_block
            .range(BlockCount(u32::from(self.blocks_per_cluster)))
            .skip(1)
        {
            let _block = block_cache.blank_mut(block_idx);
            block_cache.write_back()?;
        }

        // The directory is complete; give it its name. Nothing up to here is
        // reachable by any name, so whatever happens the card is left as it
        // was apart from the cluster -- which the caller releases on any Err
        // out of this function.
        match long_name {
            Some(long_name) => self.write_directory_entry_lfn(
                block_cache,
                parent,
                long_name,
                sfn,
                att,
                new_cluster,
                0,
                now,
                now,
            ),
            None => self.write_new_directory_entry(
                block_cache,
                time_source,
                parent,
                sfn,
                att,
                new_cluster,
            ),
        }

    }

    /// Hand back a cluster that was allocated but never named.
    ///
    /// This only runs after something else has already failed, so it is best
    /// effort by nature. The free count is adjusted only if the FAT write
    /// actually landed: a count claiming space that is still allocated hands
    /// out clusters which are not there, where a merely pessimistic one only
    /// wastes them.
    fn release_unpublished_cluster<D>(
        &mut self,
        block_cache: &mut BlockCache<D>,
        cluster: ClusterId,
    ) where
        D: BlockDevice,
    {
        if self
            .update_fat(block_cache, cluster, ClusterId::EMPTY)
            .is_ok()
        {
            if let Some(count) = self.free_clusters_count.as_mut() {
                *count = count.saturating_add(1);
            }
            // Free again, so as good a place as any to look next.
            self.next_free_cluster = Some(cluster);
        }
    }
}

/// Load the boot parameter block from the start of the given partition and
/// determine if the partition contains a valid FAT16 or FAT32 file system.
pub fn parse_volume<D>(
    block_cache: &mut BlockCache<D>,
    lba_start: BlockIdx,
    num_blocks: BlockCount,
) -> Result<VolumeType, Error<D::Error>>
where
    D: BlockDevice,
    D::Error: core::fmt::Debug,
{
    trace!("Reading BPB");
    let block = block_cache.read(lba_start).map_err(Error::DeviceError)?;
    let bpb = Bpb::create_from_bytes(block).map_err(Error::FormatError)?;
    let fat_start = BlockCount(u32::from(bpb.reserved_block_count()));
    let second_fat_start = if bpb.num_fats() == 2 {
        Some(fat_start + BlockCount(bpb.fat_size()))
    } else {
        None
    };
    match bpb.fat_type {
        FatType::Fat16 => {
            if bpb.bytes_per_block() as usize != Block::LEN {
                return Err(Error::BadBlockSize(bpb.bytes_per_block()));
            }
            // FirstDataSector = BPB_ResvdSecCnt + (BPB_NumFATs * FATSz) + RootDirSectors;
            let root_dir_blocks = (u32::from(bpb.root_entries_count()) * OnDiskDirEntry::LEN_U32)
                .div_ceil(Block::LEN_U32);
            let first_root_dir_block =
                fat_start + BlockCount(u32::from(bpb.num_fats()) * bpb.fat_size());
            let first_data_block = first_root_dir_block + BlockCount(root_dir_blocks);
            let volume = FatVolume {
                lba_start,
                num_blocks,
                name: VolumeName {
                    contents: bpb.volume_label(),
                },
                blocks_per_cluster: bpb.blocks_per_cluster(),
                first_data_block,
                fat_start,
                second_fat_start,
                free_clusters_count: None,
                next_free_cluster: None,
                cluster_count: bpb.total_clusters(),
                fat_specific_info: FatSpecificInfo::Fat16(Fat16Info {
                    root_entries_count: bpb.root_entries_count(),
                    first_root_dir_block,
                }),
            };
            Ok(VolumeType::Fat(volume))
        }
        FatType::Fat32 => {
            // FirstDataSector = BPB_ResvdSecCnt + (BPB_NumFATs * FATSz);
            let first_data_block =
                fat_start + BlockCount(u32::from(bpb.num_fats()) * bpb.fat_size());
            // Safe to unwrap since this is a Fat32 Type
            let info_location = bpb.fs_info_block().unwrap();
            let mut volume = FatVolume {
                lba_start,
                num_blocks,
                name: VolumeName {
                    contents: bpb.volume_label(),
                },
                blocks_per_cluster: bpb.blocks_per_cluster(),
                first_data_block,
                fat_start,
                second_fat_start,
                free_clusters_count: None,
                next_free_cluster: None,
                cluster_count: bpb.total_clusters(),
                fat_specific_info: FatSpecificInfo::Fat32(Fat32Info {
                    info_location: lba_start + info_location,
                    first_root_dir_cluster: ClusterId(bpb.first_root_dir_cluster()),
                }),
            };

            // Now we don't need the BPB, update the volume with data from the info sector
            trace!("Reading info block");
            let info_block = block_cache
                .read(lba_start + info_location)
                .map_err(Error::DeviceError)?;
            let info_sector =
                InfoSector::create_from_bytes(info_block).map_err(Error::FormatError)?;
            volume.free_clusters_count = info_sector.free_clusters_count();
            volume.next_free_cluster = info_sector.next_free_cluster();

            Ok(VolumeType::Fat(volume))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volume_name() {
        let sfn = VolumeName {
            contents: *b"Hello \xA399  ",
        };
        assert_eq!(sfn, VolumeName::create_from_str("Hello £99").unwrap())
    }
}

// ****************************************************************************
//
// End Of File
//
// ****************************************************************************
