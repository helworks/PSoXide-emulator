//! Disc images read on demand.
//!
//! A mounted disc used to be the whole image in memory: 700 MB for a full
//! CD, most of which a game never touches in a session. Here a track is a
//! byte range of an image that is read (or, for ECM, decoded) a sector at a
//! time when the drive asks for it. The operating system's file cache does
//! the caching for plain images; ECM keeps a few decoded chunks.

use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use psx_iso::TrackSource;

/// A shared, random-access disc image that tracks are cut from.
pub type SharedImage = Arc<dyn TrackSource>;

/// A raw image file, read with positioned reads so nothing of it is held
/// in process memory.
pub struct FileImage {
    file: File,
    len: u64,
}

impl FileImage {
    /// Open `path` for reading.
    pub fn open(path: &Path) -> Result<Self, String> {
        let file = File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let len = file
            .metadata()
            .map_err(|e| format!("{}: {e}", path.display()))?
            .len();
        Ok(Self { file, len })
    }
}

impl TrackSource for FileImage {
    fn len(&self) -> u64 {
        self.len
    }

    fn read_at(&self, offset: u64, out: &mut [u8]) -> bool {
        if offset
            .checked_add(out.len() as u64)
            .is_none_or(|end| end > self.len)
        {
            return false;
        }
        read_exact_at(&self.file, offset, out)
    }
}

#[cfg(unix)]
fn read_exact_at(file: &File, offset: u64, out: &mut [u8]) -> bool {
    use std::os::unix::fs::FileExt;
    file.read_exact_at(out, offset).is_ok()
}

#[cfg(windows)]
fn read_exact_at(file: &File, mut offset: u64, mut out: &mut [u8]) -> bool {
    use std::os::windows::fs::FileExt;
    while !out.is_empty() {
        match file.seek_read(out, offset) {
            Ok(0) | Err(_) => return false,
            Ok(n) => {
                out = &mut out[n..];
                offset += n as u64;
            }
        }
    }
    true
}

#[cfg(not(any(unix, windows)))]
fn read_exact_at(_file: &File, _offset: u64, _out: &mut [u8]) -> bool {
    false
}

/// One track's bytes: the `len` bytes at `base` of a shared image. Several
/// tracks of a single-file CUE or a CloneCD image share one open file.
pub struct ImageSlice {
    image: SharedImage,
    base: u64,
    len: u64,
}

impl ImageSlice {
    /// The `len` bytes at `base` of `image`; clamped to the image.
    pub fn new(image: SharedImage, base: u64, len: u64) -> Self {
        let base = base.min(image.len());
        let len = len.min(image.len() - base);
        Self { image, base, len }
    }

    /// Byte `offset` of this slice as a range of the image, if it fits.
    fn span(&self, offset: u64, len: u64) -> Option<u64> {
        let end = offset.checked_add(len)?;
        (end <= self.len).then_some(self.base + offset)
    }
}

impl TrackSource for ImageSlice {
    fn len(&self) -> u64 {
        self.len
    }

    fn read_at(&self, offset: u64, out: &mut [u8]) -> bool {
        self.span(offset, out.len() as u64)
            .is_some_and(|at| self.image.read_at(at, out))
    }

    fn ready(&self, offset: u64, len: u64) -> bool {
        self.span(offset, len)
            .is_none_or(|at| self.image.ready(at, len))
    }

    fn prefetch(&self, offset: u64, len: u64) {
        let len = len.min(self.len.saturating_sub(offset));
        if let Some(at) = self.span(offset, len) {
            self.image.prefetch(at, len);
        }
    }
}

/// Bytes in one cooked sector: the user data alone.
pub const COOKED_SECTOR_BYTES: u64 = psx_iso::SECTOR_USER_DATA_BYTES as u64;

const RAW_SECTOR_BYTES: u64 = psx_iso::SECTOR_BYTES as u64;

/// What a cooked sector is rebuilt as. CUE sheets name it in the track
/// mode: `MODE1/2048` or `MODE2/2048`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CookedMode {
    /// Mode 1: header mode byte 1, data at 0x10, EDC and ECC.
    Mode1,
    /// Mode 2 Form 1: header mode byte 2, a plain-data subheader
    /// (submode 0x08) twice, data at 0x18, EDC and ECC.
    Mode2Form1,
}

/// A cooked image (2048 bytes of user data per sector, nothing else) seen
/// as raw 2352-byte sectors. The drive model reads sync, header and
/// subheader out of every sector, so each one is rebuilt on read: sync,
/// the BCD MSF of image sector `n` as `n + 150` (00:02:00 for the first
/// sector, as a disc that starts at LBA 0 has it), the mode byte, the
/// subheader for Mode 2, and ECMA-130 EDC/ECC from psx-iso.
///
/// The subheader cannot be recovered from a cooked image, so the
/// end-of-record and end-of-file bits of the original disc are gone; data
/// reads do not depend on them.
pub struct CookedImage {
    image: SharedImage,
    mode: CookedMode,
    sectors: u64,
}

impl CookedImage {
    /// View `image` as raw sectors. A trailing partial sector is ignored.
    pub fn new(image: SharedImage, mode: CookedMode) -> Self {
        let sectors = image.len() / COOKED_SECTOR_BYTES;
        Self {
            image,
            mode,
            sectors,
        }
    }

    /// Rebuild raw sector `index` into `sector`.
    fn raw_sector(&self, index: u64, sector: &mut [u8; psx_iso::SECTOR_BYTES]) -> bool {
        sector.fill(0);
        sector[1..11].fill(0xff);
        let (m, s, f) = psx_iso::lba_to_msf(index.min(u64::from(u32::MAX)) as u32);
        sector[12] = psx_iso::bin_to_bcd(m);
        sector[13] = psx_iso::bin_to_bcd(s);
        sector[14] = psx_iso::bin_to_bcd(f);
        let user_at = match self.mode {
            CookedMode::Mode1 => 0x10,
            CookedMode::Mode2Form1 => 0x18,
        };
        let user = &mut sector[user_at..user_at + COOKED_SECTOR_BYTES as usize];
        if !self.image.read_at(index * COOKED_SECTOR_BYTES, user) {
            return false;
        }
        match self.mode {
            CookedMode::Mode1 => {
                sector[15] = 1;
                psx_iso::protect_mode1(sector);
            }
            CookedMode::Mode2Form1 => {
                sector[15] = 2;
                sector[0x12] = 0x08;
                sector[0x16] = 0x08;
                psx_iso::protect_mode2_form1(sector);
            }
        }
        true
    }

    /// The cooked byte range behind raw bytes `offset..offset + len`.
    fn cooked_span(&self, offset: u64, len: u64) -> (u64, u64) {
        let first = offset / RAW_SECTOR_BYTES;
        let end = offset.saturating_add(len).div_ceil(RAW_SECTOR_BYTES);
        let end = end.min(self.sectors).max(first);
        (
            first * COOKED_SECTOR_BYTES,
            (end - first) * COOKED_SECTOR_BYTES,
        )
    }
}

impl TrackSource for CookedImage {
    fn len(&self) -> u64 {
        self.sectors * RAW_SECTOR_BYTES
    }

    fn read_at(&self, offset: u64, out: &mut [u8]) -> bool {
        if offset
            .checked_add(out.len() as u64)
            .is_none_or(|end| end > self.len())
        {
            return false;
        }
        let mut sector = [0u8; psx_iso::SECTOR_BYTES];
        let mut at = offset;
        let mut done = 0usize;
        while done < out.len() {
            let index = at / RAW_SECTOR_BYTES;
            let within = (at % RAW_SECTOR_BYTES) as usize;
            if !self.raw_sector(index, &mut sector) {
                return false;
            }
            let n = (psx_iso::SECTOR_BYTES - within).min(out.len() - done);
            out[done..done + n].copy_from_slice(&sector[within..within + n]);
            done += n;
            at += n as u64;
        }
        true
    }

    fn ready(&self, offset: u64, len: u64) -> bool {
        let (at, len) = self.cooked_span(offset, len);
        self.image.ready(at, len)
    }

    fn prefetch(&self, offset: u64, len: u64) {
        let (at, len) = self.cooked_span(offset, len);
        self.image.prefetch(at, len);
    }
}

/// Whether `image` starts with a raw sector's sync pattern.
pub fn starts_with_sync(image: &dyn TrackSource) -> bool {
    let mut sync = [0u8; 12];
    image.read_at(0, &mut sync) && sync[0] == 0 && sync[11] == 0 && sync[1..11] == [0xff; 10]
}

/// Open a raw image, or an ECM-packed one (decoded on demand), by content:
/// ECM streams start with `ECM\0`.
pub fn open_image(path: &Path) -> Result<SharedImage, String> {
    let file = FileImage::open(path)?;
    let mut magic = [0u8; 4];
    if file.read_at(0, &mut magic) && &magic == b"ECM\0" {
        return open_ecm(path);
    }
    Ok(Arc::new(file))
}

/// Open an ECM-packed image, decoded on demand.
pub fn open_ecm(path: &Path) -> Result<SharedImage, String> {
    let file = FileImage::open(path)?;
    let image = crate::ecm::EcmImage::open(Arc::new(file))
        .map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(Arc::new(image))
}

/// Read all of a track source in order through `f`, a megabyte at a time,
/// without holding more than that. Stops early and returns `false` if a read
/// fails.
pub fn for_each_chunk(source: &dyn TrackSource, mut f: impl FnMut(&[u8])) -> bool {
    const CHUNK: usize = 1 << 20;
    let mut buf = vec![0u8; CHUNK.min(source.len() as usize)];
    let mut at = 0u64;
    while at < source.len() {
        let n = CHUNK.min((source.len() - at) as usize);
        if !source.read_at(at, &mut buf[..n]) {
            return false;
        }
        f(&buf[..n]);
        at += n as u64;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slice_reads_stay_inside_their_range() {
        let image: SharedImage = Arc::new((0..100u8).collect::<Vec<u8>>());
        let slice = ImageSlice::new(image, 10, 20);
        let mut out = [0u8; 5];
        assert!(slice.read_at(15, &mut out));
        assert_eq!(out, [25, 26, 27, 28, 29]);
        assert!(!slice.read_at(16, &mut out));
        assert_eq!(slice.len(), 20);
    }

    #[test]
    fn slice_is_clamped_to_the_image() {
        let image: SharedImage = Arc::new(vec![7u8; 10]);
        let slice = ImageSlice::new(image.clone(), 8, 50);
        assert_eq!(slice.len(), 2);
        let slice = ImageSlice::new(image, 50, 50);
        assert_eq!(slice.len(), 0);
    }

    #[test]
    fn file_image_reads_positioned_ranges() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.bin");
        std::fs::write(&path, (0..=255u8).collect::<Vec<u8>>()).unwrap();
        let image = FileImage::open(&path).unwrap();
        assert_eq!(image.len(), 256);
        let mut out = [0u8; 4];
        assert!(image.read_at(252, &mut out));
        assert_eq!(out, [252, 253, 254, 255]);
        assert!(!image.read_at(253, &mut out));
    }

    #[test]
    fn cooked_mode2_sectors_match_the_disc_builders_raw_image() {
        let mut builder = psx_iso::IsoBuilder::new().volume_id("COOKED");
        builder.add_file("DATA.BIN", (0..9000u32).map(|i| (i * 7) as u8).collect());
        let cooked: SharedImage = Arc::new(builder.build());
        let raw = builder.build_bin();
        let image = CookedImage::new(cooked, CookedMode::Mode2Form1);
        assert_eq!(image.len(), raw.len() as u64);
        let mut out = vec![0u8; raw.len()];
        assert!(image.read_at(0, &mut out));
        assert_eq!(out, raw);
    }

    #[test]
    fn cooked_mode1_sector_has_a_mode1_header_and_valid_protection() {
        let user: Vec<u8> = (0..2 * COOKED_SECTOR_BYTES as u32)
            .map(|i| (i ^ 0x5a) as u8)
            .collect();
        let image = CookedImage::new(Arc::new(user.clone()), CookedMode::Mode1);
        let mut sector = [0u8; psx_iso::SECTOR_BYTES];
        assert!(image.read_at(RAW_SECTOR_BYTES, &mut sector));
        let mut reference = [0u8; psx_iso::SECTOR_BYTES];
        reference[1..11].fill(0xff);
        reference[12..16].copy_from_slice(&[0x00, 0x02, 0x01, 0x01]);
        reference[0x10..0x810].copy_from_slice(&user[2048..]);
        psx_iso::protect_mode1(&mut reference);
        assert_eq!(sector, reference);
    }

    #[test]
    fn cooked_reads_span_sectors_and_stop_at_the_end() {
        let user = vec![0x11u8; 3 * COOKED_SECTOR_BYTES as usize + 100];
        let image = CookedImage::new(Arc::new(user), CookedMode::Mode2Form1);
        // The partial fourth sector is not part of the image.
        assert_eq!(image.len(), 3 * RAW_SECTOR_BYTES);
        let mut whole = vec![0u8; image.len() as usize];
        assert!(image.read_at(0, &mut whole));
        let mut straddle = [0u8; 40];
        assert!(image.read_at(RAW_SECTOR_BYTES - 20, &mut straddle));
        let from = RAW_SECTOR_BYTES as usize - 20;
        assert_eq!(straddle[..], whole[from..from + 40]);
        // The second sector's header follows the first sector's tail.
        assert_eq!(
            straddle[20..32],
            [0, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0]
        );
        assert_eq!(straddle[32..36], [0x00, 0x02, 0x01, 0x02]);
        assert!(!image.read_at(image.len() - 10, &mut straddle));
    }

    #[test]
    fn sync_detection_tells_raw_from_cooked() {
        let mut builder = psx_iso::IsoBuilder::new();
        builder.add_file("A.BIN", vec![1; 10]);
        assert!(starts_with_sync(&builder.build_bin()));
        assert!(!starts_with_sync(&builder.build()));
        assert!(!starts_with_sync(&Vec::<u8>::new()));
    }

    #[test]
    fn chunks_cover_the_whole_source_in_order() {
        let bytes: Vec<u8> = (0..3_000_000u32).map(|i| (i % 251) as u8).collect();
        let mut seen = Vec::new();
        assert!(for_each_chunk(&bytes, |c| seen.extend_from_slice(c)));
        assert_eq!(seen, bytes);
    }
}
