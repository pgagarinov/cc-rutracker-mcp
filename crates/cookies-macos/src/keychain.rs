//! macOS Keychain lookup for Brave's cookie-storage password.

use crate::{Error, Result};

const SERVICE: &str = "Brave Safe Storage";
const ACCOUNT: &str = "Brave";

/// Fetch the Brave Safe Storage password from the macOS Keychain. On first invocation
/// the user will receive a Keychain approval prompt (same UX as Python `pycookiecheat`).
pub fn fetch_brave_safe_storage_password() -> Result<String> {
    let result = security_framework::passwords::get_generic_password(SERVICE, ACCOUNT);
    match result {
        Ok(bytes) => String::from_utf8(bytes)
            .map_err(|e| Error::Keychain(format!("password is not valid UTF-8: {e}"))),
        Err(err) => Err(Error::Keychain(err.to_string())),
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
