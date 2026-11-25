use std::io::Cursor;

use super::super::*;

fn push_u16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_le_bytes());
}
fn push_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}
fn push_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn build_zip64_single_entry(name: &str, data: &[u8], truncate_extra_by: usize) -> Vec<u8> {
    let crc = crc32fast::hash(data);
    let nb = name.as_bytes();
    let mut out = Vec::new();

    push_u32(&mut out, LOCAL_FILE_HEADER_SIG);
    push_u16(&mut out, 45);
    push_u16(&mut out, FLAG_UTF8);
    push_u16(&mut out, METHOD_STORED);
    push_u16(&mut out, 0);
    push_u16(&mut out, 0x0021);
    push_u32(&mut out, crc);
    push_u32(&mut out, data.len() as u32);
    push_u32(&mut out, data.len() as u32);
    push_u16(&mut out, nb.len() as u16);
    push_u16(&mut out, 0);
    out.extend_from_slice(nb);
    out.extend_from_slice(data);

    let full_payload_len = 24usize.saturating_sub(truncate_extra_by) as u16;
    let mut payload = Vec::new();
    push_u64(&mut payload, data.len() as u64);
    push_u64(&mut payload, data.len() as u64);
    push_u64(&mut payload, 0);
    let mut extra = Vec::new();
    push_u16(&mut extra, ZIP64_EXTRA_TAG);
    push_u16(&mut extra, full_payload_len);
    extra.extend_from_slice(&payload[..full_payload_len as usize]);

    let cd_offset = out.len() as u64;
    push_u32(&mut out, CENTRAL_DIR_HEADER_SIG);
    push_u16(&mut out, 0x032d);
    push_u16(&mut out, 45);
    push_u16(&mut out, FLAG_UTF8);
    push_u16(&mut out, METHOD_STORED);
    push_u16(&mut out, 0);
    push_u16(&mut out, 0x0021);
    push_u32(&mut out, crc);
    push_u32(&mut out, u32::MAX);
    push_u32(&mut out, u32::MAX);
    push_u16(&mut out, nb.len() as u16);
    push_u16(&mut out, extra.len() as u16);
    out.extend_from_slice(&[0u8; 10]);
    push_u32(&mut out, u32::MAX);
    out.extend_from_slice(nb);
    out.extend_from_slice(&extra);
    let cd_size = (out.len() as u64) - cd_offset;

    let z64_eocd_offset = out.len() as u64;
    push_u32(&mut out, ZIP64_EOCD_SIG);
    push_u64(&mut out, 44);
    push_u16(&mut out, 0x032d);
    push_u16(&mut out, 45);
    push_u64(&mut out, 0);
    push_u64(&mut out, 1);
    push_u64(&mut out, 1);
    push_u64(&mut out, cd_size);
    push_u64(&mut out, cd_offset);

    push_u32(&mut out, ZIP64_LOCATOR_SIG);
    push_u32(&mut out, 0);
    push_u64(&mut out, z64_eocd_offset);
    push_u32(&mut out, 1);

    push_u32(&mut out, EOCD_SIG);
    push_u32(&mut out, 0);
    push_u16(&mut out, u16::MAX);
    push_u16(&mut out, u16::MAX);
    push_u32(&mut out, u32::MAX);
    push_u32(&mut out, u32::MAX);
    push_u16(&mut out, 0);
    out
}

#[test]
fn roundtrips_stored_and_deflated_entries_and_detects_crc_corruption() {
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    writer
        .add_deflated("native-collections.json", br#"{"liked":["/a.flac"]}"#)
        .unwrap();
    writer
        .add_stored(
            "covers/native_pl_c_0123456789abcdef01234567.jpg",
            b"\xff\xd8\xff\xe0jpg",
        )
        .unwrap();
    let bytes = writer.finish().unwrap().into_inner();

    let mut reader = ZipReader::open(Cursor::new(&bytes), 100).unwrap();
    let entries = reader.entries().to_vec();
    assert_eq!(entries.len(), 2);
    assert_eq!(
        reader
            .read_entry_bounded(&entries[0], 1024, 8 * 1024 * 1024, 100)
            .unwrap(),
        br#"{"liked":["/a.flac"]}"#
    );
    assert_eq!(
        reader
            .read_entry_bounded(&entries[1], 1024, 8 * 1024 * 1024, 100)
            .unwrap(),
        b"\xff\xd8\xff\xe0jpg"
    );

    let mut bad = bytes;
    let offset = entries[1].local_header_offset as usize + 30 + entries[1].raw_name.len();
    bad[offset] ^= 0xFF;
    let mut bad_reader = ZipReader::open(Cursor::new(&bad), 100).unwrap();
    let bad_entry = bad_reader.entries()[1].clone();
    assert!(matches!(
        bad_reader.read_entry_bounded(&bad_entry, 1024, 8 * 1024 * 1024, 100),
        Err(ZipError::CrcMismatch { .. })
    ));
}

#[test]
fn reads_java_data_descriptor_entries_via_central_directory() {
    let payload = br#"{"playlists":[{"id":1,"name":"Android Export"}]}"#;
    let zip = build_data_descriptor_zip(&[("native-collections.json", payload, METHOD_DEFLATED)]);
    let mut reader = ZipReader::open(Cursor::new(zip), 100).unwrap();
    let entry = reader.entries()[0].clone();
    assert_eq!(entry.uncompressed_size, payload.len() as u64);
    assert_eq!(
        reader
            .read_entry_bounded(&entry, 4096, 8 * 1024 * 1024, 100)
            .unwrap(),
        payload
    );
}

#[test]
fn rejects_zip_bomb_on_ignored_deflated_entry_and_directory_payload() {
    let zeros = vec![0u8; 16 * 1024 * 1024];
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    writer.add_deflated("sneaky/", &zeros).unwrap();
    let zip = writer.finish().unwrap().into_inner();
    let mut reader = ZipReader::open(Cursor::new(zip), 100).unwrap();
    let entry = reader.entries()[0].clone();
    assert!(entry.is_dir);
    let mut ignored = 0u64;
    assert!(matches!(
        reader.drain_ignored_entry(&entry, &mut ignored, 1 << 30, 8 * 1024 * 1024, 100),
        Err(ZipError::ZipBomb { .. })
    ));
}

#[test]
fn reads_zip64_entries_and_rejects_truncated_zip64_extra_field() {
    let payload = br#"{"zip64":true}"#;
    let valid = build_zip64_single_entry("native-collections.json", payload, 0);
    let mut reader = ZipReader::open(Cursor::new(valid), 10).unwrap();
    let entry = reader.entries()[0].clone();
    assert_eq!(
        reader
            .read_entry_bounded(&entry, 4096, 8 * 1024 * 1024, 100)
            .unwrap(),
        payload
    );

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

    base.extend_from_slice(b"junk");
    assert!(matches!(
        ZipReader::open(Cursor::new(base), 10),
        Err(ZipError::InvalidArchive(_))
    ));
}

#[test]
fn rejects_deflate_with_wrong_cd_uncompressed_size() {
    let mut w = ZipWriter::new(Cursor::new(Vec::new()));
    w.add_deflated("data.json", b"0123456789abcdefghij")
        .unwrap();
    let bytes = w.finish().unwrap().into_inner();
    let cd_start = ZipReader::open(Cursor::new(&bytes), 10).unwrap().cd_offset as usize;

    let mut too_small = bytes.clone();
    too_small[cd_start + 24..cd_start + 28].copy_from_slice(&5u32.to_le_bytes());
    let mut r_small = ZipReader::open(Cursor::new(too_small), 10).unwrap();
    let e_small = r_small.entries()[0].clone();
    assert!(matches!(
        r_small.read_entry_bounded(&e_small, 4096, 4096, 100),
        Err(ZipError::ZipBomb { .. })
    ));

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
    let cd_start = ZipReader::open(Cursor::new(&bytes), 10).unwrap().cd_offset as usize;

    bytes[cd_start + 8..cd_start + 10].copy_from_slice(&(FLAG_UTF8 | FLAG_ENCRYPTED).to_le_bytes());
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
    let cd_start = ZipReader::open(Cursor::new(&bytes), 10).unwrap().cd_offset as u32;

    let mut overlap = bytes.clone();
    overlap[cd_start as usize + 42..cd_start as usize + 46]
        .copy_from_slice(&cd_start.to_le_bytes());
    assert!(matches!(
        ZipReader::open(Cursor::new(overlap), 10),
        Err(ZipError::InvalidArchive(msg)) if msg.contains("overlaps central directory")
    ));

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
