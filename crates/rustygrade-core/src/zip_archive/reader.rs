use std::io::{Read, Seek, SeekFrom};

use flate2::read::DeflateDecoder;

use super::zip64::{find_eocd_offset, parse_eocd_and_zip64, resolve_entry_sizes};
use super::{
    CENTRAL_DIR_HEADER_SIG, FLAG_ENCRYPTED, FLAG_UTF8, LOCAL_FILE_HEADER_SIG, METHOD_DEFLATED,
    METHOD_STORED, ZipEntryInfo, ZipError,
};

const MAX_CENTRAL_DIR_BYTES: u64 = 128 * 1024 * 1024;
const STREAM_CHUNK_BYTES: usize = 8192;

/// Bounded PKWARE ZIP reader driven by the Central Directory so data-descriptor entries
/// (written by Java's `ZipOutputStream`) and ZIP64 archives work accurately.
pub(crate) struct ZipReader<R: Read + Seek> {
    reader: R,
    archive_len: u64,
    cd_offset: u64,
    entries: Vec<ZipEntryInfo>,
}

impl<R: Read + Seek> ZipReader<R> {
    pub(crate) fn open(mut reader: R, max_entries: usize) -> Result<Self, ZipError> {
        let archive_len = reader.seek(SeekFrom::End(0))?;
        if archive_len < 22 {
            return Err(ZipError::InvalidArchive(
                "file is shorter than a ZIP end-of-central-directory record".to_owned(),
            ));
        }

        let tail_len = archive_len.min(22 + u16::MAX as u64) as usize;
        let tail_start = archive_len - tail_len as u64;
        reader.seek(SeekFrom::Start(tail_start))?;
        let mut tail = vec![0u8; tail_len];
        reader.read_exact(&mut tail)?;

        let eocd_rel = find_eocd_offset(&tail).ok_or_else(|| {
            ZipError::InvalidArchive("missing ZIP end-of-central-directory record".to_owned())
        })?;
        let eocd_abs = tail_start + eocd_rel as u64;
        let (total_entries, cd_size, cd_offset) =
            parse_eocd_and_zip64(&mut reader, &tail[eocd_rel..eocd_rel + 22], eocd_abs)?;

        if total_entries > max_entries as u64 {
            return Err(ZipError::TooManyEntries { limit: max_entries });
        }
        if cd_size > MAX_CENTRAL_DIR_BYTES {
            return Err(ZipError::InvalidArchive(
                "central directory exceeds 128 MiB limit".to_owned(),
            ));
        }
        if cd_offset
            .checked_add(cd_size)
            .is_none_or(|end| end > eocd_abs)
        {
            return Err(ZipError::InvalidArchive(
                "central directory extends past EOCD".to_owned(),
            ));
        }
        if total_entries.saturating_mul(46) > cd_size {
            return Err(ZipError::InvalidArchive(
                "truncated central directory entry".to_owned(),
            ));
        }

        reader.seek(SeekFrom::Start(cd_offset))?;
        let mut cd_buf = vec![0u8; cd_size as usize];
        reader.read_exact(&mut cd_buf)?;

        let mut entries = Vec::with_capacity(total_entries as usize);
        let mut pos = 0usize;
        for _ in 0..total_entries {
            if pos + 46 > cd_buf.len() {
                return Err(ZipError::InvalidArchive(
                    "truncated central directory entry".to_owned(),
                ));
            }
            let h = &cd_buf[pos..pos + 46];
            if u32::from_le_bytes([h[0], h[1], h[2], h[3]]) != CENTRAL_DIR_HEADER_SIG {
                return Err(ZipError::InvalidArchive(
                    "invalid central directory header signature".to_owned(),
                ));
            }
            let flags = u16::from_le_bytes([h[8], h[9]]);
            let method = u16::from_le_bytes([h[10], h[11]]);
            let crc32 = u32::from_le_bytes([h[16], h[17], h[18], h[19]]);
            let comp32 = u32::from_le_bytes([h[20], h[21], h[22], h[23]]);
            let uncomp32 = u32::from_le_bytes([h[24], h[25], h[26], h[27]]);
            let name_len = u16::from_le_bytes([h[28], h[29]]) as usize;
            let extra_len = u16::from_le_bytes([h[30], h[31]]) as usize;
            let comment_len = u16::from_le_bytes([h[32], h[33]]) as usize;
            let disk_start = u16::from_le_bytes([h[34], h[35]]);
            let local32 = u32::from_le_bytes([h[42], h[43], h[44], h[45]]);
            pos += 46;

            if disk_start != 0 {
                return Err(ZipError::InvalidArchive(
                    "multi-disk ZIP entries are not supported".to_owned(),
                ));
            }
            let var_len = name_len + extra_len + comment_len;
            if pos + var_len > cd_buf.len() {
                return Err(ZipError::InvalidArchive(
                    "central directory entry overflows directory bounds".to_owned(),
                ));
            }

            let name_bytes = &cd_buf[pos..pos + name_len];
            let extra_bytes = &cd_buf[pos + name_len..pos + name_len + extra_len];
            pos += var_len;

            if name_bytes.contains(&0) {
                return Err(ZipError::PathTraversal(
                    String::from_utf8_lossy(name_bytes).into_owned(),
                ));
            }
            let raw_name = if (flags & FLAG_UTF8) != 0 {
                std::str::from_utf8(name_bytes)
                    .map_err(|_| {
                        ZipError::InvalidArchive("entry name is not valid UTF-8".to_owned())
                    })?
                    .to_owned()
            } else {
                String::from_utf8_lossy(name_bytes).into_owned()
            };

            let (uncompressed_size, compressed_size, local_header_offset) =
                resolve_entry_sizes(uncomp32, comp32, local32, extra_bytes)?;

            if local_header_offset >= cd_offset {
                return Err(ZipError::InvalidArchive(format!(
                    "entry '{raw_name}' local header overlaps central directory"
                )));
            }

            let is_dir = raw_name.ends_with('/') || raw_name.ends_with('\\');
            entries.push(ZipEntryInfo {
                raw_name,
                method,
                flags,
                crc32,
                compressed_size,
                uncompressed_size,
                local_header_offset,
                is_dir,
            });
        }

        Ok(Self {
            reader,
            archive_len,
            cd_offset,
            entries,
        })
    }

    pub(crate) fn entries(&self) -> &[ZipEntryInfo] {
        &self.entries
    }

    fn locate_entry_data(&mut self, entry: &ZipEntryInfo) -> Result<(), ZipError> {
        if entry
            .local_header_offset
            .checked_add(30)
            .is_none_or(|end| end > self.cd_offset)
        {
            return Err(ZipError::InvalidArchive(format!(
                "local file header out of bounds for '{}'",
                entry.raw_name
            )));
        }
        self.reader
            .seek(SeekFrom::Start(entry.local_header_offset))?;
        let mut lfh = [0u8; 30];
        self.reader.read_exact(&mut lfh)?;
        if u32::from_le_bytes([lfh[0], lfh[1], lfh[2], lfh[3]]) != LOCAL_FILE_HEADER_SIG {
            return Err(ZipError::InvalidArchive(format!(
                "invalid local file header signature for '{}'",
                entry.raw_name
            )));
        }
        let local_name_len = u16::from_le_bytes([lfh[26], lfh[27]]) as u64;
        let local_extra_len = u16::from_le_bytes([lfh[28], lfh[29]]) as u64;
        let data_start = entry
            .local_header_offset
            .checked_add(30 + local_name_len + local_extra_len)
            .ok_or_else(|| ZipError::InvalidArchive("entry offset overflow".to_owned()))?;
        if data_start
            .checked_add(entry.compressed_size)
            .is_none_or(|end| end > self.cd_offset)
        {
            return Err(ZipError::InvalidArchive(format!(
                "entry '{}' compressed payload extends past central directory",
                entry.raw_name
            )));
        }
        self.reader.seek(SeekFrom::Start(data_start))?;
        Ok(())
    }

    pub(crate) fn read_entry_bounded(
        &mut self,
        entry: &ZipEntryInfo,
        max_bytes: usize,
        ratio_grace: u64,
        max_ratio: u64,
    ) -> Result<Vec<u8>, ZipError> {
        if (entry.flags & FLAG_ENCRYPTED) != 0 {
            return Err(ZipError::EncryptedEntry {
                name: entry.raw_name.clone(),
            });
        }
        if entry.uncompressed_size > max_bytes as u64 {
            return Err(ZipError::EntryTooLarge {
                name: entry.raw_name.clone(),
                limit: max_bytes,
            });
        }
        if entry.uncompressed_size > ratio_grace
            && entry.uncompressed_size > max_ratio.saturating_mul(entry.compressed_size.max(1))
        {
            return Err(ZipError::ZipBomb {
                name: entry.raw_name.clone(),
            });
        }

        match entry.method {
            METHOD_STORED => {
                if entry.compressed_size != entry.uncompressed_size {
                    return Err(ZipError::InvalidArchive(format!(
                        "stored entry '{}' has mismatched sizes",
                        entry.raw_name
                    )));
                }
                self.locate_entry_data(entry)?;
                let mut out = vec![0u8; entry.uncompressed_size as usize];
                self.reader.read_exact(&mut out)?;
                if crc32fast::hash(&out) != entry.crc32 {
                    return Err(ZipError::CrcMismatch {
                        name: entry.raw_name.clone(),
                    });
                }
                Ok(out)
            }
            METHOD_DEFLATED => {
                self.locate_entry_data(entry)?;
                let initial_cap = (entry.uncompressed_size as usize)
                    .min(max_bytes)
                    .min(1024 * 1024);
                let mut out = Vec::with_capacity(initial_cap);
                let limited = (&mut self.reader).take(entry.compressed_size);
                let mut decoder = DeflateDecoder::new(limited);
                let mut buf = [0u8; STREAM_CHUNK_BYTES];
                loop {
                    let n = decoder.read(&mut buf)?;
                    if n == 0 {
                        break;
                    }
                    if out.len() + n > max_bytes {
                        return Err(ZipError::EntryTooLarge {
                            name: entry.raw_name.clone(),
                            limit: max_bytes,
                        });
                    }
                    let next_len = (out.len() + n) as u64;
                    if next_len > entry.uncompressed_size {
                        return Err(ZipError::ZipBomb {
                            name: entry.raw_name.clone(),
                        });
                    }
                    if next_len > ratio_grace
                        && next_len > max_ratio.saturating_mul(decoder.total_in().max(1))
                    {
                        return Err(ZipError::ZipBomb {
                            name: entry.raw_name.clone(),
                        });
                    }
                    out.extend_from_slice(&buf[..n]);
                }
                if out.len() as u64 != entry.uncompressed_size {
                    return Err(ZipError::InvalidArchive(format!(
                        "entry '{}' uncompressed size mismatch",
                        entry.raw_name
                    )));
                }
                if crc32fast::hash(&out) != entry.crc32 {
                    return Err(ZipError::CrcMismatch {
                        name: entry.raw_name.clone(),
                    });
                }
                Ok(out)
            }
            other => Err(ZipError::UnsupportedCompression {
                name: entry.raw_name.clone(),
                method: other,
            }),
        }
    }

    pub(crate) fn drain_ignored_entry(
        &mut self,
        entry: &ZipEntryInfo,
        total_ignored: &mut u64,
        max_ignored: u64,
        ratio_grace: u64,
        max_ratio: u64,
    ) -> Result<(), ZipError> {
        if entry.uncompressed_size > max_ignored || entry.compressed_size > max_ignored {
            return Err(ZipError::ZipBomb {
                name: entry.raw_name.clone(),
            });
        }
        if entry.uncompressed_size > ratio_grace
            && entry.uncompressed_size > max_ratio.saturating_mul(entry.compressed_size.max(1))
        {
            return Err(ZipError::ZipBomb {
                name: entry.raw_name.clone(),
            });
        }
        if entry.compressed_size == 0 && entry.uncompressed_size == 0 {
            return Ok(());
        }

        self.locate_entry_data(entry)?;

        if (entry.flags & FLAG_ENCRYPTED) != 0
            || !matches!(entry.method, METHOD_STORED | METHOD_DEFLATED)
        {
            *total_ignored = total_ignored.saturating_add(entry.uncompressed_size);
            return check_ignored_totals(
                &entry.raw_name,
                *total_ignored,
                max_ignored,
                ratio_grace,
                max_ratio,
                self.archive_len,
            );
        }

        if entry.method == METHOD_STORED {
            if entry.compressed_size != entry.uncompressed_size {
                return Err(ZipError::InvalidArchive(format!(
                    "stored entry '{}' has mismatched sizes",
                    entry.raw_name
                )));
            }
            *total_ignored = total_ignored.saturating_add(entry.uncompressed_size);
            return check_ignored_totals(
                &entry.raw_name,
                *total_ignored,
                max_ignored,
                ratio_grace,
                max_ratio,
                self.archive_len,
            );
        }

        let limited = (&mut self.reader).take(entry.compressed_size);
        let mut decoder = DeflateDecoder::new(limited);
        let mut buf = [0u8; STREAM_CHUNK_BYTES];
        let mut entry_out = 0u64;
        loop {
            let n = decoder.read(&mut buf)?;
            if n == 0 {
                break;
            }
            entry_out = entry_out.saturating_add(n as u64);
            *total_ignored = total_ignored.saturating_add(n as u64);
            if entry_out > ratio_grace
                && entry_out > max_ratio.saturating_mul(decoder.total_in().max(1))
            {
                return Err(ZipError::ZipBomb {
                    name: entry.raw_name.clone(),
                });
            }
            check_ignored_totals(
                &entry.raw_name,
                *total_ignored,
                max_ignored,
                ratio_grace,
                max_ratio,
                self.archive_len,
            )?;
        }
        Ok(())
    }
}

fn check_ignored_totals(
    name: &str,
    total_ignored: u64,
    max_ignored: u64,
    ratio_grace: u64,
    max_ratio: u64,
    archive_len: u64,
) -> Result<(), ZipError> {
    if total_ignored > max_ignored {
        return Err(ZipError::ZipBomb {
            name: name.to_owned(),
        });
    }
    if total_ignored > ratio_grace && total_ignored > max_ratio.saturating_mul(archive_len.max(1)) {
        return Err(ZipError::ZipBomb {
            name: name.to_owned(),
        });
    }
    Ok(())
}

#[cfg(test)]
#[path = "reader_tests.rs"]
mod tests;
