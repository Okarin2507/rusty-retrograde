use std::io::Write;

use flate2::Compression;
use flate2::write::DeflateEncoder;

use super::{
    CENTRAL_DIR_HEADER_SIG, EOCD_SIG, FLAG_UTF8, LOCAL_FILE_HEADER_SIG, METHOD_DEFLATED,
    METHOD_STORED, ZipError,
};

struct WrittenEntry {
    name: Vec<u8>,
    method: u16,
    crc32: u32,
    compressed_size: u32,
    uncompressed_size: u32,
    local_header_offset: u32,
}

fn checked_non_sentinel_u32(val: usize, msg: &str) -> Result<u32, ZipError> {
    u32::try_from(val)
        .ok()
        .filter(|&n| n < u32::MAX)
        .ok_or_else(|| ZipError::InvalidArchive(msg.to_owned()))
}

fn checked_non_sentinel_u64_to_u32(val: u64, msg: &str) -> Result<u32, ZipError> {
    u32::try_from(val)
        .ok()
        .filter(|&n| n < u32::MAX)
        .ok_or_else(|| ZipError::InvalidArchive(msg.to_owned()))
}

/// Minimal PKWARE-compliant ZIP writer supporting STORED (method 0) and DEFLATE (method 8).
pub(crate) struct ZipWriter<W: Write> {
    writer: W,
    offset: u64,
    entries: Vec<WrittenEntry>,
}

impl<W: Write> ZipWriter<W> {
    pub(crate) fn new(writer: W) -> Self {
        Self {
            writer,
            offset: 0,
            entries: Vec::new(),
        }
    }

    pub(crate) fn add_stored(&mut self, name: &str, data: &[u8]) -> Result<(), ZipError> {
        self.write_entry(name, METHOD_STORED, data, data)
    }

    pub(crate) fn add_deflated(&mut self, name: &str, data: &[u8]) -> Result<(), ZipError> {
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
        // Standard 32-bit ZIP reserves `u16::MAX` as the ZIP64 sentinel.
        if self.entries.len() >= u16::MAX as usize - 1 {
            return Err(ZipError::TooManyEntries {
                limit: u16::MAX as usize - 1,
            });
        }
        let name_bytes = name.as_bytes();
        let name_len = u16::try_from(name_bytes.len())
            .map_err(|_| ZipError::InvalidArchive("entry name exceeds 64 KiB".to_owned()))?;
        let uncompressed_size =
            checked_non_sentinel_u32(uncompressed.len(), "entry exceeds 4 GiB ZIP limit")?;
        let compressed_size =
            checked_non_sentinel_u32(payload.len(), "entry exceeds 4 GiB ZIP limit")?;
        let local_header_offset =
            checked_non_sentinel_u64_to_u32(self.offset, "archive exceeds 4 GiB ZIP limit")?;

        let crc32 = crc32fast::hash(uncompressed);

        let mut header = [0u8; 30];
        header[0..4].copy_from_slice(&LOCAL_FILE_HEADER_SIG.to_le_bytes());
        header[4..6].copy_from_slice(&20u16.to_le_bytes());
        header[6..8].copy_from_slice(&FLAG_UTF8.to_le_bytes());
        header[8..10].copy_from_slice(&method.to_le_bytes());
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

    pub(crate) fn finish(mut self) -> Result<W, ZipError> {
        let cd_offset =
            checked_non_sentinel_u64_to_u32(self.offset, "archive exceeds 4 GiB ZIP limit")?;
        let entry_count = u16::try_from(self.entries.len())
            .ok()
            .filter(|&n| n < u16::MAX)
            .ok_or(ZipError::TooManyEntries {
                limit: u16::MAX as usize - 1,
            })?;

        let mut cd_size_u64 = 0u64;
        for entry in &self.entries {
            let name_len = entry.name.len() as u16;
            let mut cd = [0u8; 46];
            cd[0..4].copy_from_slice(&CENTRAL_DIR_HEADER_SIG.to_le_bytes());
            cd[4..6].copy_from_slice(&0x0314u16.to_le_bytes());
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

        let cd_size =
            checked_non_sentinel_u64_to_u32(cd_size_u64, "central directory exceeds 4 GiB")?;

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
