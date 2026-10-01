//! The token's entry in the OS keychain: macOS Keychain or Windows Credential Manager. A keychain that cannot store,
//! read or delete the token is an error, never a reason to keep the token elsewhere.

use std::io;

use keyring_core::{Entry, Error};

use super::config::Secret;

const SERVICE: &str = "chunk";

/// `account`'s entry in the OS store, which becomes the default store on first use.
fn entry(account: &str) -> Result<Entry, Error> {
    if keyring_core::get_default_store().is_none() {
        #[cfg(target_os = "macos")]
        let store = apple_native_keyring_store::keychain::Store::new()?;
        #[cfg(target_os = "windows")]
        let store = windows_native_keyring_store::Store::new()?;
        keyring_core::set_default_store(store);
    }
    Entry::new(SERVICE, account)
}

/// The token saved for `account`, if there is one.
pub(super) fn get(account: &str) -> io::Result<Option<Secret>> {
    match entry(account).and_then(|entry| entry.get_password()) {
        Ok(token) => Ok(Some(Secret::new(token))),
        Err(Error::NoEntry) => Ok(None),
        Err(error) => Err(failed("read", &error)),
    }
}

pub(super) fn set(account: &str, token: &Secret) -> io::Result<()> {
    entry(account).and_then(|entry| entry.set_password(token.expose())).map_err(|error| failed("save", &error))
}

pub(super) fn delete(account: &str) -> io::Result<()> {
    match entry(account).and_then(|entry| entry.delete_credential()) {
        Ok(()) | Err(Error::NoEntry) => Ok(()),
        Err(error) => Err(failed("delete", &error)),
    }
}

fn failed(action: &str, error: &Error) -> io::Error {
    io::Error::other(format!("cannot {action} the token in the OS keychain: {error}"))
}
