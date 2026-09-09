//! Pure-Rust decompression for the encodings dnf repo metadata uses.
//!
//! Metadata files are named `<hash>-primary.xml.gz` / `.xml.zst` / `.xml.xz`.
//! We pick the decoder from the location suffix; unknown/plain files pass
//! through unchanged.

use std::io::Read;

#[derive(Debug, thiserror::Error)]
pub enum DecompressError {
    #[error("gzip decode failed: {0}")]
    Gzip(std::io::Error),
    #[error("xz decode failed: {0}")]
    Xz(String),
    #[error("zstd decode failed: {0}")]
    Zstd(String),
}

/// Decompress `data` based on the file extension of `location` (its href).
pub fn decompress(location: &str, data: &[u8]) -> Result<Vec<u8>, DecompressError> {
    let lower = location.to_ascii_lowercase();
    if lower.ends_with(".gz") {
        gunzip(data)
    } else if lower.ends_with(".zst") || lower.ends_with(".zstd") {
        unzstd(data)
    } else if lower.ends_with(".xz") {
        unxz(data)
    } else {
        // .xml (uncompressed) or anything else: return as-is.
        Ok(data.to_vec())
    }
}

fn gunzip(data: &[u8]) -> Result<Vec<u8>, DecompressError> {
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(data)
        .read_to_end(&mut out)
        .map_err(DecompressError::Gzip)?;
    Ok(out)
}

fn unxz(data: &[u8]) -> Result<Vec<u8>, DecompressError> {
    let mut out = Vec::new();
    let mut reader = std::io::BufReader::new(data);
    lzma_rs::xz_decompress(&mut reader, &mut out).map_err(|e| DecompressError::Xz(e.to_string()))?;
    Ok(out)
}

fn unzstd(data: &[u8]) -> Result<Vec<u8>, DecompressError> {
    let mut out = Vec::new();
    let mut decoder =
        ruzstd::StreamingDecoder::new(data).map_err(|e| DecompressError::Zstd(e.to_string()))?;
    decoder
        .read_to_end(&mut out)
        .map_err(|e| DecompressError::Zstd(e.to_string()))?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn gzip_roundtrip() {
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(b"hello rum").unwrap();
        let gz = enc.finish().unwrap();
        assert_eq!(decompress("x-primary.xml.gz", &gz).unwrap(), b"hello rum");
    }

    #[test]
    fn plain_passthrough() {
        assert_eq!(decompress("repomd.xml", b"<xml/>").unwrap(), b"<xml/>");
    }
}
