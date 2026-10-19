//! Miner address and miner tag from a coinbase payload.
//!
//! Layout (same as Kaspa): blue score (8 bytes), subsidy (8), script version (2),
//! script length (1), script, then free-form UTF-8 text set by the miner or pool.

use crate::address::{script_to_address, MAINNET_PREFIX};

/// Returns `(miner_address, miner_info)`, or `None` when the payload does not follow
/// the layout or the script has no standard address.
pub fn parse_coinbase_payload(payload: &[u8]) -> Option<(String, String)> {
    let script_len = *payload.get(18)? as usize;
    let script = payload.get(19..19 + script_len)?;
    let info = std::str::from_utf8(&payload[19 + script_len..]).ok()?;
    let address = script_to_address(MAINNET_PREFIX, script)?;
    Some((address, info.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn real_coinbase_payload() {
        // Coinbase of block ba735a63... (golden-block.json).
        let payload = hex::decode(
            "0d668b0500000000cb814d74000000000000222073c860ebc776ce87d04f348b07aaacafbbb2\
             6c2c29acbaa35107721aaa7054adac332e312e302f576f6f6c79506f6f6c79205553312d6b6c732d31",
        )
        .unwrap();
        let (address, info) = parse_coinbase_payload(&payload).unwrap();
        // The same address the coinbase pays to.
        assert_eq!(
            address,
            "karlsen:qpeusc8tcamvap7sfu6gkpa24jhmhvnv9s56ew4r2yrhyx42wp22609y5q4p0"
        );
        assert_eq!(info, "3.1.0/WoolyPooly US1-kls-1");
    }

    #[test]
    fn short_or_broken_payload_is_none() {
        assert!(parse_coinbase_payload(&[]).is_none());
        assert!(parse_coinbase_payload(&[0u8; 19]).is_none());
        let mut p = vec![0u8; 18];
        p.push(40); // script length past the end
        p.extend_from_slice(&[0u8; 10]);
        assert!(parse_coinbase_payload(&p).is_none());
    }
}
