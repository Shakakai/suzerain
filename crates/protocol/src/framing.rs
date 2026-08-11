//! JSONL framing shared by the pi RPC dataplane and the suzerain wire
//! protocols. Per pi's rpc.md: LF (`\n`) is the only record delimiter; strip
//! a trailing `\r`; never split on Unicode line separators.

use serde::de::DeserializeOwned;
use serde::Serialize;
use thiserror::Error;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

#[derive(Debug, Error)]
pub enum FramingError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("stream closed")]
    Eof,
}

/// Read one JSONL record. Returns `Err(FramingError::Eof)` on clean EOF.
pub async fn read_jsonl<R, T>(reader: &mut R) -> Result<T, FramingError>
where
    R: AsyncBufRead + Unpin,
    T: DeserializeOwned,
{
    let mut line = String::new();
    let n = reader.read_line(&mut line).await?;
    if n == 0 {
        return Err(FramingError::Eof);
    }
    if line.ends_with('\n') {
        line.pop();
        if line.ends_with('\r') {
            line.pop();
        }
    }
    Ok(serde_json::from_str(&line)?)
}

/// Write one JSONL record (serialized JSON + `\n`).
pub async fn write_jsonl<W, T>(writer: &mut W, value: &T) -> Result<(), FramingError>
where
    W: AsyncWrite + Unpin,
    T: Serialize + ?Sized,
{
    let mut buf = serde_json::to_vec(value)?;
    buf.push(b'\n');
    writer.write_all(&buf).await?;
    writer.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{duplex, BufReader};

    #[tokio::test]
    async fn jsonl_roundtrip() {
        let (tx, rx) = duplex(1024);
        let mut tx = tx;
        let mut rx = BufReader::new(rx);

        write_jsonl(&mut tx, &serde_json::json!({"type": "ping", "n": 1}))
            .await
            .unwrap();
        write_jsonl(&mut tx, &serde_json::json!({"type": "pong", "n": 2}))
            .await
            .unwrap();

        let a: serde_json::Value = read_jsonl(&mut rx).await.unwrap();
        let b: serde_json::Value = read_jsonl(&mut rx).await.unwrap();
        assert_eq!(a["type"], "ping");
        assert_eq!(b["n"], 2);
    }

    #[tokio::test]
    async fn eof_is_reported() {
        let (tx, rx) = duplex(64);
        drop(tx);
        let mut rx = BufReader::new(rx);
        let r: Result<serde_json::Value, _> = read_jsonl(&mut rx).await;
        assert!(matches!(r, Err(FramingError::Eof)));
    }

    #[test]
    fn fleet_topic_is_32_bytes() {
        assert_eq!(crate::alpn::FLEET_TOPIC.len(), 32);
    }
}

/// Hex-encoded SHA-256 of `data` — used for bundle integrity (G8).
pub fn sha256_hex(data: &[u8]) -> String {
    use sha2::Digest;
    let digest = sha2::Sha256::digest(data);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod sha_tests {
    #[test]
    fn sha256_hex_matches_known_vector() {
        // sha256("abc") — well-known test vector.
        assert_eq!(
            super::sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
