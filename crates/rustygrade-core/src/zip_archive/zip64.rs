use std::io::{Read, Seek, SeekFrom};

use super::{EOCD_SIG, ZIP64_EOCD_SIG, ZIP64_EXTRA_TAG, ZIP64_LOCATOR_SIG, ZipError};

pub(super) fn parse_eocd_and_zip64<R: Read + Seek>(
    reader: &mut R,
    eocd: &[u8],
    eocd_abs: u64,
) -> Result<(u64, u64, u64), ZipError> {
    let disk_num = u16::from_le_bytes([eocd[4], eocd[5]]);
    let cd_start_disk = u16::from_le_bytes([eocd[6], eocd[7]]);
    let disk_entries_16 = u16::from_le_bytes([eocd[8], eocd[9]]);
    let total_entries_16 = u16::from_le_bytes([eocd[10], eocd[11]]);
    let cd_size_32 = u32::from_le_bytes([eocd[12], eocd[13], eocd[14], eocd[15]]);
    let cd_offset_32 = u32::from_le_bytes([eocd[16], eocd[17], eocd[18], eocd[19]]);

    if eocd_abs >= 20 {
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
            if zip64_eocd_offset
                .checked_add(56)
                .is_none_or(|e| e > eocd_abs - 20)
            {
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
            return Ok((z_total_entries, z_cd_size, z_cd_offset));
        }
    }

    validate_standard_eocd(
        disk_num,
        cd_start_disk,
        disk_entries_16,
        total_entries_16,
        cd_size_32,
        cd_offset_32,
    )
}

pub(super) fn find_eocd_offset(tail: &[u8]) -> Option<usize> {
    let sig_bytes = EOCD_SIG.to_le_bytes();
    let mut idx = tail.len().saturating_sub(22);
    loop {
        if tail[idx..idx + 4] == sig_bytes {
            let comment_len = u16::from_le_bytes([tail[idx + 20], tail[idx + 21]]) as usize;
            if idx + 22 + comment_len == tail.len() {
                return Some(idx);
            }
        }
        if idx == 0 {
            return None;
        }
        idx -= 1;
    }
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

pub(super) fn resolve_entry_sizes(
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
