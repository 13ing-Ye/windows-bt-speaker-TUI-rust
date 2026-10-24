//! Human-readable Bluetooth address extraction.
//!
//! Windows exposes an A2DP sink peer as a PnP device interface path rather
//! than as a plain address, for example:
//!
//! ```text
//! \\?\BTHENUM#{0000110a-...}_VID&0001004c_PID&7012#7&d8f392&0&AABBCCDDEE01_C00000000#{...}\SNK
//! ```
//!
//! The path is useful for `TryCreateFromId`, but showing it to a person is
//! useless. The 12 hex digits after `&0&` are the peer address; that layout is
//! an observation about current Windows builds, not a documented contract, so
//! parsing stays tolerant: it accepts any hex casing and also recognises the
//! `Dev_<addr>` and colon-separated shapes used by other BTHENUM paths.
//!
//! When nothing matches the caller must *not* fall back to the raw path; it
//! logs the full id for troubleshooting and shows a placeholder instead.

const ADDRESS_HEX_LEN: usize = 12;

/// Extracts the peer Bluetooth address as `AA:BB:CC:DD:EE:FF`.
///
/// Returns `None` when the id does not contain a recognisable address.
pub fn mac_address(device_id: &str) -> Option<String> {
    let formatted = format_address(
        zero_marker_address(device_id)
            .or_else(|| dev_prefix_address(device_id))
            .or_else(|| colon_address(device_id))?,
    );
    (formatted != "00:00:00:00:00:00").then_some(formatted)
}

/// Reads 12 hex digits at `start`, refusing to cut a longer hex token.
///
/// The observed ids continue with `_C00000000`, but requiring that suffix
/// would be brittle; a non-hex boundary is enough.
fn hex12_at(id: &str, start: usize) -> Option<&str> {
    let bytes = id.as_bytes();
    let end = start.checked_add(ADDRESS_HEX_LEN)?;
    if end > bytes.len() {
        return None;
    }
    let candidate = &bytes[start..end];
    if !candidate.iter().all(u8::is_ascii_hexdigit) {
        return None;
    }
    // `&0&000000000000_` is instance padding, not a peer address. Rejecting it
    // lets the other extractors try instead of showing 00:00:00:00:00:00.
    if candidate.iter().all(|byte| *byte == b'0') {
        return None;
    }
    if bytes.get(end).is_some_and(u8::is_ascii_hexdigit) {
        return None;
    }
    // All bytes are ASCII hex, so the slice is valid UTF-8.
    std::str::from_utf8(candidate).ok()
}

/// `...&0&AABBCCDDEE01_C00000000...` — the shape seen on this machine.
fn zero_marker_address(id: &str) -> Option<&str> {
    let bytes = id.as_bytes();
    let mut found = None;
    let mut index = 0;
    while index + 3 <= bytes.len() {
        if &bytes[index..index + 3] == b"&0&" {
            if let Some(candidate) = hex12_at(id, index + 3) {
                // Keep scanning: a VID/PID segment could contain the marker.
                found = Some(candidate);
            }
        }
        index += 1;
    }
    found
}

/// `\\?\BTHENUM\Dev_AABBCCDDEE01\...` — device (not interface) paths.
fn dev_prefix_address(id: &str) -> Option<&str> {
    const NEEDLE: &[u8] = b"dev_";
    let bytes = id.as_bytes();
    let mut index = 0;
    while index + NEEDLE.len() <= bytes.len() {
        if bytes[index..index + NEEDLE.len()].eq_ignore_ascii_case(NEEDLE) {
            if let Some(candidate) = hex12_at(id, index + NEEDLE.len()) {
                return Some(candidate);
            }
        }
        index += 1;
    }
    None
}

/// `Bluetooth#BluetoothAA:BB:CC:DD:EE:01-...` — association endpoint ids.
fn colon_address(id: &str) -> Option<&str> {
    let bytes = id.as_bytes();
    let mut index = 0;
    while index + 17 <= bytes.len() {
        let candidate = &bytes[index..index + 17];
        let is_mac = candidate.iter().enumerate().all(|(offset, byte)| {
            if offset % 3 == 2 {
                *byte == b':'
            } else {
                byte.is_ascii_hexdigit()
            }
        });
        if is_mac {
            let before_ok = index == 0 || {
                let previous = bytes[index - 1];
                !previous.is_ascii_hexdigit() && previous != b':'
            };
            let after_ok = bytes
                .get(index + 17)
                .map_or(true, |byte| !byte.is_ascii_hexdigit() && *byte != b':');
            if before_ok && after_ok {
                return std::str::from_utf8(candidate).ok();
            }
        }
        index += 1;
    }
    None
}

/// Normalises `aabbccddee01` or `AA:BB:CC:DD:EE:01` to uppercase with colons.
fn format_address(address: &str) -> String {
    let digits: String = address
        .chars()
        .filter(char::is_ascii_hexdigit)
        .collect::<String>()
        .to_ascii_uppercase();
    digits
        .as_bytes()
        .chunks(2)
        .map(|pair| String::from_utf8_lossy(pair).into_owned())
        .collect::<Vec<_>>()
        .join(":")
}

#[cfg(test)]
mod tests {
    use super::*;

    const IPAD_ID: &str = concat!(
        r"\\?\BTHENUM#{0000110a-0000-1000-8000-00805f9b34fb}",
        r"_VID&0001004c_PID&7012#7&d8f392&0&AABBCCDDEE01_C00000000",
        r"#{6994ad04-93ef-11d0-a3cc-00a0c9223196}\SNK"
    );

    const CIVI_ID: &str = concat!(
        r"\\?\BTHENUM#{0000110a-0000-1000-8000-00805f9b34fb}",
        r"_VID&0001038f_PID&1200#7&d8f392&0&AABBCCDDEE02_C00000000",
        r"#{6994ad04-93ef-11d0-a3cc-00a0c9223196}\SNK"
    );

    #[test]
    fn parses_the_observed_interface_path() {
        assert_eq!(mac_address(IPAD_ID).as_deref(), Some("AA:BB:CC:DD:EE:01"));
        assert_eq!(mac_address(CIVI_ID).as_deref(), Some("AA:BB:CC:DD:EE:02"));
    }

    #[test]
    fn uppercase_hex_is_formatted() {
        assert_eq!(mac_address(IPAD_ID).as_deref(), Some("AA:BB:CC:DD:EE:01"));
    }

    #[test]
    fn lowercase_hex_is_uppercased() {
        let id = IPAD_ID.replace("AABBCCDDEE01", "aabbccddee01");
        assert_eq!(mac_address(&id).as_deref(), Some("AA:BB:CC:DD:EE:01"));
    }

    #[test]
    fn mixed_case_hex_is_uppercased() {
        let id = IPAD_ID.replace("AABBCCDDEE01", "AaBbCcDdEe01");
        assert_eq!(mac_address(&id).as_deref(), Some("AA:BB:CC:DD:EE:01"));
    }

    #[test]
    fn unrecognised_format_yields_none() {
        let id = r"\\?\BTHENUM#{0000110a-0000-1000-8000-00805f9b34fb}_VID&0001004c_PID&7012#7&d8f392&1&NOADDRESSHERE_C00000000#{6994ad04}\SNK";
        assert_eq!(mac_address(id), None);
        assert_eq!(mac_address("demo-soundcore"), None);
        assert_eq!(mac_address("&0&AABBCCDDEE0"), None, "too short");
        assert_eq!(mac_address("&0&AABBCCDDEE0ZZZ"), None, "not hex");
        assert_eq!(mac_address("&0&000000000000_C00000000"), None, "padding");
        assert_eq!(mac_address("Bluetooth#Bluetooth00:00:00:00:00:00"), None);
    }

    #[test]
    fn empty_id_yields_none() {
        assert_eq!(mac_address(""), None);
    }

    #[test]
    fn parses_dev_prefixed_and_colon_paths() {
        let dev = r"\\?\BTHENUM\Dev_AABBCCDDEE02\7&2f0a1b3&0&000000000000_C00000000";
        assert_eq!(mac_address(dev).as_deref(), Some("AA:BB:CC:DD:EE:02"));

        let endpoint = "Bluetooth#Bluetoothaa:bb:cc:dd:ee:01-7&d8f392&0&0000";
        assert_eq!(mac_address(endpoint).as_deref(), Some("AA:BB:CC:DD:EE:01"));
    }
}
