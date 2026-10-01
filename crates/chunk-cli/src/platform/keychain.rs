//! The token's entry in the OS keychain: macOS Keychain, Windows Credential Manager or the Linux Secret Service.

use std::io;

use keyring_core::{Entry, Error};

use super::config::Secret;

const SERVICE: &str = "chunk";

/// `account`'s entry in the OS store, which `keyring` makes the default store on its first use.
fn entry(account: &str) -> Result<Entry, Error> {
    let _ = keyring::Entry::store_status();
    Entry::new(SERVICE, account)
}

/// The token saved for `account`, or none if there isn't one or the OS has no keychain.
pub(super) fn get(account: &str) -> io::Result<Option<Secret>> {
    match entry(account).and_then(|entry| entry.get_password()) {
        Ok(token) => Ok(Some(Secret::new(token))),
        Err(Error::NoEntry | Error::NoDefaultStore) => Ok(None),
        Err(error) => Err(failed("read", &error)),
    }
}

pub(super) fn set(account: &str, token: &Secret) -> Result<(), Error> {
    entry(account)?.set_password(token.expose())
}

pub(super) fn delete(account: &str) -> io::Result<()> {
    match entry(account).and_then(|entry| entry.delete_credential()) {
        Ok(()) | Err(Error::NoEntry | Error::NoDefaultStore) => Ok(()),
        Err(error) => Err(failed("delete", &error)),
    }
}

fn failed(action: &str, error: &Error) -> io::Error {
    io::Error::other(format!("cannot {action} the token in the OS keychain: {error}"))
}
