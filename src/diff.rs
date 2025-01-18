use std::cmp::Ordering;
use std::io::{ErrorKind, Read, Write};

use byteorder::WriteBytesExt;
#[cfg(not(feature = "c"))]
use divsufsort as cdivsufsort;
use thiserror::Error;
use zerocopy::{IntoBytes, I64, U64};

use crate::{EntryHeader, PatchHeader, State, DDELTA_MAGIC};

type Str = Box<str>;
type Result<T> = std::result::Result<T, DiffError>;

#[derive(Error, Debug)]
pub enum DiffError {
    #[error("io error while generating patch {0}")]
    Io(#[from] std::io::Error),
    #[error("patch generation failed: {0}")]
    Internal(Str),
}

const FUZZ: isize = 8;

fn read_up_to(reader: &mut impl Read, buf: &mut [u8]) -> Result<usize> {
    let mut bytes_read = 0;
    while bytes_read < buf.len() {
        match reader.read(&mut buf[bytes_read..]) {
            Ok(0) => break,
            Ok(n) => {
                bytes_read += n;
            }
            Err(ref e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(bytes_read)
}

/// Generate a ddelta patch. This does **not** have a limit of 2^31-1 bytes, unlike [`generate`].
///
/// However, the output is not compatible with the original ddelta tool or bsdiff. Attempting to use
/// the original program or [`apply`][crate::apply] with the output created by this function will
/// create an unspecified output, that is only valid up to `chunk_sizes` or 2^31-1 bytes, whichever
/// is smaller. [`apply_chunked`][crate::apply_chunked] must be used to use the patch file.
/// `progress` is a function that will be called periodically with progress updates. The algorithm
/// will never consume more than chunk_sizes * 6, so this parameter can be used to implement a RAM
/// limit. Pass [`None`] as a parameter to set no limit. Note that this uses anything implementing
/// `Into<Option<usize>>`, including a [`usize`] itself, so you can just pass a number to that
/// parameter. A smaller `chunk_sizes` value uses less RAM, but creates less optimal patches.
// todo: i don't think impl Read is a honest representation of whats happening.
// this reads gigabytes of data into memory at first opportunity
// take (&[u8], &[u8], impl Write) instead
pub fn generate_chunked(
    old_f: &mut impl Read,
    new_f: &mut impl Read,
    patch_f: &mut impl Write,
    chunk_sizes: impl Into<Option<usize>>,
    mut progress: impl FnMut(State),
) -> Result<()> {
    let chunk_sizes = chunk_sizes
        .into()
        .unwrap_or(i32::MAX as usize - 1)
        .min(i32::MAX as usize - 1);
    let mut old_buf = vec![0; chunk_sizes];
    let mut new_buf = vec![0; chunk_sizes];
    let mut bytes_completed = 0;
    loop {
        progress(State::Reading);
        let new_bytes_read = read_up_to(new_f, &mut new_buf)?;
        let new_buf = &new_buf[..new_bytes_read];
        // Nothing left in new file, so no need to read any more
        if new_buf.is_empty() {
            if bytes_completed == 0 {
                write_header(patch_f, 0)?;
                write_ending(patch_f)?;
            }
            break;
        }

        let old_bytes_read = read_up_to(old_f, &mut old_buf)?;
        let old_buf = &old_buf[..old_bytes_read];

        generate(old_buf, new_buf, patch_f, |d| match d {
            State::Working(bytes) => progress(State::Working(bytes + bytes_completed)),
            other => progress(other),
        })?;
        bytes_completed += new_bytes_read as u64;
    }
    Ok(())
}

fn write_header(patch: &mut impl Write, len: u64) -> Result<()> {
    patch
        .write_all(
            PatchHeader {
                magic: *DDELTA_MAGIC,
                new_file_size: U64::new(len),
            }
            .as_bytes(),
        )
        .map_err(|e| e.into())
}

fn write_ending(patch: &mut impl Write) -> Result<()> {
    patch
        .write_all(
            EntryHeader {
                diff: Default::default(),
                extra: Default::default(),
                seek: Default::default(),
            }
            .as_bytes(),
        )
        .map_err(|e| e.into())
}

/// Generate a ddelta patch. This has a limit of 2^31-1 bytes.
///
/// Beyond this, use [`generate_chunked`]
/// to create a patch file with multiple patches. The output is compatible with the original ddelta
/// tool, but not with bsdiff. Call [`apply`][crate::apply] or
/// [`apply_chunked`][crate::apply_chunked] to use the created patch file. `progress` is a function
/// that will be called periodically with progress updates.
pub fn generate(
    old: &[u8],
    new: &[u8],
    patch: &mut impl Write,
    mut progress: impl FnMut(State),
) -> Result<()> {
    if !old.len().max(new.len()) < i32::MAX as usize {
        return Err(DiffError::Internal(
            format!("The filesize must not be larger than {} bytes", i32::MAX).into(),
        ));
    }
    progress(State::Sorting);
    write_header(patch, new.len() as u64)?;
    let mut sorted = cdivsufsort::sort(old).into_parts().1;
    sorted.push(0);
    let mut scan = 0;
    let mut len = 0;
    let mut pos = 0;
    let mut lastoffset = 0;
    let mut lastscan = 0;
    let mut lastpos = 0;
    while scan < new.len() as isize {
        let mut num_less_than_eight = 0;
        let mut oldscore: isize = 0;
        scan += len;
        let mut scsc = scan;
        // If we come across a large block of data that only differs
        // by less than 8 bytes, this loop will take a long time to
        // go past that block of data. We need to track the number of
        // times we're stuck in the block and break out of it.
        while scan < new.len() as isize {
            if scan % 10_000 == 0 {
                progress(State::Working(scan as u64));
            }
            let prev_len = len;
            let prev_oldscore = oldscore;
            let prev_pos = pos;

            len = search(
                &sorted,
                &old[..old.len().wrapping_sub(1).min(old.len())],
                &new[scan as usize..],
                0,
                old.len(),
                &mut pos,
            );

            while scsc < scan + len {
                if (scsc + lastoffset < old.len() as isize)
                    && (old[(scsc + lastoffset) as usize] == new[scsc as usize])
                {
                    oldscore += 1;
                }
                scsc += 1;
            }

            if ((len == oldscore) && (len != 0)) || (len > oldscore + 8) {
                break;
            }

            if (scan + lastoffset < old.len() as isize)
                && (old[(scan + lastoffset) as usize] == new[scan as usize])
            {
                oldscore -= 1;
            }

            if prev_len - FUZZ <= len
                && len <= prev_len
                && prev_oldscore - FUZZ <= oldscore
                && oldscore <= prev_oldscore
                && prev_pos <= pos
                && pos <= prev_pos + FUZZ
                && oldscore <= len
                && len <= oldscore + FUZZ
            {
                num_less_than_eight += 1;
            } else {
                num_less_than_eight = 0;
            }

            if num_less_than_eight > 100 {
                break;
            }

            scan += 1;
        }

        if (len != oldscore) || (scan == new.len() as isize) {
            let mut s = 0;
            let mut s_f = 0;
            let mut lenf = 0;
            let mut i = 0;
            while (lastscan + i < scan) && (lastpos + i < old.len() as isize) {
                if old[(lastpos + i) as usize] == new[(lastscan + i) as usize] {
                    s += 1;
                }
                i += 1;
                if s * 2 - i > s_f * 2 - lenf {
                    s_f = s;
                    lenf = i;
                }
            }
            let mut lenb = 0;
            if scan < new.len() as isize {
                let mut s = 0;
                let mut s_b = 0;
                i = 1;
                while (scan >= lastscan + i) && (pos >= i) {
                    if old[(pos - i) as usize] == new[(scan - i) as usize] {
                        s += 1;
                    }
                    if s * 2 - i > s_b * 2 - lenb {
                        s_b = s;
                        lenb = i;
                    }
                    i += 1;
                }
            }
            if lastscan + lenf > scan - lenb {
                let overlap = (lastscan + lenf) - (scan - lenb);
                let mut s = 0;
                let mut s_s = 0;
                let mut lens = 0;
                for i in 0..overlap {
                    if new[(lastscan + lenf - overlap + i) as usize]
                        == old[(lastpos + lenf - overlap + i) as usize]
                    {
                        s += 1;
                    }
                    if new[(scan - lenb + i) as usize] == old[(pos - lenb + i) as usize] {
                        s -= 1;
                    }
                    if s > s_s {
                        s_s = s;
                        lens = i + 1;
                    }
                }
                lenf += lens - overlap;
                lenb -= lens;
            }
            if lenf < 0 || (scan - lenb) - (lastscan + lenf) < 0 {
                return Err(DiffError::Internal(
                    "invalid state while creating patch".into(),
                ));
            }
            patch.write_all(
                EntryHeader {
                    diff: U64::new(lenf as u64),
                    extra: U64::new(((scan - lenb) - (lastscan + lenf)) as u64),
                    seek: I64::new(((pos - lenb) - (lastpos + lenf)) as i64),
                }
                .as_bytes(),
            )?;
            for i in 0..lenf {
                patch.write_u8(
                    new[(lastscan + i) as usize].wrapping_sub(old[(lastpos + i) as usize]),
                )?;
            }
            if (scan - lenb) - (lastscan + lenf) != 0 {
                patch.write_all(&new[(lastscan + lenf) as usize..(scan - lenb) as usize])?;
            }

            lastscan = scan - lenb;
            lastpos = pos - lenb;
            lastoffset = pos - scan;
        }
    }
    write_ending(patch)?;
    patch.flush()?;
    Ok(())
}

fn match_len(a: &[u8], b: &[u8]) -> usize {
    a.iter()
        .zip(b.iter())
        .enumerate()
        .take_while(|(_, (old, new))| old == new)
        .last()
        .map_or(0, |(i, _)| i + 1)
}

/// Compares lexicographically the common part of these slices, i.e. takes the smallest length and
/// compares within that.
fn min_memcmp(a: &[u8], b: &[u8]) -> Ordering {
    let len = a.len().min(b.len());
    a[..len].cmp(&b[..len])
}

/// This is a binary search of the string `new` in the `old` string using the suffix array
/// `sorted`. `st` and `en` is the start and end of the search range (inclusive).
/// Returns the length of the longest prefix found and stores the position of the
/// string found in `*pos`.
fn search(sorted: &[i32], old: &[u8], new: &[u8], st: usize, en: usize, pos: &mut isize) -> isize {
    if en - st < 2 {
        let x = match_len(&old[(sorted[st] as usize)..], new) as isize;
        let y = match_len(&old[(sorted[en] as usize)..], new) as isize;

        if x > y {
            *pos = sorted[st] as isize;
            x
        } else {
            *pos = sorted[en] as isize;
            y
        }
    } else {
        let x = st + (en - st) / 2;
        if min_memcmp(&old[(sorted[x] as usize)..], new) != Ordering::Greater {
            search(sorted, old, new, x, en, pos)
        } else {
            search(sorted, old, new, st, x, pos)
        }
    }
}

#[cfg(test)]
mod test {
    use crate::diff::match_len;

    #[test]
    fn testy() {
        assert_eq!(match_len(b"abcdef", b"abcfed"), 3);
        assert_eq!(match_len(b"abc", b"abcfed"), 3);
        assert_eq!(match_len(b"abcdef", b"abc"), 3);
        assert_eq!(match_len(b"dabcde", b"abcfed"), 0);
    }
}
