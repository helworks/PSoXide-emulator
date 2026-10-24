//! Producer half of emucap's launch-bound front authentication (`ECFA` v1).
//!
//! A managed launch hands the producer two secrets out of band:
//! `EMUCAP_SESSION_TOKEN` (a `front-direct-<64 hex>` or `front-broker-<64 hex>` key
//! derived for this port and launch) and `EMUCAP_LAUNCH_ID`. After connecting to
//! the Control MCP's listener the producer must complete this exchange before the
//! first NDJSON byte:
//!
//! ```text
//! listener -> challenge  ECFA 1 type=1 boundary port_be16 nonce16          (25 B)
//! producer -> proof      ECFA 1 type=2 boundary launch_id33 nonce16 hmac32 (88 B)
//! listener -> proof      ECFA 1 type=3 boundary hmac32                     (39 B)
//! producer -> commit     ECFA 1 type=4 boundary hmac32                     (39 B)
//! listener -> ok         ECFA 1 type=5 boundary                            (7 B)
//! ```
//!
//! This is a byte-for-byte port of emucap `adapters/_common/emucap_front_auth.h`;
//! the Rust listener side lives in emucap `src/live/producer_auth.rs`.

use std::io::{Read, Write};

use sha2::{Digest, Sha256};

const MAGIC: &[u8; 4] = b"ECFA";
const VERSION: u8 = 1;
const CHALLENGE_SIZE: usize = 25;
const PRODUCER_PROOF_SIZE: usize = 88;
const DIRECTIONAL_PROOF_SIZE: usize = 39;
const LISTENER_OK_SIZE: usize = 7;
const LAUNCH_ID_SIZE: usize = 33;
const NONCE_SIZE: usize = 16;
const PROOF_SIZE: usize = 32;
const AUTH_DOMAIN: &str = "emucap-producer-front-auth-v1";
const NONCE_DOMAIN: &str = "emucap-producer-front-nonce-v1";

/// HMAC-SHA256 over length-prefixed fields: each field is preceded by its byte
/// length as a big-endian `u64`, so field boundaries are unambiguous.
pub(crate) fn hmac(key: &[u8], fields: &[&[u8]]) -> [u8; PROOF_SIZE] {
    const BLOCK: usize = 64;
    let mut key_block = [0u8; BLOCK];
    if key.len() > BLOCK {
        key_block[..PROOF_SIZE].copy_from_slice(&Sha256::digest(key));
    } else {
        key_block[..key.len()].copy_from_slice(key);
    }
    let mut inner_pad = [0x36u8; BLOCK];
    let mut outer_pad = [0x5cu8; BLOCK];
    for index in 0..BLOCK {
        inner_pad[index] ^= key_block[index];
        outer_pad[index] ^= key_block[index];
    }
    let mut inner = Sha256::new();
    inner.update(inner_pad);
    for field in fields {
        inner.update((field.len() as u64).to_be_bytes());
        inner.update(field);
    }
    let inner_digest = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(outer_pad);
    outer.update(inner_digest);
    outer.finalize().into()
}

/// Boundary byte for a front key: 1 = direct listener, 2 = broker producer port.
pub(crate) fn boundary(front_key: &str) -> Option<u8> {
    let hex_ok = |rest: &str| rest.len() == 64 && rest.bytes().all(|b| b.is_ascii_hexdigit());
    if let Some(rest) = front_key.strip_prefix("front-direct-") {
        return hex_ok(rest).then_some(1);
    }
    if let Some(rest) = front_key.strip_prefix("front-broker-") {
        return hex_ok(rest).then_some(2);
    }
    None
}

/// `launch-` followed by a 26-character lowercase Crockford ULID.
pub(crate) fn valid_launch_id(launch_id: &str) -> bool {
    let Some(ulid) = launch_id.strip_prefix("launch-") else {
        return false;
    };
    launch_id.len() == LAUNCH_ID_SIZE
        && ulid.bytes().all(|b| {
            matches!(b, b'0'..=b'9' | b'a'..=b'h' | b'j' | b'k' | b'm' | b'n' | b'p'..=b't' | b'v'..=b'z')
        })
}

pub(crate) fn header(record_type: u8, boundary: u8) -> [u8; 7] {
    [
        MAGIC[0],
        MAGIC[1],
        MAGIC[2],
        MAGIC[3],
        VERSION,
        record_type,
        boundary,
    ]
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
}

/// Run the producer side of the exchange on a freshly connected stream. The
/// caller bounds the whole exchange with socket timeouts; any short read, bad
/// header or failed proof is an error and the stream must be discarded.
pub fn authenticate<S: Read + Write>(
    stream: &mut S,
    front_key: &str,
    launch_id: &str,
    port: u16,
) -> Result<(), String> {
    let boundary = boundary(front_key)
        .ok_or_else(|| "EMUCAP_SESSION_TOKEN is not a launch-bound front key".to_string())?;
    if !valid_launch_id(launch_id) {
        return Err("EMUCAP_LAUNCH_ID is not a managed launch identity".into());
    }
    let key = front_key.as_bytes();

    let mut challenge = [0u8; CHALLENGE_SIZE];
    stream
        .read_exact(&mut challenge)
        .map_err(|error| format!("read listener challenge: {error}"))?;
    if challenge[..7] != header(1, boundary) || challenge[7..9] != port.to_be_bytes() {
        return Err("listener challenge has the wrong header, boundary or port".into());
    }

    let nonce_digest = hmac(key, &[NONCE_DOMAIN.as_bytes(), &challenge]);
    let nonce = &nonce_digest[..NONCE_SIZE];
    let mut transcript = Vec::with_capacity(CHALLENGE_SIZE + LAUNCH_ID_SIZE + NONCE_SIZE);
    transcript.extend_from_slice(&challenge);
    transcript.extend_from_slice(launch_id.as_bytes());
    transcript.extend_from_slice(nonce);
    let producer_proof = hmac(key, &[AUTH_DOMAIN.as_bytes(), b"producer", &transcript]);

    let mut producer = Vec::with_capacity(PRODUCER_PROOF_SIZE);
    producer.extend_from_slice(&header(2, boundary));
    producer.extend_from_slice(launch_id.as_bytes());
    producer.extend_from_slice(nonce);
    producer.extend_from_slice(&producer_proof);
    debug_assert_eq!(producer.len(), PRODUCER_PROOF_SIZE);
    stream
        .write_all(&producer)
        .and_then(|()| stream.flush())
        .map_err(|error| format!("write producer proof: {error}"))?;

    let mut listener = [0u8; DIRECTIONAL_PROOF_SIZE];
    stream
        .read_exact(&mut listener)
        .map_err(|error| format!("read listener proof: {error}"))?;
    let expected = hmac(key, &[AUTH_DOMAIN.as_bytes(), b"listener", &transcript]);
    if listener[..7] != header(3, boundary) || !constant_time_eq(&listener[7..], &expected) {
        return Err("listener proof did not verify; refusing to send the commit".into());
    }

    let commit_proof = hmac(key, &[AUTH_DOMAIN.as_bytes(), b"commit", &transcript]);
    let mut commit = Vec::with_capacity(DIRECTIONAL_PROOF_SIZE);
    commit.extend_from_slice(&header(4, boundary));
    commit.extend_from_slice(&commit_proof);
    stream
        .write_all(&commit)
        .and_then(|()| stream.flush())
        .map_err(|error| format!("write producer commit: {error}"))?;

    let mut ok = [0u8; LISTENER_OK_SIZE];
    stream
        .read_exact(&mut ok)
        .map_err(|error| format!("read listener acceptance: {error}"))?;
    if ok != header(5, boundary) {
        return Err("listener did not accept the producer".into());
    }
    Ok(())
}

/// Listener side, for tests only: verifies a producer exactly as emucap's
/// `producer_auth.rs` does and returns the launch id it presented.
#[cfg(test)]
pub(crate) fn accept_for_test<S: Read + Write>(
    stream: &mut S,
    front_key: &str,
    port: u16,
    nonce: [u8; NONCE_SIZE],
    corrupt_listener_proof: bool,
) -> Result<String, String> {
    let boundary = boundary(front_key).ok_or("bad key")?;
    let key = front_key.as_bytes();
    let mut challenge = Vec::with_capacity(CHALLENGE_SIZE);
    challenge.extend_from_slice(&header(1, boundary));
    challenge.extend_from_slice(&port.to_be_bytes());
    challenge.extend_from_slice(&nonce);
    stream.write_all(&challenge).map_err(|e| e.to_string())?;

    let mut producer = [0u8; PRODUCER_PROOF_SIZE];
    stream
        .read_exact(&mut producer)
        .map_err(|e| e.to_string())?;
    if producer[..7] != header(2, boundary) {
        return Err("bad producer header".into());
    }
    let mut transcript = challenge.clone();
    transcript.extend_from_slice(&producer[7..7 + LAUNCH_ID_SIZE + NONCE_SIZE]);
    let expected = hmac(key, &[AUTH_DOMAIN.as_bytes(), b"producer", &transcript]);
    if producer[7 + LAUNCH_ID_SIZE + NONCE_SIZE..] != expected {
        return Err("bad producer proof".into());
    }
    let mut listener = header(3, boundary).to_vec();
    let mut proof = hmac(key, &[AUTH_DOMAIN.as_bytes(), b"listener", &transcript]);
    if corrupt_listener_proof {
        proof[0] ^= 1;
    }
    listener.extend_from_slice(&proof);
    stream.write_all(&listener).map_err(|e| e.to_string())?;

    let mut commit = [0u8; DIRECTIONAL_PROOF_SIZE];
    stream.read_exact(&mut commit).map_err(|e| e.to_string())?;
    let expected = hmac(key, &[AUTH_DOMAIN.as_bytes(), b"commit", &transcript]);
    if commit[..7] != header(4, boundary) || commit[7..] != expected {
        return Err("bad commit".into());
    }
    stream
        .write_all(&header(5, boundary))
        .map_err(|e| e.to_string())?;
    Ok(String::from_utf8_lossy(&producer[7..7 + LAUNCH_ID_SIZE]).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str =
        "front-direct-0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const LAUNCH: &str = "launch-01arz3ndektsv4rrffq69g5fav";

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn hmac_matches_the_shared_native_vector() {
        // Same vector as emucap adapters/_common/emucap_front_auth_test.cpp.
        assert_eq!(
            hex(&hmac(b"key", &[b"a", b"bc"])),
            "ba41d3aaace0b4bb31fb49a559ae1df182e1fd8add9b6b3b5603e6cb342dce22"
        );
    }

    #[test]
    fn key_and_launch_validation() {
        assert_eq!(boundary(KEY), Some(1));
        assert_eq!(boundary(&KEY.replace("direct", "broker")), Some(2));
        assert_eq!(boundary("front-direct-xyz"), None);
        assert!(valid_launch_id(LAUNCH));
        assert!(!valid_launch_id("launch-01ARZ3NDEKTSV4RRFFQ69G5FAV"));
        assert!(!valid_launch_id("launch-short"));
    }

    fn handshake(corrupt: bool) -> (Result<(), String>, Result<String, String>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut nonce = [0u8; NONCE_SIZE];
            for (index, byte) in nonce.iter_mut().enumerate() {
                *byte = index as u8 + 9;
            }
            accept_for_test(&mut stream, KEY, port, nonce, corrupt)
        });
        let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        let producer = authenticate(&mut stream, KEY, LAUNCH, port);
        drop(stream);
        (producer, server.join().unwrap())
    }

    #[test]
    fn mutual_authentication_round_trip() {
        let (producer, listener) = handshake(false);
        producer.unwrap();
        assert_eq!(listener.unwrap(), LAUNCH);
    }

    #[test]
    fn counterfeit_listener_never_receives_a_commit() {
        let (producer, listener) = handshake(true);
        assert!(producer.unwrap_err().contains("listener proof"));
        assert!(listener.is_err(), "listener must not see a valid commit");
    }
}
