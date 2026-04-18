//! macOS Keychain lookup for Brave's cookie-storage password.

use crate::{Error, Result};
use base64::Engine;

const SERVICE: &str = "Brave Safe Storage";
const ACCOUNT: &str = "Brave";

/// Fetch the Brave Safe Storage password from the macOS Keychain. On first invocation
/// the user will receive a Keychain approval prompt (same UX as Python `pycookiecheat`).
///
/// IMPORTANT: The Keychain entry stores 16 raw bytes. macOS `security -w` displays these
/// as base64 before handing them to `pycookiecheat`, and `pycookiecheat` passes the
/// base64 STRING as the PBKDF2 password. We mirror that behavior: if the raw value is
/// valid UTF-8 we use it as-is (matches the Python path when `security -w` returned an
/// ASCII string); otherwise we base64-encode the raw bytes (matches the Python path when
/// `security -w` had to base64-encode binary material for display).
pub fn fetch_brave_safe_storage_password() -> Result<String> {
    let bytes = security_framework::passwords::get_generic_password(SERVICE, ACCOUNT)
        .map_err(|e| Error::Keychain(e.to_string()))?;
    match String::from_utf8(bytes.clone()) {
        Ok(s) if s.is_ascii() => Ok(s),
        _ => Ok(base64::engine::general_purpose::STANDARD.encode(&bytes)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires macOS Keychain access (user prompt)"]
    fn test_live_keychain_lookup() {
        let pw = fetch_brave_safe_storage_password().expect("Keychain lookup");
        assert!(!pw.is_empty(), "Keychain password should be non-empty");
    }
}
