//! FTVI — FrankenTerm Vector Index binary format.
//!
//! Layout: [magic:4][version:2][dimension:2][count:4][records...]
//! Each record: [id:8][f16 vector: dimension*2 bytes]
//!
//! Uses 8-lane unrolled dot product for search and IEEE 754 half-precision (f16).

use std::io::{self, Read, Write};

const MAGIC: &[u8; 4] = b"FTVI";
const VERSION: u16 = 1;

/// Convert f32 → f16 (IEEE 754 binary16).
fn f32_to_f16(val: f32) -> u16 {
    let bits = val.to_bits();
    let sign = (bits >> 16) & 0x8000;
    let exponent = ((bits >> 23) & 0xFF) as i32;
    let mantissa = bits & 0x007FFFFF;

    if exponent == 255 {
        // Inf or NaN
        return (sign | 0x7C00 | if mantissa != 0 { 0x0200 } else { 0 }) as u16;
    }

    let new_exp = exponent - 127 + 15;
    if new_exp >= 31 {
        return (sign | 0x7C00) as u16; // overflow → Inf
    }
    if new_exp <= 0 {
        if new_exp < -10 {
            return sign as u16; // underflow → ±0
        }
        let m = (mantissa | 0x00800000) >> (1 - new_exp);
        return (sign | (m >> 13)) as u16;
    }
    (sign | ((new_exp as u32) << 10) | (mantissa >> 13)) as u16
}

/// Convert f16 → f32.
fn f16_to_f32(half: u16) -> f32 {
    let sign = ((half as u32) & 0x8000) << 16;
    let exponent = ((half as u32) >> 10) & 0x1F;
    let mantissa = (half as u32) & 0x03FF;

    if exponent == 0 {
        if mantissa == 0 {
            return f32::from_bits(sign); // ±0
        }
        // Denormalized
        let mut m = mantissa;
        let mut e = 1u32;
        while m & 0x0400 == 0 {
            m <<= 1;
            e += 1;
        }
        let exp = (127 - 15 + 1 - e) << 23;
        let man = (m & 0x03FF) << 13;
        return f32::from_bits(sign | exp | man);
    }
    if exponent == 31 {
        let bits = sign | 0x7F800000 | (mantissa << 13);
        return f32::from_bits(bits);
    }
    let exp = (exponent + 127 - 15) << 23;
    let man = mantissa << 13;
    f32::from_bits(sign | exp | man)
}

/// 8-lane unrolled dot product.
fn dot_product(a: &[f32], b: &[f32]) -> f32 {
    debug_assert_eq!(a.len(), b.len());
    let n = a.len();
    let chunks = n / 8;
    let mut sum0 = 0.0f32;
    let mut sum1 = 0.0f32;
    let mut sum2 = 0.0f32;
    let mut sum3 = 0.0f32;
    let mut sum4 = 0.0f32;
    let mut sum5 = 0.0f32;
    let mut sum6 = 0.0f32;
    let mut sum7 = 0.0f32;

    for i in 0..chunks {
        let base = i * 8;
        sum0 += a[base] * b[base];
        sum1 += a[base + 1] * b[base + 1];
        sum2 += a[base + 2] * b[base + 2];
        sum3 += a[base + 3] * b[base + 3];
        sum4 += a[base + 4] * b[base + 4];
        sum5 += a[base + 5] * b[base + 5];
        sum6 += a[base + 6] * b[base + 6];
        sum7 += a[base + 7] * b[base + 7];
    }

    let mut tail = 0.0f32;
    for i in (chunks * 8)..n {
        tail += a[i] * b[i];
    }
    (sum0 + sum1) + (sum2 + sum3) + (sum4 + sum5) + (sum6 + sum7) + tail
}

/// A single record in the FTVI index.
#[derive(Debug, Clone)]
pub struct FtviRecord {
    pub id: u64,
    pub vector: Vec<f32>,
}

/// Writer for creating FTVI index files.
pub struct FtviWriter<W: Write> {
    writer: W,
    dimension: u16,
    count: u32,
    buf: Vec<u8>,
}

impl<W: Write> FtviWriter<W> {
    pub fn new(mut writer: W, dimension: u16) -> io::Result<Self> {
        writer.write_all(MAGIC)?;
        writer.write_all(&VERSION.to_le_bytes())?;
        writer.write_all(&dimension.to_le_bytes())?;
        // placeholder for count — will be patched on finish
        writer.write_all(&0u32.to_le_bytes())?;
        Ok(Self {
            writer,
            dimension,
            count: 0,
            buf: Vec::with_capacity(dimension as usize * 2 + 8),
        })
    }

    pub fn push(&mut self, id: u64, vector: &[f32]) -> io::Result<()> {
        if vector.len() != self.dimension as usize {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "expected dimension {}, got {}",
                    self.dimension,
                    vector.len()
                ),
            ));
        }
        self.buf.clear();
        self.buf.extend_from_slice(&id.to_le_bytes());
        for &val in vector {
            self.buf.extend_from_slice(&f32_to_f16(val).to_le_bytes());
        }
        self.writer.write_all(&self.buf)?;
        self.count += 1;
        Ok(())
    }

    pub fn count(&self) -> u32 {
        self.count
    }

    /// Finish writing. Returns the inner writer.
    /// NOTE: The count header field requires a seekable writer to patch.
    /// For non-seekable writers, use `finish_to_vec` instead.
    pub fn finish(self) -> io::Result<W> {
        Ok(self.writer)
    }
}

/// Write an FTVI index to a Vec<u8> with correct count header.
pub fn write_ftvi_vec(dimension: u16, records: &[(u64, &[f32])]) -> io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    buf.extend_from_slice(MAGIC);
    buf.extend_from_slice(&VERSION.to_le_bytes());
    buf.extend_from_slice(&dimension.to_le_bytes());
    buf.extend_from_slice(&(records.len() as u32).to_le_bytes());
    for &(id, vector) in records {
        if vector.len() != dimension as usize {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "dimension mismatch",
            ));
        }
        buf.extend_from_slice(&id.to_le_bytes());
        for &val in vector {
            buf.extend_from_slice(&f32_to_f16(val).to_le_bytes());
        }
    }
    Ok(buf)
}

/// In-memory FTVI index for search.
#[derive(Debug)]
pub struct FtviIndex {
    dimension: usize,
    ids: Vec<u64>,
    vectors: Vec<Vec<f32>>,
}

impl FtviIndex {
    /// Parse an FTVI index from bytes.
    pub fn from_bytes(data: &[u8]) -> io::Result<Self> {
        let mut cursor = io::Cursor::new(data);
        let mut magic = [0u8; 4];
        cursor.read_exact(&mut magic)?;
        if &magic != MAGIC {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "bad FTVI magic"));
        }
        let mut ver_buf = [0u8; 2];
        cursor.read_exact(&mut ver_buf)?;
        let version = u16::from_le_bytes(ver_buf);
        if version != VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsupported FTVI version: {}", version),
            ));
        }
        let mut dim_buf = [0u8; 2];
        cursor.read_exact(&mut dim_buf)?;
        let dimension = u16::from_le_bytes(dim_buf) as usize;

        let mut count_buf = [0u8; 4];
        cursor.read_exact(&mut count_buf)?;
        let count = u32::from_le_bytes(count_buf) as usize;

        let mut ids = Vec::with_capacity(count);
        let mut vectors = Vec::with_capacity(count);

        let mut id_buf = [0u8; 8];
        let mut half_buf = [0u8; 2];

        for _ in 0..count {
            cursor.read_exact(&mut id_buf)?;
            let id = u64::from_le_bytes(id_buf);
            let mut vec = Vec::with_capacity(dimension);
            for _ in 0..dimension {
                cursor.read_exact(&mut half_buf)?;
                let half = u16::from_le_bytes(half_buf);
                vec.push(f16_to_f32(half));
            }
            ids.push(id);
            vectors.push(vec);
        }

        Ok(Self {
            dimension,
            ids,
            vectors,
        })
    }

    pub fn dimension(&self) -> usize {
        self.dimension
    }

    pub fn len(&self) -> usize {
        self.ids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    /// Search for top-k nearest neighbors by dot product similarity.
    pub fn search(&self, query: &[f32], k: usize) -> Vec<(u64, f32)> {
        if query.len() != self.dimension || self.is_empty() {
            return Vec::new();
        }
        let mut scored: Vec<(u64, f32)> = self
            .ids
            .iter()
            .zip(&self.vectors)
            .map(|(&id, vec)| (id, dot_product(query, vec)))
            .collect();
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(k);
        scored
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_index(vecs: &[(u64, Vec<f32>)]) -> FtviIndex {
        let dim = if vecs.is_empty() {
            4
        } else {
            vecs[0].1.len() as u16
        };
        let records: Vec<(u64, &[f32])> = vecs.iter().map(|(id, v)| (*id, v.as_slice())).collect();
        let data = write_ftvi_vec(dim, &records).unwrap();
        FtviIndex::from_bytes(&data).unwrap()
    }

    #[test]
    fn roundtrip_basic() {
        let vecs = vec![
            (1u64, vec![1.0, 0.0, 0.0, 0.0]),
            (2, vec![0.0, 1.0, 0.0, 0.0]),
        ];
        let idx = make_index(&vecs);
        assert_eq!(idx.len(), 2);
        assert_eq!(idx.dimension(), 4);
        // f16 roundtrip preserves 1.0 and 0.0 exactly
        assert!((idx.vectors[0][0] - 1.0).abs() < f32::EPSILON);
        assert!(idx.vectors[0][1].abs() < f32::EPSILON);
    }

    #[test]
    fn search_returns_nearest() {
        let vecs = vec![
            (10u64, vec![1.0, 0.0, 0.0, 0.0]),
            (20, vec![0.0, 1.0, 0.0, 0.0]),
            (30, vec![0.7, 0.7, 0.0, 0.0]),
        ];
        let idx = make_index(&vecs);
        let results = idx.search(&[1.0, 0.0, 0.0, 0.0], 2);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].0, 10); // exact match first
    }

    #[test]
    fn search_top_k_truncation() {
        let vecs: Vec<(u64, Vec<f32>)> = (0..10).map(|i| (i, vec![i as f32, 0.0])).collect();
        let idx = make_index(&vecs);
        let results = idx.search(&[1.0, 0.0], 3);
        assert_eq!(results.len(), 3);
    }

    #[test]
    fn f16_precision() {
        // f16 has ~3 decimal digits of precision
        let val = 0.123f32;
        let roundtrip = f16_to_f32(f32_to_f16(val));
        assert!(
            (val - roundtrip).abs() < 0.001,
            "precision loss: {} vs {}",
            val,
            roundtrip
        );
    }

    #[test]
    fn f16_special_values() {
        // zero
        assert!(f16_to_f32(f32_to_f16(0.0)).abs() < f32::EPSILON);
        // negative zero
        assert_eq!(f16_to_f32(f32_to_f16(-0.0)).to_bits(), (-0.0f32).to_bits());
        // infinity
        assert!(f16_to_f32(f32_to_f16(f32::INFINITY)).is_infinite());
        // NaN
        assert!(f16_to_f32(f32_to_f16(f32::NAN)).is_nan());
    }

    #[test]
    fn f16_large_value_clamps_to_inf() {
        let big = 100000.0f32; // exceeds f16 range
        let h = f32_to_f16(big);
        assert!(f16_to_f32(h).is_infinite());
    }

    #[test]
    fn f16_tiny_value_underflows() {
        let tiny = 1e-10f32;
        let h = f32_to_f16(tiny);
        let back = f16_to_f32(h);
        assert!(back.abs() < 1e-5);
    }

    #[test]
    fn empty_index() {
        let vecs: Vec<(u64, Vec<f32>)> = vec![];
        let idx = make_index(&vecs);
        assert!(idx.is_empty());
        assert_eq!(idx.search(&[1.0, 0.0, 0.0, 0.0], 5), vec![]);
    }

    #[test]
    fn bad_magic() {
        let mut data = write_ftvi_vec(2, &[(1, &[1.0, 0.0])]).unwrap();
        data[0] = b'X'; // corrupt magic
        let err = FtviIndex::from_bytes(&data).unwrap_err();
        assert!(err.to_string().contains("magic"));
    }

    #[test]
    fn dot_product_correctness() {
        let a = vec![1.0, 2.0, 3.0];
        let b = vec![4.0, 5.0, 6.0];
        let dot = dot_product(&a, &b);
        assert!((dot - 32.0).abs() < f32::EPSILON);
    }

    #[test]
    fn dot_product_8lane() {
        // test with >8 dimensions to exercise unrolled path
        let a: Vec<f32> = (0..16).map(|i| i as f32).collect();
        let b: Vec<f32> = (0..16).map(|i| (15 - i) as f32).collect();
        let result = dot_product(&a, &b);
        let expected: f32 = a.iter().zip(&b).map(|(x, y)| x * y).sum();
        assert!((result - expected).abs() < f32::EPSILON);
    }

    #[test]
    fn dimension_mismatch_search() {
        let vecs = vec![(1u64, vec![1.0, 0.0])];
        let idx = make_index(&vecs);
        // query has wrong dimension
        let results = idx.search(&[1.0, 0.0, 0.0], 5);
        assert!(results.is_empty());
    }

    #[test]
    fn writer_count() {
        let mut buf = Vec::new();
        let mut w = FtviWriter::new(&mut buf, 2).unwrap();
        assert_eq!(w.count(), 0);
        w.push(1, &[1.0, 0.0]).unwrap();
        w.push(2, &[0.0, 1.0]).unwrap();
        assert_eq!(w.count(), 2);
    }
}
