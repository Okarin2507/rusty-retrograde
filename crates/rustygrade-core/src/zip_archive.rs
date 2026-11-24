use std::io::{self, Read, Seek, SeekFrom, Write};

use flate2::Compression;
use flate2::read::DeflateDecoder;
use flate2::write::DeflateEncoder;
use thiserror::Error;

const LOCAL_FILE_HEADER_SIG: u32 = 0x0403_4b50;
const CENTRAL_DIR_HEADER_SIG: u32 = 0x0201_4b50;
const EOCD_SIG: u32 = 0x0605_4b50;
const ZIP64_EOCD_SIG: u32 = 0x0606_4b50;
const ZIP64_LOCATOR_SIG: u32 = 0x0706_4b50;
const ZIP64_EXTRA_TAG: u16 = 0x0001;

const FLAG_ENCRYPTED: u16 = 0x0001;
const FLAG_UTF8: u16 = 0x0800;

pub const METHOD_STORED: u16 = 0;
pub const METHOD_DEFLATED: u16 = 8;

const MAX_CENTRAL_DIR_BYTES: u64 = 128 * 1024 * 1024;
const STREAM_CHUNK_BYTES: usize = 8192;

#[derive(Debug, Error)]
pub enum ZipError {
    #[error("Invalid ZIP archive: {0}")]
    InvalidArchive(String),
    #[error("Unsafe path in ZIP archive: {0}")]
    PathTraversal(String),
    #[error("ZIP archive has too many entries (max {limit})")]
    TooManyEntries { limit: usize },
    #[error("ZIP entry '{name}' exceeds size limit ({limit} bytes)")]
    EntryTooLarge { name: String, limit: usize },
    #[error("ZIP archive exceeds total size limit ({limit} bytes)")]
    TotalTooLarge { limit: usize },
    #[error("ZIP archive looks like a zip bomb (entry '{name}')")]
    ZipBomb { name: String },
    #[error("ZIP entry '{name}' failed CRC-32 check")]
    CrcMismatch { name: String },
    #[error("ZIP entry '{name}' uses unsupported compression method {method}")]
    UnsupportedCompression { name: String, method: u16 },
    #[error("ZIP entry '{name}' is encrypted")]
    EncryptedEntry { name: String },
    #[error("I/O error reading or writing ZIP archive: {0}")]
    Io(#[from] io::Error),
}

struct WrittenEntry {
    name: Vec<u8>,
    method: u16,
    crc32: u32,
    compressed_size: u32,
    uncompressed_size: u32,
    local_header_offset: u32,
}

/// Minimal PKWARE-compliant ZIP writer supporting STORED (method 0) and DEFLATE (method 8).
pub struct ZipWriter<W: Write> {
    writer: W,
    offset: u64,
    entries: Vec<WrittenEntry>,
}

impl<W: Write> ZipWriter<W> {
    pub fn new(writer: W) -> Self {
        Self {
            writer,
            offset: 0,
            entries: Vec::new(),
        }
    }

    pub fn add_stored(&mut self, name: &str, data: &[u8]) -> Result<(), ZipError> {
        self.write_entry(name, METHOD_STORED, data, data)
    }

    pub fn add_deflated(&mut self, name: &str, data: &[u8]) -> Result<(), ZipError> {
        let mut encoder = DeflateEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(data)?;
        let compressed = encoder.finish()?;
        self.write_entry(name, METHOD_DEFLATED, data, &compressed)
    }

    fn write_entry(
        &mut self,
        name: &str,
        method: u16,
        uncompressed: &[u8],
        payload: &[u8],
    ) -> Result<(), ZipError> {
        if name.is_empty() || name.contains('\0') {
            return Err(ZipError::PathTraversal(name.to_owned()));
        }
        if self.entries.len() >= u16::MAX as usize {
            return Err(ZipError::TooManyEntries {
                limit: u16::MAX as usize,
            });
        }
        let name_bytes = name.as_bytes();
        let name_len = u16::try_from(name_bytes.len())
            .map_err(|_| ZipError::InvalidArchive("entry name exceeds 64 KiB".to_owned()))?;
        let uncompressed_size = u32::try_from(uncompressed.len())
            .map_err(|_| ZipError::InvalidArchive("entry exceeds 4 GiB ZIP limit".to_owned()))?;
        let compressed_size = u32::try_from(payload.len())
            .map_err(|_| ZipError::InvalidArchive("entry exceeds 4 GiB ZIP limit".to_owned()))?;
        let local_header_offset = u32::try_from(self.offset)
            .map_err(|_| ZipError::InvalidArchive("archive exceeds 4 GiB ZIP limit".to_owned()))?;

        let crc32 = crc32fast::hash(uncompressed);

        let mut header = [0u8; 30];
        header[0..4].copy_from_slice(&LOCAL_FILE_HEADER_SIG.to_le_bytes());
        header[4..6].copy_from_slice(&20u16.to_le_bytes()); // version needed: 2.0
        header[6..8].copy_from_slice(&FLAG_UTF8.to_le_bytes());
        header[8..10].copy_from_slice(&method.to_le_bytes());
        // DOS time 0, DOS date 1980-01-01 (0x0021)
        header[10..12].copy_from_slice(&0u16.to_le_bytes());
        header[12..14].copy_from_slice(&0x0021u16.to_le_bytes());
        header[14..18].copy_from_slice(&crc32.to_le_bytes());
        header[18..22].copy_from_slice(&compressed_size.to_le_bytes());
        header[22..26].copy_from_slice(&uncompressed_size.to_le_bytes());
        header[26..28].copy_from_slice(&name_len.to_le_bytes());
        header[28..30].copy_from_slice(&0u16.to_le_bytes());

        self.writer.write_all(&header)?;
        self.writer.write_all(name_bytes)?;
        self.writer.write_all(payload)?;

        self.offset = self
            .offset
            .checked_add(30 + name_bytes.len() as u64 + payload.len() as u64)
            .ok_or_else(|| ZipError::InvalidArchive("archive size overflow".to_owned()))?;

        self.entries.push(WrittenEntry {
            name: name_bytes.to_vec(),
            method,
            crc32,
            compressed_size,
            uncompressed_size,
            local_header_offset,
        });
        Ok(())
    }

    pub fn finish(mut self) -> Result<W, ZipError> {
        let cd_offset = u32::try_from(self.offset)
            .map_err(|_| ZipError::InvalidArchive("archive exceeds 4 GiB ZIP limit".to_owned()))?;
        let entry_count = u16::try_from(self.entries.len()).map_err(|_| {
            ZipError::TooManyEntries {
                limit: u16::MAX as usize,
            }
        })?;

        let mut cd_size_u64 = 0u64;
        for entry in &self.entries {
            let name_len = entry.name.len() as u16;
            let mut cd = [0u8; 46];
            cd[0..4].copy_from_slice(&CENTRAL_DIR_HEADER_SIG.to_le_bytes());
            cd[4..6].copy_from_slice(&0x0314u16.to_le_bytes()); // UNIX, version 2.0
            cd[6..8].copy_from_slice(&20u16.to_le_bytes());
            cd[8..10].copy_from_slice(&FLAG_UTF8.to_le_bytes());
            cd[10..12].copy_from_slice(&entry.method.to_le_bytes());
            cd[12..14].copy_from_slice(&0u16.to_le_bytes());
            cd[14..16].copy_from_slice(&0x0021u16.to_le_bytes());
            cd[16..20].copy_from_slice(&entry.crc32.to_le_bytes());
            cd[20..24].copy_from_slice(&entry.compressed_size.to_le_bytes());
            cd[24..28].copy_from_slice(&entry.uncompressed_size.to_le_bytes());
            cd[28..30].copy_from_slice(&name_len.to_le_bytes());
            cd[30..32].copy_from_slice(&0u16.to_le_bytes());
            cd[32..34].copy_from_slice(&0u16.to_le_bytes());
            cd[34..36].copy_from_slice(&0u16.to_le_bytes());
            cd[36..38].copy_from_slice(&0u16.to_le_bytes());
            cd[38..42].copy_from_slice(&(0o100_644u32 << 16).to_le_bytes());
            cd[42..46].copy_from_slice(&entry.local_header_offset.to_le_bytes());

            self.writer.write_all(&cd)?;
            self.writer.write_all(&entry.name)?;
            cd_size_u64 += 46 + entry.name.len() as u64;
        }

        let cd_size = u32::try_from(cd_size_u64)
            .map_err(|_| ZipError::InvalidArchive("central directory exceeds 4 GiB".to_owned()))?;

        let mut eocd = [0u8; 22];
        eocd[0..4].copy_from_slice(&EOCD_SIG.to_le_bytes());
        eocd[4..6].copy_from_slice(&0u16.to_le_bytes());
        eocd[6..8].copy_from_slice(&0u16.to_le_bytes());
        eocd[8..10].copy_from_slice(&entry_count.to_le_bytes());
        eocd[10..12].copy_from_slice(&entry_count.to_le_bytes());
        eocd[12..16].copy_from_slice(&cd_size.to_le_bytes());
        eocd[16..20].copy_from_slice(&cd_offset.to_le_bytes());
        eocd[20..22].copy_from_slice(&0u16.to_le_bytes());

        self.writer.write_all(&eocd)?;
        self.writer.flush()?;
        Ok(self.writer)
    }
}

#[derive(Debug, Clone)]
pub struct ZipEntryInfo {
    pub raw_name: String,
    pub method: u16,
    pub flags: u16,
    pub crc32: u32,
    pub compressed_size: u64,
    pub uncompressed_size: u64,
    pub local_header_offset: u64,
    pub is_dir: bool,
}

/// Bounded PKWARE ZIP reader driven by the Central Directory so data-descriptor entries
/// (written by Java's `ZipOutputStream`) and ZIP64 archives work accurately.
pub struct ZipReader<R: Read + Seek> {
    reader: R,
    archive_len: u64,
    cd_offset: u64,
    entries: Vec<ZipEntryInfo>,
}

impl<R: Read + Seek> ZipReader<R> {
    pub fn open(mut reader: R, max_entries: usize) -> Result<Self, ZipError> {
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

        let mut eocd_pos: Option<usize> = None;
        let sig_bytes = EOCD_SIG.to_le_bytes();
        let mut idx = tail.len().saturating_sub(22);
        loop {
            if tail[idx..idx + 4] == sig_bytes {
                let comment_len = u16::from_le_bytes([tail[idx + 20], tail[idx + 21]]) as usize;
                if idx + 22 + comment_len == tail.len() {
                    eocd_pos = Some(idx);
                    break;
                }
            }
            if idx == 0 {
                break;
            }
            idx -= 1;
        }

        let eocd_rel = eocd_pos.ok_or_else(|| {
            ZipError::InvalidArchive("missing ZIP end-of-central-directory record".to_owned())
        })?;
        let eocd_abs = tail_start + eocd_rel as u64;
        let eocd = &tail[eocd_rel..eocd_rel + 22];

        let disk_num = u16::from_le_bytes([eocd[4], eocd[5]]);
        let cd_start_disk = u16::from_le_bytes([eocd[6], eocd[7]]);
        let disk_entries_16 = u16::from_le_bytes([eocd[8], eocd[9]]);
        let total_entries_16 = u16::from_le_bytes([eocd[10], eocd[11]]);
        let cd_size_32 = u32::from_le_bytes([eocd[12], eocd[13], eocd[14], eocd[15]]);
        let cd_offset_32 = u32::from_le_bytes([eocd[16], eocd[17], eocd[18], eocd[19]]);

        let (total_entries, cd_size, cd_offset) = if eocd_abs >= 20 {
            let mut loc = [0u8; 20];
            reader.seek(SeekFrom::Start(eocd_abs - 20))?;
            reader.read_exact(&mut loc)?;
            if u32::from_le_bytes([loc[0], loc[1], loc[2], loc[3]]) == ZIP64_LOCATOR_SIG {
                let loc_disk = u32::from_le_bytes([loc[4], loc[5], loc[6], loc[7]]);
                let zip64_eocd_offset = u64::from_le_bytes(loc[8..16].try_into().unwrap());
                let total_disks = u32::from_le_bytes([loc[16], loc[17], loc[18], loc[19]]);
                if loc_disk != 0 || total_disks != 1 {
                    return Err(ZipError::InvalidArchive(
                        "multi-disk ZIP archives are not supported".to_owned(),
                    ));
                }
                if zip64_eocd_offset.checked_add(56).is_none_or(|e| e > eocd_abs - 20) {
                    return Err(ZipError::InvalidArchive(
                        "invalid ZIP64 end-of-central-directory offset".to_owned(),
                    ));
                }
                let mut z64 = [0u8; 56];
                reader.seek(SeekFrom::Start(zip64_eocd_offset))?;
                reader.read_exact(&mut z64)?;
                if u32::from_le_bytes([z64[0], z64[1], z64[2], z64[3]]) != ZIP64_EOCD_SIG {
                    return Err(ZipError::InvalidArchive(
                        "invalid ZIP64 end-of-central-directory signature".to_owned(),
                    ));
                }
                let record_size = u64::from_le_bytes(z64[4..12].try_into().unwrap());
                if record_size < 44 {
                    return Err(ZipError::InvalidArchive(
                        "truncated ZIP64 end-of-central-directory record".to_owned(),
                    ));
                }
                let z_disk = u32::from_le_bytes(z64[16..20].try_into().unwrap());
                let z_cd_disk = u32::from_le_bytes(z64[20..24].try_into().unwrap());
                let z_disk_entries = u64::from_le_bytes(z64[24..32].try_into().unwrap());
                let z_total_entries = u64::from_le_bytes(z64[32..40].try_into().unwrap());
                let z_cd_size = u64::from_le_bytes(z64[40..48].try_into().unwrap());
                let z_cd_offset = u64::from_le_bytes(z64[48..56].try_into().unwrap());
                if z_disk != 0 || z_cd_disk != 0 || z_disk_entries != z_total_entries {
                    return Err(ZipError::InvalidArchive(
                        "multi-disk ZIP64 archives are not supported".to_owned(),
                    ));
                }
                (z_total_entries, z_cd_size, z_cd_offset)
            } else {
                validate_standard_eocd(
                    disk_num,
                    cd_start_disk,
                    disk_entries_16,
                    total_entries_16,
                    cd_size_32,
                    cd_offset_32,
                )?
            }
        } else {
            validate_standard_eocd(
                disk_num,
                cd_start_disk,
                disk_entries_16,
                total_entries_16,
                cd_size_32,
                cd_offset_32,
            )?
        };

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
            .is_none_or(|end| end > archive_len)
        {
            return Err(ZipError::InvalidArchive(
                "central directory extends past end of archive".to_owned(),
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

            let is_dir = raw_name.ends_with('/');
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

    pub fn entries(&self) -> &[ZipEntryInfo] {
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

    pub fn read_entry_bounded(
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
                let initial_cap = (entry.uncompressed_size as usize).min(max_bytes).min(1024 * 1024);
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

    pub fn drain_ignored_entry(
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

        if (entry.flags & FLAG_ENCRYPTED) != 0 || !matches!(entry.method, METHOD_STORED | METHOD_DEFLATED) {
            *total_ignored = total_ignored.saturating_add(entry.uncompressed_size);
            check_ignored_totals(
                &entry.raw_name,
                *total_ignored,
                max_ignored,
                ratio_grace,
                max_ratio,
                self.archive_len,
            )?;
            return Ok(());
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

fn validate_standard_eocd(
    disk_num: u16,
    cd_start_disk: u16,
    disk_entries: u16,
    total_entries: u16,
    cd_size: u32,
    cd_offset: u32,
) -> Result<(u64, u64, u64), ZipError> {
    if disk_num != 0 || cd_start_disk != 0 || disk_entries != total_entries {
        return Err(ZipError::InvalidArchive(
            "multi-disk ZIP archives are not supported".to_owned(),
        ));
    }
    if total_entries == u16::MAX || cd_size == u32::MAX || cd_offset == u32::MAX {
        return Err(ZipError::InvalidArchive(
            "ZIP64 archive is missing ZIP64 locator".to_owned(),
        ));
    }
    Ok((total_entries as u64, cd_size as u64, cd_offset as u64))
}

fn resolve_entry_sizes(
    uncomp32: u32,
    comp32: u32,
    local32: u32,
    extra: &[u8],
) -> Result<(u64, u64, u64), ZipError> {
    let need_uncomp = uncomp32 == u32::MAX;
    let need_comp = comp32 == u32::MAX;
    let need_local = local32 == u32::MAX;
    if !need_uncomp && !need_comp && !need_local {
        return Ok((uncomp32 as u64, comp32 as u64, local32 as u64));
    }

    let mut pos = 0usize;
    while pos + 4 <= extra.len() {
        let tag = u16::from_le_bytes([extra[pos], extra[pos + 1]]);
        let len = u16::from_le_bytes([extra[pos + 2], extra[pos + 3]]) as usize;
        pos += 4;
        if pos + len > extra.len() {
            break;
        }
        if tag == ZIP64_EXTRA_TAG {
            let field = &extra[pos..pos + len];
            let mut fpos = 0usize;
            let uncomp = if need_uncomp {
                read_u64_le(field, &mut fpos)?
            } else {
                uncomp32 as u64
            };
            let comp = if need_comp {
                read_u64_le(field, &mut fpos)?
            } else {
                comp32 as u64
            };
            let local = if need_local {
                read_u64_le(field, &mut fpos)?
            } else {
                local32 as u64
            };
            return Ok((uncomp, comp, local));
        }
        pos += len;
    }

    Err(ZipError::InvalidArchive(
        "ZIP64 extra field missing for 64-bit entry".to_owned(),
    ))
}

fn read_u64_le(slice: &[u8], pos: &mut usize) -> Result<u64, ZipError> {
    if *pos + 8 > slice.len() {
        return Err(ZipError::InvalidArchive(
            "truncated ZIP64 extra field".to_owned(),
        ));
    }
    let val = u64::from_le_bytes(slice[*pos..*pos + 8].try_into().unwrap());
    *pos += 8;
    Ok(val)
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    /// Builds an archive where the Local File Header sets bit 3 (data descriptor) with zeroed
    /// header sizes/CRC and places the data descriptor right after the deflated payload,
    /// matching Java `ZipOutputStream` output used by Android `BackupArchive.kt`.
    fn build_java_style_data_descriptor_zip(name: &str, content: &[u8]) -> Vec<u8> {
        let crc = crc32fast::hash(content);
        let mut enc = DeflateEncoder::new(Vec::new(), Compression::default());
        enc.write_all(content).unwrap();
        let compressed = enc.finish().unwrap();

        let mut out = Vec::new();
        let lfh_offset = 0u32;
        let name_bytes = name.as_bytes();

        // Local File Header with bit 3 (0x0008) + UTF-8 (0x0800), zeroed sizes/CRC
        let flags: u16 = 0x0808;
        out.extend_from_slice(&LOCAL_FILE_HEADER_SIG.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes());
        out.extend_from_slice(&flags.to_le_bytes());
        out.extend_from_slice(&METHOD_DEFLATED.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0x0021u16.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(name_bytes);
        out.extend_from_slice(&compressed);

        // Data descriptor (PK78 optional sig + crc + comp + uncomp)
        out.extend_from_slice(&0x0807_4b50u32.to_le_bytes());
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&(compressed.len() as u32).to_le_bytes());
        out.extend_from_slice(&(content.len() as u32).to_le_bytes());

        let cd_offset = out.len() as u32;
        out.extend_from_slice(&CENTRAL_DIR_HEADER_SIG.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes());
        out.extend_from_slice(&flags.to_le_bytes());
        out.extend_from_slice(&METHOD_DEFLATED.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0x0021u16.to_le_bytes());
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&(compressed.len() as u32).to_le_bytes());
        out.extend_from_slice(&(content.len() as u32).to_le_bytes());
        out.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&lfh_offset.to_le_bytes());
        out.extend_from_slice(name_bytes);

        let cd_size = (out.len() as u32) - cd_offset;
        out.extend_from_slice(&EOCD_SIG.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&cd_size.to_le_bytes());
        out.extend_from_slice(&cd_offset.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out
    }

    #[test]
    fn roundtrips_stored_and_deflated_entries_and_detects_crc_corruption() {
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        writer
            .add_deflated("native-collections.json", br#"{"liked":["/a.flac"]}"#)
            .unwrap();
        writer
            .add_stored("covers/native_pl_c_0123456789abcdef01234567.jpg", b"\xff\xd8\xff\xe0jpg")
            .unwrap();
        let bytes = writer.finish().unwrap().into_inner();

        let mut reader = ZipReader::open(Cursor::new(&bytes), 100).unwrap();
        let entries = reader.entries().to_vec();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].method, METHOD_DEFLATED);
        assert_eq!(entries[1].method, METHOD_STORED);

        let json = reader
            .read_entry_bounded(&entries[0], 1024, 8 * 1024 * 1024, 100)
            .unwrap();
        assert_eq!(json, br#"{"liked":["/a.flac"]}"#);

        let cover = reader
            .read_entry_bounded(&entries[1], 1024, 8 * 1024 * 1024, 100)
            .unwrap();
        assert_eq!(cover, b"\xff\xd8\xff\xe0jpg");

        // Corrupt a payload byte of the stored entry -> CRC-32 mismatch must trigger.
        let mut bad_bytes = bytes;
        let offset = entries[1].local_header_offset as usize + 30 + entries[1].raw_name.len();
        bad_bytes[offset] ^= 0xFF;
        let mut bad_reader = ZipReader::open(Cursor::new(&bad_bytes), 100).unwrap();
        let bad_entry = bad_reader.entries()[1].clone();
        assert!(matches!(
            bad_reader.read_entry_bounded(&bad_entry, 1024, 8 * 1024 * 1024, 100),
            Err(ZipError::CrcMismatch { .. })
        ));
    }

    #[test]
    fn reads_java_data_descriptor_entries_via_central_directory() {
        let payload = br#"{"playlists":[{"id":1,"name":"Android Export"}]}"#;
        let zip = build_java_style_data_descriptor_zip("native-collections.json", payload);
        let mut reader = ZipReader::open(Cursor::new(zip), 100).unwrap();
        let entry = reader.entries()[0].clone();
        assert_eq!(entry.uncompressed_size, payload.len() as u64);
        let decoded = reader
            .read_entry_bounded(&entry, 4096, 8 * 1024 * 1024, 100)
            .unwrap();
        assert_eq!(decoded, payload);
    }

    #[test]
    fn rejects_zip_bomb_on_ignored_deflated_entry_and_directory_payload() {
        // 32 MiB of zeros compresses well under 100 KiB -> ratio >> 100 after 8 MiB grace.
        let zeros = vec![0u8; 32 * 1024 * 1024];
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        writer.add_deflated("sneaky/", &zeros).unwrap();
        let zip = writer.finish().unwrap().into_inner();
        assert!(zip.len() < 512 * 1024);

        let mut reader = ZipReader::open(Cursor::new(zip), 100).unwrap();
        let entry = reader.entries()[0].clone();
        assert!(entry.is_dir);
        let mut ignored = 0u64;
        let err = reader
            .drain_ignored_entry(&entry, &mut ignored, 1 << 30, 8 * 1024 * 1024, 100)
            .unwrap_err();
        assert!(matches!(err, ZipError::ZipBomb { .. }));
    }

    /// Wraps one entry in an archive using a ZIP64 EOCD record, ZIP64 locator, and 0x0001 extra field.
    fn build_zip64_single_entry(
        name: &str,
        data: &[u8],
        truncate_extra_by: usize,
    ) -> Vec<u8> {
        let crc = crc32fast::hash(data);
        let name_bytes = name.as_bytes();
        let mut out = Vec::new();

        // LFH at offset 0 (32-bit standard sizes, payload follows immediately)
        out.extend_from_slice(&LOCAL_FILE_HEADER_SIG.to_le_bytes());
        out.extend_from_slice(&45u16.to_le_bytes());
        out.extend_from_slice(&FLAG_UTF8.to_le_bytes());
        out.extend_from_slice(&METHOD_STORED.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0x0021u16.to_le_bytes());
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(name_bytes);
        out.extend_from_slice(data);

        // Build ZIP64 extra field (header 4 bytes + uncomp u64 + comp u64 + offset u64 = 28 bytes)
        let mut extra = Vec::new();
        let full_payload_len = 24usize.saturating_sub(truncate_extra_by) as u16;
        extra.extend_from_slice(&ZIP64_EXTRA_TAG.to_le_bytes());
        extra.extend_from_slice(&full_payload_len.to_le_bytes());
        let mut payload = Vec::new();
        payload.extend_from_slice(&(data.len() as u64).to_le_bytes());
        payload.extend_from_slice(&(data.len() as u64).to_le_bytes());
        payload.extend_from_slice(&0u64.to_le_bytes());
        extra.extend_from_slice(&payload[..full_payload_len as usize]);

        let cd_offset = out.len() as u64;
        out.extend_from_slice(&CENTRAL_DIR_HEADER_SIG.to_le_bytes());
        out.extend_from_slice(&0x032du16.to_le_bytes());
        out.extend_from_slice(&45u16.to_le_bytes());
        out.extend_from_slice(&FLAG_UTF8.to_le_bytes());
        out.extend_from_slice(&METHOD_STORED.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0x0021u16.to_le_bytes());
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&u32::MAX.to_le_bytes()); // ZIP64 sentinel
        out.extend_from_slice(&u32::MAX.to_le_bytes()); // ZIP64 sentinel
        out.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
        out.extend_from_slice(&(extra.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&u32::MAX.to_le_bytes()); // ZIP64 sentinel
        out.extend_from_slice(name_bytes);
        out.extend_from_slice(&extra);
        let cd_size = (out.len() as u64) - cd_offset;

        let z64_eocd_offset = out.len() as u64;
        out.extend_from_slice(&ZIP64_EOCD_SIG.to_le_bytes());
        out.extend_from_slice(&44u64.to_le_bytes());
        out.extend_from_slice(&0x032du16.to_le_bytes());
        out.extend_from_slice(&45u16.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&1u64.to_le_bytes());
        out.extend_from_slice(&1u64.to_le_bytes());
        out.extend_from_slice(&cd_size.to_le_bytes());
        out.extend_from_slice(&cd_offset.to_le_bytes());

        // ZIP64 locator
        out.extend_from_slice(&ZIP64_LOCATOR_SIG.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&z64_eocd_offset.to_le_bytes());
        out.extend_from_slice(&1u32.to_le_bytes());

        // Standard EOCD with 0xFFFF / 0xFFFFFFFF sentinels
        out.extend_from_slice(&EOCD_SIG.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&u16::MAX.to_le_bytes());
        out.extend_from_slice(&u16::MAX.to_le_bytes());
        out.extend_from_slice(&u32::MAX.to_le_bytes());
        out.extend_from_slice(&u32::MAX.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out
    }

    #[test]
    fn reads_zip64_entries_and_rejects_truncated_zip64_extra_field() {
        let payload = br#"{"zip64":true}"#;
        let valid = build_zip64_single_entry("native-collections.json", payload, 0);
        let mut reader = ZipReader::open(Cursor::new(valid), 10).unwrap();
        let entry = reader.entries()[0].clone();
        assert_eq!(entry.uncompressed_size, payload.len() as u64);
        let decoded = reader
            .read_entry_bounded(&entry, 4096, 8 * 1024 * 1024, 100)
            .unwrap();
        assert_eq!(decoded, payload);

        let truncated = build_zip64_single_entry("native-collections.json", payload, 1);
        assert!(matches!(
            ZipReader::open(Cursor::new(truncated), 10),
            Err(ZipError::InvalidArchive(msg)) if msg.contains("truncated ZIP64 extra")
        ));
    }

    #[test]
    fn accepts_eocd_with_comment_and_rejects_trailing_junk_after_eocd() {
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        writer.add_stored("a.txt", b"hello").unwrap();
        let mut base = writer.finish().unwrap().into_inner();

        // Patch EOCD comment length from 0 to 7 and append 7 comment bytes -> valid.
        let mut commented = base.clone();
        let eocd_idx = commented.len() - 22;
        commented[eocd_idx + 20..eocd_idx + 22].copy_from_slice(&7u16.to_le_bytes());
        commented.extend_from_slice(b"comment");
        let mut r = ZipReader::open(Cursor::new(commented), 10).unwrap();
        let entry = r.entries()[0].clone();
        assert_eq!(
            r.read_entry_bounded(&entry, 1024, 1024, 10).unwrap(),
            b"hello"
        );

        // Appending unrecorded trailing bytes after EOCD breaks comment_len check -> rejected.
        base.extend_from_slice(b"junk");
        assert!(matches!(
            ZipReader::open(Cursor::new(base), 10),
            Err(ZipError::InvalidArchive(_))
        ));
    }

    #[test]
    fn rejects_deflate_with_wrong_cd_uncompressed_size() {
        let mut w = ZipWriter::new(Cursor::new(Vec::new()));
        w.add_deflated("data.json", b"0123456789abcdefghij").unwrap();
        let bytes = w.finish().unwrap().into_inner();
        let probe = ZipReader::open(Cursor::new(&bytes), 10).unwrap();
        let cd_start = probe.cd_offset as usize;
        drop(probe);

        // CD `uncompressed_size` smaller than real inflated length -> ZipBomb
        let mut too_small = bytes.clone();
        too_small[cd_start + 24..cd_start + 28].copy_from_slice(&5u32.to_le_bytes());
        let mut r_small = ZipReader::open(Cursor::new(too_small), 10).unwrap();
        let e_small = r_small.entries()[0].clone();
        assert!(matches!(
            r_small.read_entry_bounded(&e_small, 4096, 4096, 100),
            Err(ZipError::ZipBomb { .. })
        ));

        // CD `uncompressed_size` larger than real inflated length -> InvalidArchive mismatch
        let mut too_big = bytes;
        too_big[cd_start + 24..cd_start + 28].copy_from_slice(&999u32.to_le_bytes());
        let mut r_big = ZipReader::open(Cursor::new(too_big), 10).unwrap();
        let e_big = r_big.entries()[0].clone();
        assert!(matches!(
            r_big.read_entry_bounded(&e_big, 4096, 4096, 100),
            Err(ZipError::InvalidArchive(msg)) if msg.contains("uncompressed size mismatch")
        ));
    }

    #[test]
    fn rejects_encrypted_and_unsupported_methods_on_read_and_bounds_on_drain() {
        let mut w = ZipWriter::new(Cursor::new(Vec::new()));
        w.add_stored("enc.bin", b"secret").unwrap();
        w.add_stored("algo.bin", b"custom").unwrap();
        let mut bytes = w.finish().unwrap().into_inner();
        let probe = ZipReader::open(Cursor::new(&bytes), 10).unwrap();
        let cd_start = probe.cd_offset as usize;
        drop(probe);

        // Set FLAG_ENCRYPTED on entry 0
        bytes[cd_start + 8..cd_start + 10].copy_from_slice(&(FLAG_UTF8 | FLAG_ENCRYPTED).to_le_bytes());
        // Locate second CD header (46 + len("enc.bin") = 53) and set method 99
        let cd2 = cd_start + 46 + "enc.bin".len();
        bytes[cd2 + 10..cd2 + 12].copy_from_slice(&99u16.to_le_bytes());

        let mut reader = ZipReader::open(Cursor::new(bytes), 10).unwrap();
        let enc_entry = reader.entries()[0].clone();
        let custom_entry = reader.entries()[1].clone();

        assert!(matches!(
            reader.read_entry_bounded(&enc_entry, 4096, 4096, 100),
            Err(ZipError::EncryptedEntry { .. })
        ));
        assert!(matches!(
            reader.read_entry_bounded(&custom_entry, 4096, 4096, 100),
            Err(ZipError::UnsupportedCompression { method: 99, .. })
        ));

        let mut ignored = 0u64;
        reader
            .drain_ignored_entry(&enc_entry, &mut ignored, 1024, 4096, 100)
            .unwrap();
        reader
            .drain_ignored_entry(&custom_entry, &mut ignored, 1024, 4096, 100)
            .unwrap();
        assert_eq!(ignored, 12);

        // Tight max_ignored rejects ignored encrypted/unknown entries as ZipBomb.
        let mut tight = 0u64;
        assert!(matches!(
            reader.drain_ignored_entry(&custom_entry, &mut tight, 3, 4096, 100),
            Err(ZipError::ZipBomb { .. })
        ));
    }

    #[test]
    fn rejects_local_header_cd_overlap_and_stored_size_mismatch() {
        let mut w = ZipWriter::new(Cursor::new(Vec::new()));
        w.add_stored("a.bin", b"123456").unwrap();
        let bytes = w.finish().unwrap().into_inner();
        let probe = ZipReader::open(Cursor::new(&bytes), 10).unwrap();
        let cd_start = probe.cd_offset as u32;
        drop(probe);

        // Local header offset overlapping Central Directory -> rejected in open()
        let mut overlap = bytes.clone();
        overlap[cd_start as usize + 42..cd_start as usize + 46]
            .copy_from_slice(&cd_start.to_le_bytes());
        assert!(matches!(
            ZipReader::open(Cursor::new(overlap), 10),
            Err(ZipError::InvalidArchive(msg)) if msg.contains("overlaps central directory")
        ));

        // STORED entry with uncompressed_size != compressed_size -> rejected by read & drain
        let mut bad_stored = bytes;
        bad_stored[cd_start as usize + 24..cd_start as usize + 28]
            .copy_from_slice(&12u32.to_le_bytes());
        let mut reader = ZipReader::open(Cursor::new(bad_stored), 10).unwrap();
        let entry = reader.entries()[0].clone();
        assert!(matches!(
            reader.read_entry_bounded(&entry, 4096, 4096, 100),
            Err(ZipError::InvalidArchive(msg)) if msg.contains("mismatched sizes")
        ));
        let mut ignored = 0u64;
        assert!(matches!(
            reader.drain_ignored_entry(&entry, &mut ignored, 4096, 4096, 100),
            Err(ZipError::InvalidArchive(msg)) if msg.contains("mismatched sizes")
        ));
    }
}
