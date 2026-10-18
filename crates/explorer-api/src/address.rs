//! Karlsen address encoding/decoding (Kaspa cashaddr-style, 40-bit polymod).
//!
//! Addresses are *derived from script_public_key at read time* instead of
//! trusting stored text columns: the legacy indexer stored address strings,
//! which produced wrong-prefix / bad-checksum entries.

const CHARSET: &[u8; 32] = b"qpzry9x8gf2tvdw0s3jn54khce6mua7l";
pub const MAINNET_PREFIX: &str = "karlsen";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Version {
    PubKey = 0,
    PubKeyEcdsa = 1,
    ScriptHash = 8,
}

fn polymod(values: &[u8]) -> u64 {
    let mut c: u64 = 1;
    for &d in values {
        let c0 = c >> 35;
        c = ((c & 0x07_ffff_ffff) << 5) ^ d as u64;
        if c0 & 0x01 != 0 {
            c ^= 0x98_f2bc_8e61;
        }
        if c0 & 0x02 != 0 {
            c ^= 0x79_b76d_99e2;
        }
        if c0 & 0x04 != 0 {
            c ^= 0xf3_3e5f_b3c4;
        }
        if c0 & 0x08 != 0 {
            c ^= 0xae_2eab_e2a8;
        }
        if c0 & 0x10 != 0 {
            c ^= 0x1e_4f43_e470;
        }
    }
    c ^ 1
}

fn conv8to5(data: &[u8]) -> Vec<u8> {
    let (mut acc, mut bits, mut out) = (0u32, 0u32, Vec::new());
    for &b in data {
        acc = (acc << 8) | b as u32;
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(((acc >> bits) & 31) as u8);
        }
    }
    if bits > 0 {
        out.push(((acc << (5 - bits)) & 31) as u8);
    }
    out
}

fn conv5to8(data: &[u8]) -> Option<Vec<u8>> {
    let (mut acc, mut bits, mut out) = (0u32, 0u32, Vec::new());
    for &v in data {
        acc = (acc << 5) | v as u32;
        bits += 5;
        while bits >= 8 {
            bits -= 8;
            out.push(((acc >> bits) & 0xff) as u8);
        }
    }
    // Remaining padding bits must be zero.
    if bits >= 5 || (acc << (8 - bits)) & 0xff != 0 && bits > 0 {
        return None;
    }
    Some(out)
}

fn checksum_input(prefix: &str, payload5: &[u8]) -> Vec<u8> {
    let mut v: Vec<u8> = prefix.bytes().map(|b| b & 0x1f).collect();
    v.push(0);
    v.extend_from_slice(payload5);
    v.extend_from_slice(&[0u8; 8]);
    v
}

pub fn encode(prefix: &str, version: Version, payload: &[u8]) -> String {
    let mut raw = vec![version as u8];
    raw.extend_from_slice(payload);
    let payload5 = conv8to5(&raw);
    let sum = polymod(&checksum_input(prefix, &payload5));
    let mut out = String::with_capacity(prefix.len() + 1 + payload5.len() + 8);
    out.push_str(prefix);
    out.push(':');
    for v in &payload5 {
        out.push(CHARSET[*v as usize] as char);
    }
    for i in (0..8).rev() {
        out.push(CHARSET[((sum >> (5 * i)) & 31) as usize] as char);
    }
    out
}

/// Decode and fully validate (charset, checksum). Returns (prefix, version, payload).
pub fn decode(addr: &str) -> Option<(String, Version, Vec<u8>)> {
    let (prefix, rest) = addr.split_once(':')?;
    if rest.len() < 9 {
        return None;
    }
    let mut vals = Vec::with_capacity(rest.len());
    for ch in rest.bytes() {
        vals.push(CHARSET.iter().position(|&c| c == ch)? as u8);
    }
    let (data5, sum5) = vals.split_at(vals.len() - 8);
    let expect = polymod(&checksum_input(prefix, data5));
    let got = sum5.iter().fold(0u64, |a, &v| (a << 5) | v as u64);
    if expect != got {
        return None;
    }
    let raw = conv5to8(data5)?;
    let (&ver, payload) = raw.split_first()?;
    let version = match ver {
        0 => Version::PubKey,
        1 => Version::PubKeyEcdsa,
        8 => Version::ScriptHash,
        _ => return None,
    };
    Some((prefix.to_string(), version, payload.to_vec()))
}

/// Classify a raw script_public_key and derive its address, if standard.
pub fn script_to_address(prefix: &str, script: &[u8]) -> Option<String> {
    match script_type(script) {
        "pubkey" => Some(encode(prefix, Version::PubKey, &script[1..33])),
        "pubkeyecdsa" => Some(encode(prefix, Version::PubKeyEcdsa, &script[1..34])),
        "scripthash" => Some(encode(prefix, Version::ScriptHash, &script[2..34])),
        _ => None,
    }
}

pub fn script_type(script: &[u8]) -> &'static str {
    match script {
        [0x20, mid @ .., 0xac] if mid.len() == 32 => "pubkey",
        [0x21, mid @ .., 0xab] if mid.len() == 33 => "pubkeyecdsa",
        [0xaa, 0x20, mid @ .., 0x87] if mid.len() == 32 => "scripthash",
        _ => "nonstandard",
    }
}

/// Inverse: address -> script_public_key bytes (used for script-based lookups).
pub fn address_to_script(addr: &str) -> Option<Vec<u8>> {
    let (_, version, payload) = decode(addr)?;
    match (version, payload.len()) {
        (Version::PubKey, 32) => Some([&[0x20][..], &payload, &[0xac]].concat()),
        (Version::PubKeyEcdsa, 33) => Some([&[0x21][..], &payload, &[0xab]].concat()),
        (Version::ScriptHash, 32) => Some([&[0xaa, 0x20][..], &payload, &[0x87]].concat()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Real mainnet addresses taken from the production explorer frontend
    // (public addresses; no secrets). The first is a script-hash address.
    const REAL: [&str; 6] = [
        "karlsen:pq6pu5rcsaehtv2dnf8qyx4lycmpyp2f2spy4xay8z2rrl2h3r2zww8yqlt0s",
        "karlsen:qpjnywfxsdrj3nkuscf4mc0m457wt35sgfgfv379zuuekl6z0ulwkkgayg83r",
        "karlsen:qqe3p64wpjf5y27kxppxrgks298ge6lhu6ws7ndx4tswzj7c84qkjlrspcuxw",
        "karlsen:qrdn0eyzyc2z4leqwgxcngt98su5gq4p47gz435q8mu2wh8c78502qlfquvnj",
        "karlsen:qz202hrxwx6052dq4rv6vmd0lsjtlkpfx2l6zml8lneu3k4f8t9h77lvqgn42",
        "karlsen:qzzl0jxsjstn6qaqq3avc6pve2lum5cjnym2jhng06fq85v5ycp9wz7x3wq75",
    ];
    // Public Kaspa example address (cross-check of the polymod itself).
    const KASPA: &str = "kaspa:qqkqkzjvr7zwxxmjxjkmxxdwju9kjs6e9u82uh59z07vgaks6gg62v8707g73";

    #[test]
    fn real_addresses_roundtrip() {
        for a in REAL {
            let (prefix, version, payload) = decode(a).unwrap_or_else(|| panic!("decode {a}"));
            assert_eq!(prefix, "karlsen");
            assert_eq!(encode(&prefix, version, &payload), a);
        }
    }

    #[test]
    fn script_hash_address_is_version_8() {
        let (_, v, p) = decode(REAL[0]).unwrap();
        assert_eq!(v, Version::ScriptHash);
        assert_eq!(p.len(), 32);
    }

    #[test]
    fn kaspa_example_cross_check() {
        let (prefix, version, payload) = decode(KASPA).expect("kaspa example must decode");
        assert_eq!(encode(&prefix, version, &payload), KASPA);
    }

    #[test]
    fn corrupted_checksum_is_rejected() {
        let mut bad = REAL[1].to_string();
        bad.pop();
        bad.push(if REAL[1].ends_with('q') { 'p' } else { 'q' });
        assert!(decode(&bad).is_none());
    }

    #[test]
    fn script_roundtrip_for_all_kinds() {
        for a in REAL {
            let script = address_to_script(a).unwrap();
            assert_ne!(script_type(&script), "nonstandard");
            assert_eq!(script_to_address("karlsen", &script).as_deref(), Some(a));
        }
    }

    #[test]
    fn other_prefix_never_validates_as_karlsen() {
        let (_, v, p) = decode(REAL[1]).unwrap();
        let other = encode("kaspa", v, &p);
        let swapped = format!("karlsen:{}", &other["kaspa:".len()..]);
        assert!(decode(&swapped).is_none());
    }
}
