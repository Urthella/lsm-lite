//! Record framing, shared by the log and the manifest, and the log's record format.

use std::io;

use crate::{Reader, corrupt};

/// Frames a payload as `[len u32][crc u32][payload]`. The CRC covers the length too, so a run of
/// zeros isn't read as an empty record.
pub fn frame(payload: &[u8]) -> Vec<u8> {
    let len = (payload.len() as u32).to_le_bytes();
    let mut out = Vec::with_capacity(8 + payload.len());
    out.extend_from_slice(&len);
    out.extend_from_slice(&checksum(&len, payload).to_le_bytes());
    out.extend_from_slice(payload);
    out
}

/// Returns the records before the first one that's cut short or fails its CRC, and how many bytes
/// they take up. A crash can tear the end of a log, but not what was synced before it.
pub fn records(buf: &[u8]) -> (Vec<&[u8]>, usize) {
    let mut out = Vec::new();
    let mut pos = 0;
    while let Some(header) = buf[pos..].get(..8) {
        let len = u32::from_le_bytes(header[..4].try_into().unwrap());
        let crc = u32::from_le_bytes(header[4..].try_into().unwrap());
        let Some(payload) = buf[pos + 8..].get(..len as usize) else {
            break;
        };
        if checksum(&header[..4], payload) != crc {
            break;
        }
        out.push(payload);
        pos += 8 + payload.len();
    }
    (out, pos)
}

fn checksum(len: &[u8], payload: &[u8]) -> u32 {
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(len);
    hasher.update(payload);
    hasher.finalize()
}

const DELETE: u8 = 0;
const PUT: u8 = 1;

/// A log record: `[kind u8][key_len u32][key][value]`.
pub fn encode(key: &[u8], value: Option<&[u8]>) -> Vec<u8> {
    let mut out = vec![if value.is_some() { PUT } else { DELETE }];
    out.extend_from_slice(&(key.len() as u32).to_le_bytes());
    out.extend_from_slice(key);
    out.extend_from_slice(value.unwrap_or_default());
    out
}

pub fn decode(record: &[u8]) -> io::Result<(Vec<u8>, Option<Vec<u8>>)> {
    let mut r = Reader::new(record);
    let kind = r.u8()?;
    let len = r.u32()? as usize;
    let key = r.bytes(len)?.to_vec();
    match (kind, r.rest()) {
        (PUT, value) => Ok((key, Some(value.to_vec()))),
        (DELETE, []) => Ok((key, None)),
        _ => Err(corrupt("a log record")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_torn_or_zeroed_end_stops_the_log() {
        let mut log = [frame(b"one"), frame(b""), frame(b"three")].concat();
        let whole = log.len();
        log.extend_from_slice(&frame(b"four")[..9]);
        assert_eq!(records(&log), (vec![&b"one"[..], b"", b"three"], whole));

        log.truncate(whole);
        log.extend_from_slice(&[0; 32]);
        assert_eq!(records(&log), (vec![&b"one"[..], b"", b"three"], whole));
    }

    #[test]
    fn a_flipped_bit_stops_the_log() {
        let mut log = [frame(b"one"), frame(b"two")].concat();
        log[9] ^= 1;
        assert_eq!(records(&log), (vec![], 0));
    }

    #[test]
    fn records_round_trip() {
        assert_eq!(
            decode(&encode(b"k", Some(b"v"))).unwrap(),
            (b"k".to_vec(), Some(b"v".to_vec()))
        );
        assert_eq!(
            decode(&encode(b"k", Some(b""))).unwrap(),
            (b"k".to_vec(), Some(vec![]))
        );
        assert_eq!(decode(&encode(b"k", None)).unwrap(), (b"k".to_vec(), None));
        assert!(decode(&[DELETE, 1, 0, 0, 0, b'k', b'x']).is_err());
    }
}
