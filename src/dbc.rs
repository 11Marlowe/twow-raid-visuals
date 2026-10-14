use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;

/// Representation of a vanilla DBC header (WDBC).  The parser does not
/// support the extended WDB2/WDB5 variants and assumes a simple fixed
/// record layout.
#[derive(Debug, Clone)]
pub struct DbcHeader {
    pub magic: [u8; 4],
    pub record_count: u32,
    pub field_count: u32,
    pub record_size: u32,
    pub string_block_size: u32,
}

impl DbcHeader {
    /// Size of the header in bytes (magic + 4 u32s)
    pub const SIZE: usize = 4 + 4 * 4;
}

/// On-disk storage type of a single DBC field.
///
/// The vast majority of vanilla DBCs store every field as a 32-bit value.
/// A few tables (`CharBaseInfo.dbc`, `CharStartOutfit.dbc`) pack 8-bit
/// fields alongside 32-bit ones, which requires an explicit typed schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldType {
    /// 1-byte unsigned integer.
    U8,
    /// 4-byte unsigned integer (also used for string offsets and keys).
    U32,
    /// 4-byte IEEE-754 float, stored as the raw bit pattern.
    F32,
}

impl FieldType {
    /// Number of bytes this field occupies in a record.
    pub fn width(self) -> usize {
        match self {
            FieldType::U8 => 1,
            FieldType::U32 | FieldType::F32 => 4,
        }
    }
}

/// Resolve the per-field byte widths for a record.
///
/// Vanilla DBCs normally store every field as a 32-bit value
/// (`record_size == field_count * 4`).  When the header declares a packed
/// record layout, a typed schema covering every field is required.
fn record_field_widths(
    record_size: u32,
    field_count: u32,
    field_types: Option<&[FieldType]>,
) -> Result<Vec<usize>> {
    if record_size == field_count * 4 {
        return Ok(vec![4; field_count as usize]);
    }
    let types = field_types.with_context(|| {
        format!(
            "DBC has packed records ({} bytes for {} fields) but no typed schema was provided; \
             add a <Table>.dbc.yaml schema that lists every field with its type (u8/u32/f32)",
            record_size, field_count
        )
    })?;
    if types.len() != field_count as usize {
        bail!(
            "Typed schema lists {} fields but the DBC header declares {}",
            types.len(),
            field_count
        );
    }
    let widths: Vec<usize> = types.iter().map(|t| t.width()).collect();
    let total: usize = widths.iter().sum();
    if total != record_size as usize {
        bail!(
            "Typed schema describes {} bytes per record but the DBC header declares {}",
            total,
            record_size
        );
    }
    Ok(widths)
}

/// Read a DBC file from disk.  Returns the parsed header, a vector of
/// records (each record is a vector of 32-bit values) and the raw string
/// block.
///
/// `field_types` is only consulted when the record is not a plain array of
/// 32-bit values; pass `None` for uniform DBCs (or when no schema is
/// available).
pub fn read_dbc<P: AsRef<Path>>(
    path: P,
    field_types: Option<&[FieldType]>,
) -> Result<(DbcHeader, Vec<Vec<u32>>, Vec<u8>)> {
    let mut file = File::open(&path)
        .with_context(|| format!("Failed to open DBC file {:?}", path.as_ref()))?;

    // Read header
    let mut header_bytes = [0u8; DbcHeader::SIZE];
    file.read_exact(&mut header_bytes)
        .with_context(|| "Failed to read DBC header")?;
    let magic = [header_bytes[0], header_bytes[1], header_bytes[2], header_bytes[3]];
    let record_count = u32::from_le_bytes([
        header_bytes[4], header_bytes[5], header_bytes[6], header_bytes[7],
    ]);
    let field_count = u32::from_le_bytes([
        header_bytes[8], header_bytes[9], header_bytes[10], header_bytes[11],
    ]);
    let record_size = u32::from_le_bytes([
        header_bytes[12], header_bytes[13], header_bytes[14], header_bytes[15],
    ]);
    let string_block_size = u32::from_le_bytes([
        header_bytes[16], header_bytes[17], header_bytes[18], header_bytes[19],
    ]);
    let header = DbcHeader {
        magic,
        record_count,
        field_count,
        record_size,
        string_block_size,
    };

    // Resolve the on-disk width of every field (4 bytes for uniform DBCs).
    let widths = record_field_widths(header.record_size, header.field_count, field_types)?;

    // Read record data
    let mut records: Vec<Vec<u32>> = Vec::with_capacity(header.record_count as usize);
    for _ in 0..header.record_count {
        let mut record_bytes = vec![0u8; header.record_size as usize];
        file.read_exact(&mut record_bytes)
            .with_context(|| "Failed to read record")?;
        // Split into u32 values according to the field widths
        let mut values: Vec<u32> = Vec::with_capacity(header.field_count as usize);
        let mut offset = 0usize;
        for &width in &widths {
            let val = match width {
                1 => record_bytes[offset] as u32,
                _ => u32::from_le_bytes([
                    record_bytes[offset],
                    record_bytes[offset + 1],
                    record_bytes[offset + 2],
                    record_bytes[offset + 3],
                ]),
            };
            offset += width;
            values.push(val);
        }
        records.push(values);
    }

    // Read string block
    let mut string_block = vec![0u8; header.string_block_size as usize];
    file.read_exact(&mut string_block)
        .with_context(|| "Failed to read string block")?;

    Ok((header, records, string_block))
}

/// Write a DBC file to disk.  Takes the header for field count/record size,
/// the records to write and the final string block.  The record count and
/// string block size are recomputed automatically.
///
/// `field_types` must match the schema used when reading packed records
/// (ignored for uniform 32-bit layouts).
pub fn write_dbc<P: AsRef<Path>>(
    path: P,
    header: &DbcHeader,
    records: &[Vec<u32>],
    string_block: &[u8],
    field_types: Option<&[FieldType]>,
) -> Result<()> {
    let mut file = File::create(&path)
        .with_context(|| format!("Failed to create output DBC file {:?}", path.as_ref()))?;
    // Recalculate header fields
    let record_count = records.len() as u32;
    let field_count = header.field_count;
    let record_size = header.record_size;
    let string_block_size = string_block.len() as u32;

    let widths = record_field_widths(record_size, field_count, field_types)?;

    // Write header
    file.write_all(&header.magic)
        .context("Failed to write DBC magic")?;
    file.write_all(&record_count.to_le_bytes())
        .context("Failed to write record count")?;
    file.write_all(&field_count.to_le_bytes())
        .context("Failed to write field count")?;
    file.write_all(&record_size.to_le_bytes())
        .context("Failed to write record size")?;
    file.write_all(&string_block_size.to_le_bytes())
        .context("Failed to write string block size")?;

    // Write records
    for record in records {
        // Ensure the record has the correct number of fields
        if record.len() != field_count as usize {
            bail!("Record length mismatch: expected {} fields, got {}", field_count, record.len());
        }
        for (i, &value) in record.iter().enumerate() {
            let writer = &mut file;
            match widths[i] {
                1 => {
                    if value > u8::MAX as u32 {
                        bail!(
                            "Value {} does not fit in the 1-byte field {} of a record",
                            value,
                            i
                        );
                    }
                    writer
                        .write_all(&[value as u8])
                        .context("Failed to write record field")?;
                }
                _ => {
                    writer
                        .write_all(&value.to_le_bytes())
                        .context("Failed to write record field")?;
                }
            }
        }
    }

    // Write string block
    file.write_all(string_block)
        .context("Failed to write string block")?;
    Ok(())
}

/// Build a mapping of strings to their offsets from an existing string block.
/// Offsets are 0‑based relative to the start of the block.  The empty string
/// at offset 0 is always included.
pub fn build_string_map(block: &[u8]) -> HashMap<String, u32> {
    let mut map = HashMap::new();
    // let mut offset = 0u32;
    let mut start = 0usize;
    while start < block.len() {
        // Find the next null terminator
        if let Some(pos) = block[start..].iter().position(|&b| b == 0) {
            let end = start + pos;
            let string_bytes = &block[start..end];
            let s = String::from_utf8_lossy(string_bytes).to_string();
            map.insert(s, start as u32);
            // Move past the terminator
            start = end + 1;
        } else {
            break;
        }
    }
    // Ensure empty string at offset 0
    map.entry(String::new()).or_insert(0);
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Write a synthetic DBC file from raw per-record bytes.
    fn write_raw(
        path: &Path,
        field_count: u32,
        record_size: u32,
        records: &[Vec<u8>],
        string_block: &[u8],
    ) {
        let mut buf: Vec<u8> = Vec::new();
        buf.extend_from_slice(b"WDBC");
        buf.extend_from_slice(&(records.len() as u32).to_le_bytes());
        buf.extend_from_slice(&field_count.to_le_bytes());
        buf.extend_from_slice(&record_size.to_le_bytes());
        buf.extend_from_slice(&(string_block.len() as u32).to_le_bytes());
        for record in records {
            buf.extend_from_slice(record);
        }
        buf.extend_from_slice(string_block);
        std::fs::write(path, buf).unwrap();
    }

    fn temp_path(name: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("wow_dbc_patcher_test_{}.dbc", name));
        let _ = std::fs::remove_file(&path);
        path
    }

    #[test]
    fn packed_u8_round_trip() {
        // CharBaseInfo layout: 2 x u8 (2-byte records)
        let path = temp_path("charbaseinfo");
        let records = vec![
            vec![1u8, 5u8],
            vec![2u8, 9u8],
        ];
        write_raw(&path, 2, 2, &records, &[0]);

        let types = [FieldType::U8, FieldType::U8];
        let (header, decoded, strings) = read_dbc(&path, Some(&types)).unwrap();
        assert_eq!(header.field_count, 2);
        assert_eq!(header.record_size, 2);
        assert_eq!(decoded, vec![vec![1u32, 5u32], vec![2u32, 9u32]]);

        let out = temp_path("charbaseinfo_out");
        write_dbc(&out, &header, &decoded, &strings, Some(&types)).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), std::fs::read(&out).unwrap());

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&out);
    }

    #[test]
    fn packed_mixed_width_round_trip() {
        // CharStartOutfit-like layout: u8, u32, u8 (6-byte records)
        let path = temp_path("mixed");
        let records = vec![
            vec![7u8, 0xEF, 0xBE, 0xAD, 0xDE, 9u8],
            vec![1u8, 0x01, 0x00, 0x00, 0x00, 255u8],
        ];
        write_raw(&path, 3, 6, &records, &[0]);

        let types = [FieldType::U8, FieldType::U32, FieldType::U8];
        let (header, decoded, strings) = read_dbc(&path, Some(&types)).unwrap();
        assert_eq!(decoded, vec![vec![7u32, 0xDEADBEEF, 9u32], vec![1u32, 1u32, 255u32]]);

        let out = temp_path("mixed_out");
        write_dbc(&out, &header, &decoded, &strings, Some(&types)).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), std::fs::read(&out).unwrap());

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&out);
    }

    #[test]
    fn uniform_layout_needs_no_types() {
        let path = temp_path("uniform");
        let records = vec![vec![1u8, 0, 0, 0, 2u8, 0, 0, 0]];
        write_raw(&path, 2, 8, &records, &[0]);

        let (header, decoded, strings) = read_dbc(&path, None).unwrap();
        assert_eq!(decoded, vec![vec![1u32, 2u32]]);

        let out = temp_path("uniform_out");
        write_dbc(&out, &header, &decoded, &strings, None).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), std::fs::read(&out).unwrap());

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&out);
    }

    #[test]
    fn packed_layout_requires_types() {
        let path = temp_path("needs_types");
        write_raw(&path, 2, 2, &[vec![1u8, 2u8]], &[0]);
        let err = read_dbc(&path, None).unwrap_err();
        assert!(err.to_string().contains("packed records"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn rejects_u8_overflow() {
        let path = temp_path("overflow");
        let header = DbcHeader {
            magic: *b"WDBC",
            record_count: 1,
            field_count: 1,
            record_size: 1,
            string_block_size: 1,
        };
        let types = [FieldType::U8];
        let err = write_dbc(&path, &header, &[vec![300u32]], &[0], Some(&types)).unwrap_err();
        assert!(err.to_string().contains("does not fit"));
        let _ = std::fs::remove_file(&path);
    }
}
