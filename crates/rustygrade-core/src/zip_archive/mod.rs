use std::io;

use thiserror::Error;

pub(crate) mod reader;
pub(crate) mod writer;
mod zip64;

pub(crate) use reader::ZipReader;
pub(crate) use writer::ZipWriter;

pub(crate) const LOCAL_FILE_HEADER_SIG: u32 = 0x0403_4b50;
pub(crate) const CENTRAL_DIR_HEADER_SIG: u32 = 0x0201_4b50;
pub(crate) const EOCD_SIG: u32 = 0x0605_4b50;
pub(crate) const ZIP64_EOCD_SIG: u32 = 0x0606_4b50;
pub(crate) const ZIP64_LOCATOR_SIG: u32 = 0x0706_4b50;
pub(crate) const ZIP64_EXTRA_TAG: u16 = 0x0001;

pub(crate) const FLAG_ENCRYPTED: u16 = 0x0001;
pub(crate) const FLAG_UTF8: u16 = 0x0800;

pub const METHOD_STORED: u16 = 0;
pub const METHOD_DEFLATED: u16 = 8;

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

#[derive(Debug, Clone)]
pub(crate) struct ZipEntryInfo {
    pub raw_name: String,
    pub method: u16,
    pub flags: u16,
    pub crc32: u32,
    pub compressed_size: u64,
    pub uncompressed_size: u64,
    pub local_header_offset: u64,
    pub is_dir: bool,
}

#[cfg(test)]
pub(crate) fn build_data_descriptor_zip(entries: &[(&str, &[u8], u16)]) -> Vec<u8> {
    use flate2::Compression;
    use flate2::write::DeflateEncoder;
    use std::io::Write;

    struct CdMeta {
        name: Vec<u8>,
        method: u16,
        flags: u16,
        crc: u32,
        comp: u32,
        uncomp: u32,
        off: u32,
    }

    let mut out = Vec::new();
    let mut cds = Vec::with_capacity(entries.len());
    for &(name, raw, method) in entries {
        let crc = crc32fast::hash(raw);
        let payload = if method == METHOD_DEFLATED {
            let mut enc = DeflateEncoder::new(Vec::new(), Compression::default());
            enc.write_all(raw).unwrap();
            enc.finish().unwrap()
        } else {
            raw.to_vec()
        };
        let off = out.len() as u32;
        let flags: u16 = if method == METHOD_DEFLATED {
            0x0808
        } else {
            0x0800
        };
        let nb = name.as_bytes();
        out.extend_from_slice(&LOCAL_FILE_HEADER_SIG.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes());
        out.extend_from_slice(&flags.to_le_bytes());
        out.extend_from_slice(&method.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&(if method == METHOD_DEFLATED { 0 } else { crc }).to_le_bytes());
        out.extend_from_slice(
            &(if method == METHOD_DEFLATED {
                0
            } else {
                payload.len() as u32
            })
            .to_le_bytes(),
        );
        out.extend_from_slice(
            &(if method == METHOD_DEFLATED {
                0
            } else {
                raw.len() as u32
            })
            .to_le_bytes(),
        );
        out.extend_from_slice(&(nb.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(nb);
        out.extend_from_slice(&payload);
        if method == METHOD_DEFLATED {
            out.extend_from_slice(&0x0807_4b50u32.to_le_bytes());
            out.extend_from_slice(&crc.to_le_bytes());
            out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            out.extend_from_slice(&(raw.len() as u32).to_le_bytes());
        }
        cds.push(CdMeta {
            name: nb.to_vec(),
            method,
            flags,
            crc,
            comp: payload.len() as u32,
            uncomp: raw.len() as u32,
            off,
        });
    }
    let cd_off = out.len() as u32;
    for c in &cds {
        out.extend_from_slice(&CENTRAL_DIR_HEADER_SIG.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes());
        out.extend_from_slice(&c.flags.to_le_bytes());
        out.extend_from_slice(&c.method.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&c.crc.to_le_bytes());
        out.extend_from_slice(&c.comp.to_le_bytes());
        out.extend_from_slice(&c.uncomp.to_le_bytes());
        out.extend_from_slice(&(c.name.len() as u16).to_le_bytes());
        out.extend_from_slice(&[0u8; 12]);
        out.extend_from_slice(&c.off.to_le_bytes());
        out.extend_from_slice(&c.name);
    }
    let cd_size = (out.len() as u32) - cd_off;
    let cnt = cds.len() as u16;
    out.extend_from_slice(&EOCD_SIG.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&cnt.to_le_bytes());
    out.extend_from_slice(&cnt.to_le_bytes());
    out.extend_from_slice(&cd_size.to_le_bytes());
    out.extend_from_slice(&cd_off.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out
}
