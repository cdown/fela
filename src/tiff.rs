use anyhow::{ensure, Context, Result};
use std::cmp::Reverse;

const TAG_ORIENTATION: u16 = 0x112;
const TAG_JPEG_OFFSET: u16 = 0x201;
const TAG_JPEG_LEN: u16 = 0x202;
const IFD_ENTRY_LEN: usize = 12;

const MAX_IFDS: usize = 32;
const DRAFT_MIN_LEN: usize = 32 << 10;

/// Where something is, relative to the start of the TIFF header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ByteRange {
    pub offset: usize,
    pub len: usize,
}

#[derive(Debug, PartialEq, Eq)]
pub struct EmbeddedJpegs {
    pub full: ByteRange,
    /// A smaller JPEG to show while `full` is being read and decoded.
    pub draft: Option<ByteRange>,
    pub orientation: u16,
}

struct TiffReader<'a> {
    data: &'a [u8],
    is_le: bool,
}

impl TiffReader<'_> {
    fn bytes<const N: usize>(&self, offset: usize) -> Result<[u8; N]> {
        self.data
            .get(offset..)
            .and_then(|data| data.first_chunk())
            .copied()
            .context("TIFF structure is outside of the header we read")
    }

    fn u16(&self, offset: usize) -> Result<u16> {
        let bytes = self.bytes(offset)?;
        Ok(if self.is_le {
            u16::from_le_bytes(bytes)
        } else {
            u16::from_be_bytes(bytes)
        })
    }

    fn u32(&self, offset: usize) -> Result<usize> {
        let bytes = self.bytes(offset)?;
        let val = if self.is_le {
            u32::from_le_bytes(bytes)
        } else {
            u32::from_be_bytes(bytes)
        };
        Ok(usize::try_from(val)?)
    }
}

#[derive(Default)]
struct IfdInfo {
    jpegs: Vec<ByteRange>,
    orientation: Option<u16>,
}

pub fn has_tiff_magic(data: &[u8]) -> bool {
    data.starts_with(b"II*\0") || data.starts_with(b"MM\0*")
}

fn walk_ifds(data: &[u8]) -> Result<IfdInfo> {
    ensure!(has_tiff_magic(data), "Not a TIFF");
    let reader = TiffReader {
        data,
        is_le: data[0] == b'I',
    };
    let mut ifds = IfdInfo::default();
    let mut ifd = reader.u32(4)?;

    for _ in 0..MAX_IFDS {
        if ifd == 0 {
            break;
        }
        let nr_entries = usize::from(reader.u16(ifd)?);
        let (mut offset, mut len) = (None, None);
        for nr in 0..nr_entries {
            let entry = ifd + 2 + nr * IFD_ENTRY_LEN;
            match reader.u16(entry)? {
                TAG_JPEG_OFFSET => offset = Some(reader.u32(entry + 8)?),
                TAG_JPEG_LEN => len = Some(reader.u32(entry + 8)?),
                TAG_ORIENTATION if ifds.orientation.is_none() => {
                    ifds.orientation = Some(reader.u16(entry + 8)?);
                }
                _ => {}
            }
        }
        if let (Some(offset), Some(len)) = (offset, len) {
            ifds.jpegs.push(ByteRange { offset, len });
        }
        ifd = reader.u32(ifd + 2 + nr_entries * IFD_ENTRY_LEN)?;
    }
    Ok(ifds)
}

pub fn find_embedded(head: &[u8]) -> Result<EmbeddedJpegs> {
    let ifds = walk_ifds(head)?;
    let mut jpegs = ifds.jpegs;
    jpegs.retain(|jpeg| jpeg.len > 0);
    jpegs.sort_unstable_by_key(|jpeg| Reverse(jpeg.len));
    Ok(EmbeddedJpegs {
        full: *jpegs.first().context("No embedded JPEG")?,
        draft: jpegs
            .get(1)
            .copied()
            .filter(|jpeg| jpeg.len >= DRAFT_MIN_LEN),
        orientation: ifds.orientation.unwrap_or(1),
    })
}

pub fn exif_orientation(exif: &[u8]) -> Option<u16> {
    let tiff = exif.strip_prefix(b"Exif\0\0").unwrap_or(exif);
    walk_ifds(tiff).ok()?.orientation
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHORT: u16 = 3;
    const LONG: u16 = 4;

    fn make_tiff(is_le: bool, ifds: &[&[(u16, u16, u32)]]) -> Vec<u8> {
        let put16 = |v: u16| {
            if is_le {
                v.to_le_bytes()
            } else {
                v.to_be_bytes()
            }
        };
        let put32 = |v: u32| {
            if is_le {
                v.to_le_bytes()
            } else {
                v.to_be_bytes()
            }
        };
        let mut out = if is_le {
            b"II*\0".to_vec()
        } else {
            b"MM\0*".to_vec()
        };
        out.extend(put32(8));
        for (nr, entries) in ifds.iter().enumerate() {
            out.extend(put16(entries.len().try_into().unwrap()));
            for (tag, kind, val) in *entries {
                out.extend(put16(*tag));
                out.extend(put16(*kind));
                out.extend(put32(1));
                if *kind == SHORT {
                    out.extend(put16((*val).try_into().unwrap()));
                    out.extend([0, 0]);
                } else {
                    out.extend(put32(*val));
                }
            }
            let next = out.len() + 4;
            let is_last = nr == ifds.len() - 1;
            out.extend(put32(if is_last { 0 } else { next.try_into().unwrap() }));
        }
        out
    }

    fn jpeg(offset: u32, len: u32) -> [(u16, u16, u32); 2] {
        [(TAG_JPEG_OFFSET, LONG, offset), (TAG_JPEG_LEN, LONG, len)]
    }

    #[test]
    fn test_find_embedded_like_a1ii() {
        let preview = [&[(TAG_ORIENTATION, SHORT, 8)][..], &jpeg(200_866, 201_919)].concat();
        let tiff = make_tiff(
            true,
            &[&preview, &jpeg(44_046, 7_453), &jpeg(405_504, 2_912_933)],
        );
        let expected = EmbeddedJpegs {
            full: ByteRange {
                offset: 405_504,
                len: 2_912_933,
            },
            draft: Some(ByteRange {
                offset: 200_866,
                len: 201_919,
            }),
            orientation: 8,
        };
        assert_eq!(find_embedded(&tiff).unwrap(), expected);
    }

    #[test]
    fn test_find_embedded_big_endian() {
        let tiff = make_tiff(false, &[&jpeg(1000, 50_000)]);
        let expected = EmbeddedJpegs {
            full: ByteRange {
                offset: 1000,
                len: 50_000,
            },
            draft: None,
            orientation: 1,
        };
        assert_eq!(find_embedded(&tiff).unwrap(), expected);
    }

    #[test]
    fn test_find_embedded_thumbnail_is_not_a_draft() {
        let tiff = make_tiff(true, &[&jpeg(1000, 7_453), &jpeg(10_000, 500_000)]);
        assert_eq!(find_embedded(&tiff).unwrap().draft, None);
    }

    #[test]
    fn test_find_embedded_none() {
        let tiff = make_tiff(true, &[&[(TAG_ORIENTATION, SHORT, 1)]]);
        let err = find_embedded(&tiff).unwrap_err();
        assert_eq!(err.to_string(), "No embedded JPEG");
    }

    #[test]
    fn test_find_embedded_ifd_outside_header() {
        let tiff = make_tiff(true, &[&jpeg(1000, 50_000), &jpeg(2000, 60_000)]);
        assert!(find_embedded(&tiff[..30]).is_err());
    }

    #[test]
    fn test_find_embedded_ifd_loop() {
        let mut tiff = make_tiff(true, &[&jpeg(1000, 50_000)]);
        let next = tiff.len() - 4;
        tiff[next..].copy_from_slice(&8u32.to_le_bytes());
        assert_eq!(find_embedded(&tiff).unwrap().full.offset, 1000);
    }

    #[test]
    fn test_find_embedded_not_tiff() {
        assert!(find_embedded(b"\xff\xd8\xff\xe1 not a tiff").is_err());
    }

    #[test]
    fn test_exif_orientation() {
        let tiff = make_tiff(true, &[&[(TAG_ORIENTATION, SHORT, 6)]]);
        assert_eq!(exif_orientation(&tiff), Some(6));
        assert_eq!(
            exif_orientation(&[b"Exif\0\0", &tiff[..]].concat()),
            Some(6)
        );
        assert_eq!(exif_orientation(b"junk"), None);
    }
}
