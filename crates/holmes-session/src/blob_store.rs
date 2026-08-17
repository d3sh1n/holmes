//! Content-addressed blob storage for oversized tool results (P1-08).
//!
//! A `ToolResult.content` larger than [`BLOB_OFFLOAD_THRESHOLD`] is compressed
//! (zlib, only when it actually shrinks), split into chunks and inserted into
//! the `blobs` / `blob_chunks` tables in the SAME transaction as the event row
//! that references it. The in-event content is replaced by a
//! `__BLOB_REF__:sha256:<hex>` marker. The SQLite database is therefore the
//! sole authority for the payload — no sidecar file is required to restore
//! evidence — while the stored event stays small enough for FTS indexing and
//! transcript projection. Reads verify the SHA-256 and original size before
//! accepting the restored content; any inconsistency surfaces as an error and
//! the marker is kept (degraded read) rather than returning corrupt data.

use rusqlite::{params, Connection, Transaction};
use sha2::{Digest, Sha256};

/// Tool result content (chars) above which the payload is moved into the blob
/// tables. Same threshold the old disk-sidecar offload used.
pub const BLOB_OFFLOAD_THRESHOLD: usize = 10_000;

/// In-event marker prefix replacing an offloaded payload.
pub const BLOB_REF_PREFIX: &str = "__BLOB_REF__:sha256:";

/// Legacy pre-v6 marker: payload offloaded to a `tool-results/*.txt` sidecar.
/// Still understood by the read path (degraded to the pointer when the file is
/// gone); never written anymore.
pub const LEGACY_BYPASS_PREFIX: &str = "__BYPASS_FILE__:file://";

/// Chunk size for the stored (compressed) payload. 512 KiB keeps individual
/// rows comfortably below SQLite's default 1 GiB blob limit while amortising
/// per-row overhead for multi-megabyte outputs.
const CHUNK_SIZE: usize = 512 * 1024;

/// A payload prepared for transactional insert. Cloned into the write-retry
/// closure, so fields stay plain owned data.
#[derive(Debug, Clone)]
pub struct PreparedBlob {
    pub sha256: String,
    pub codec: &'static str,
    pub original_size: u64,
    pub chunks: Vec<Vec<u8>>,
}

/// Compress (when worthwhile) and chunk a payload, keyed by its SHA-256 over
/// the ORIGINAL bytes so reads can verify restoration integrity.
pub fn prepare(content: &str) -> PreparedBlob {
    let sha256 = format!("{:x}", Sha256::digest(content.as_bytes()));
    let compressed = {
        use std::io::Write;
        let mut encoder =
            flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        // Writing to a Vec cannot fail.
        let _ = encoder.write_all(content.as_bytes());
        encoder.finish().unwrap_or_default()
    };
    let (codec, payload): (&'static str, Vec<u8>) = if compressed.len() < content.len() {
        ("zlib", compressed)
    } else {
        ("none", content.as_bytes().to_vec())
    };
    let chunks: Vec<Vec<u8>> = payload
        .chunks(CHUNK_SIZE)
        .map(|chunk| chunk.to_vec())
        .collect();
    PreparedBlob {
        sha256,
        codec,
        original_size: content.len() as u64,
        chunks,
    }
}

/// The in-event marker standing in for an offloaded payload.
pub fn reference_for(sha256: &str) -> String {
    format!("{BLOB_REF_PREFIX}{sha256}")
}

/// Extract the blob hash from an in-event marker, if this content is one.
pub fn parse_reference(content: &str) -> Option<&str> {
    content.strip_prefix(BLOB_REF_PREFIX)
}

/// Insert a prepared blob. `INSERT OR IGNORE` keeps the statement idempotent:
/// the enclosing write closure may be re-run by the BUSY/LOCKED retry, and the
/// same content (same hash) may legitimately arrive twice.
pub fn insert(tx: &Transaction, blob: &PreparedBlob) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT OR IGNORE INTO blobs (sha256, codec, original_size, chunk_count)
         VALUES (?1, ?2, ?3, ?4)",
        params![
            blob.sha256,
            blob.codec,
            blob.original_size as i64,
            blob.chunks.len() as i64
        ],
    )?;
    for (index, chunk) in blob.chunks.iter().enumerate() {
        tx.execute(
            "INSERT OR IGNORE INTO blob_chunks (blob_sha256, chunk_index, data)
             VALUES (?1, ?2, ?3)",
            params![blob.sha256, index as i64, chunk],
        )?;
    }
    Ok(())
}

/// Restore a payload from the blob tables, verifying chunk continuity, codec,
/// original size and SHA-256. Errors are strings because callers degrade to
/// keeping the marker (with an error log) rather than failing the read.
pub fn load(conn: &Connection, sha256: &str) -> Result<String, String> {
    let (codec, original_size, chunk_count): (String, i64, i64) = conn
        .query_row(
            "SELECT codec, original_size, chunk_count FROM blobs WHERE sha256 = ?1",
            params![sha256],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .map_err(|e| format!("blob {sha256} metadata unreadable: {e}"))?;

    let mut stmt = conn
        .prepare(
            "SELECT chunk_index, data FROM blob_chunks
             WHERE blob_sha256 = ?1 ORDER BY chunk_index",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map(params![sha256], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?))
        })
        .map_err(|e| e.to_string())?;

    let mut payload = Vec::new();
    let mut expected_index = 0i64;
    for row in rows {
        let (index, data) = row.map_err(|e| e.to_string())?;
        if index != expected_index {
            return Err(format!(
                "blob {sha256} chunk gap: expected index {expected_index}, found {index}"
            ));
        }
        expected_index += 1;
        payload.extend_from_slice(&data);
    }
    if expected_index != chunk_count {
        return Err(format!(
            "blob {sha256} declares {chunk_count} chunks, {expected_index} stored"
        ));
    }

    let bytes = match codec.as_str() {
        "zlib" => {
            use std::io::Read;
            let mut decoded = Vec::new();
            flate2::read::ZlibDecoder::new(payload.as_slice())
                .read_to_end(&mut decoded)
                .map_err(|e| format!("blob {sha256} zlib decode failed: {e}"))?;
            decoded
        }
        "none" => payload,
        other => return Err(format!("blob {sha256} has unknown codec '{other}'")),
    };
    if bytes.len() as i64 != original_size {
        return Err(format!(
            "blob {sha256} size mismatch: declared {original_size}, decoded {}",
            bytes.len()
        ));
    }
    let actual = format!("{:x}", Sha256::digest(&bytes));
    if actual != sha256 {
        return Err(format!("blob {sha256} hash mismatch after decode"));
    }
    String::from_utf8(bytes).map_err(|e| format!("blob {sha256} is not valid UTF-8: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE blobs (sha256 TEXT PRIMARY KEY, codec TEXT NOT NULL,
                 original_size INTEGER NOT NULL, chunk_count INTEGER NOT NULL,
                 created_at TEXT NOT NULL DEFAULT (datetime('now')));
             CREATE TABLE blob_chunks (blob_sha256 TEXT NOT NULL, chunk_index INTEGER NOT NULL,
                 data BLOB NOT NULL, PRIMARY KEY (blob_sha256, chunk_index));",
        )
        .unwrap();
        conn
    }

    #[test]
    fn round_trip_compressible_and_incompressible() {
        let conn = mem_conn();
        // Highly compressible.
        let repeated = "abcde".repeat(10_000);
        let blob = prepare(&repeated);
        assert_eq!(blob.codec, "zlib");
        let tx = conn.unchecked_transaction().unwrap();
        insert(&tx, &blob).unwrap();
        tx.commit().unwrap();
        assert_eq!(load(&conn, &blob.sha256).unwrap(), repeated);

        // Effectively incompressible (short high-entropy-ish content is stored
        // raw when zlib would not shrink it).
        let raw = "x".to_string(); // tiny: zlib frame overhead exceeds savings
        let blob = prepare(&raw);
        assert_eq!(blob.codec, "none");
        let tx = conn.unchecked_transaction().unwrap();
        insert(&tx, &blob).unwrap();
        tx.commit().unwrap();
        assert_eq!(load(&conn, &blob.sha256).unwrap(), raw);
    }

    #[test]
    fn insert_is_idempotent_for_retry() {
        let conn = mem_conn();
        let content = "duplicate payload".repeat(1000);
        let blob = prepare(&content);
        let tx = conn.unchecked_transaction().unwrap();
        insert(&tx, &blob).unwrap();
        insert(&tx, &blob).unwrap();
        tx.commit().unwrap();
        assert_eq!(load(&conn, &blob.sha256).unwrap(), content);
    }

    #[test]
    fn load_rejects_missing_and_corrupt_blobs() {
        let conn = mem_conn();
        let content = "z".repeat(20_000);
        let blob = prepare(&content);
        let tx = conn.unchecked_transaction().unwrap();
        insert(&tx, &blob).unwrap();
        tx.commit().unwrap();

        assert!(load(&conn, "deadbeef").is_err());

        // Corrupt one chunk: hash verification must reject the restore.
        conn.execute(
            "UPDATE blob_chunks SET data = x'00' WHERE blob_sha256 = ?1 AND chunk_index = 0",
            params![blob.sha256],
        )
        .unwrap();
        assert!(load(&conn, &blob.sha256).is_err());
    }

    #[test]
    fn reference_marker_round_trips() {
        let marker = reference_for("abc123");
        assert_eq!(parse_reference(&marker), Some("abc123"));
        assert_eq!(parse_reference("plain content"), None);
    }
}
