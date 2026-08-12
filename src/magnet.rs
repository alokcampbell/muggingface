use anyhow::{anyhow, Result};
use bendy::{decoding::FromBencode, encoding::ToBencode, value::Value};
use sha1::{Digest, Sha1};

const TRACKERS: &[&str] = &[
    "udp://tracker.opentrackr.org:1337/announce",
    "udp://tracker.openbittorrent.com:6969/announce",
    "udp://tracker.torrent.eu.org:451/announce",
];

pub fn display_name(full_repo: &str, sha: &str) -> String {
    format!("{}-{}", full_repo.replace('/', "-"), sha)
}

pub fn magnet_from_torrent_bytes(torrent_bytes: &[u8], display_name: &str) -> Result<String> {
    let info_hash = info_hash_from_torrent_bytes(torrent_bytes)?;
    Ok(magnet_from_info_hash(&info_hash, display_name))
}

pub fn info_hash_from_torrent_bytes(torrent_bytes: &[u8]) -> Result<[u8; 20]> {
    let bencode_value = Value::from_bencode(torrent_bytes)
        .map_err(|e| anyhow!("Failed to parse torrent bencode: {e}"))?;
    let info_dict = match bencode_value {
        Value::Dict(d) => d
            .get(&b"info"[..])
            .cloned()
            .ok_or_else(|| anyhow!("No 'info' dict found in torrent"))?,
        _ => return Err(anyhow!("Invalid torrent format: not a dictionary at root")),
    };
    let info_bytes = info_dict
        .to_bencode()
        .map_err(|e| anyhow!("Failed to re-encode info dict: {e}"))?;
    let digest = Sha1::digest(&info_bytes);
    let mut hash = [0u8; 20];
    hash.copy_from_slice(&digest);
    Ok(hash)
}

pub fn magnet_from_info_hash(info_hash: &[u8; 20], display_name: &str) -> String {
    let mut url = format!(
        "magnet:?xt=urn:btih:{}&dn={}",
        hex::encode(info_hash),
        urlencoding::encode(display_name)
    );
    for tracker in TRACKERS {
        url.push_str("&tr=");
        url.push_str(&urlencoding::encode(tracker));
    }
    url
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tiny_torrent() -> Vec<u8> {
        // Minimal valid torrent: announce + info{name, piece length, pieces, length}
        b"d8:announce14:http://tracker4:infod6:lengthi1e4:name4:demo12:piece lengthi16384e6:pieces20:\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0ee".to_vec()
    }

    #[test]
    fn display_name_replaces_slash() {
        assert_eq!(
            display_name("openai-community/gpt2", "abc"),
            "openai-community-gpt2-abc"
        );
    }

    #[test]
    fn magnet_contains_hash_name_and_trackers() {
        let bytes = tiny_torrent();
        let magnet = magnet_from_torrent_bytes(&bytes, "openai-community-gpt2-sha").unwrap();
        assert!(magnet.starts_with("magnet:?xt=urn:btih:"));
        assert!(magnet.contains("&dn=openai-community-gpt2-sha"));
        assert!(magnet.contains("tracker.opentrackr.org"));
        assert_eq!(magnet.matches("&tr=").count(), 3);
        let hash = info_hash_from_torrent_bytes(&bytes).unwrap();
        assert_eq!(hex::encode(hash).len(), 40);
    }

    #[test]
    fn rejects_garbage() {
        assert!(magnet_from_torrent_bytes(b"not-bencode", "x").is_err());
    }
}
