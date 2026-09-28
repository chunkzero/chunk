//! What remote runners start on their hosts. Each host is booted once: the first runner boot that asks binds it, and a
//! machine that stopped fully and boots again is refused, failing its host.

use crate::{
    Control, Error, MachineKind, Result,
    state::{self, Machine},
};
use std::collections::BTreeSet;

pub use crate::state::Launch;

/// The hosts whose launch the control state in `system` records, read without serving control, as before a fresh start
/// drops that state. Blocks on the store.
/// # Errors
/// Reports a store that cannot be read, or that a serving control holds.
pub fn recorded_launches(system: chunk_backend::System) -> Result<BTreeSet<String>> {
    Ok(state::load(system)?.launches.into_keys().collect())
}

impl Control {
    /// Records what a remote runner starts on `host`, with no boot bound yet, and `host`'s JVM machine, whose credential
    /// the runner presents, in one commit. Recording the same launch again changes nothing.
    /// # Errors
    /// Rejects a launch that differs from the one recorded, an invalid machine ID or one revoked, and reports a stopped
    /// store.
    pub fn record_launch(&self, host: &str, launch: Launch) -> Result<()> {
        crate::machines::validate(host)?;
        let launch = Launch { boot: None, ..launch };
        self.update(|state| {
            match state.machines.get(host) {
                Some(machine) if machine.kind != MachineKind::Jvm => {
                    return Err(Error::Invalid("the machine ID names another kind"));
                }
                Some(machine) if machine.revoked => return Err(Error::Invalid("the machine's credential was revoked")),
                Some(_) => {}
                None => {
                    let machine = Machine { kind: MachineKind::Jvm, created_at_ms: crate::now_ms(), revoked: false };
                    state.machines.insert(host.to_owned(), machine);
                }
            }
            match state.launches.get(host) {
                Some(recorded) if Launch { boot: None, ..recorded.clone() } == launch => Ok(()),
                Some(_) => Err(Error::Invalid("the host's launch was recorded differently")),
                None => {
                    state.launches.insert(host.to_owned(), launch);
                    Ok(())
                }
            }
        })
    }

    /// Revokes `host`'s JVM machine credential for good, recording the revocation even when none was minted, so no
    /// launch is recorded for `host` again. Its launch record stays until [`Self::remove_launch`].
    /// # Errors
    /// Rejects a machine of another kind, and reports a stopped store.
    pub fn revoke_launch(&self, host: &str) -> Result<()> {
        self.update(|state| match state.machines.get_mut(host) {
            Some(machine) if machine.kind != MachineKind::Jvm => {
                Err(Error::Invalid("the machine ID names another kind"))
            }
            Some(machine) => {
                machine.revoked = true;
                Ok(())
            }
            None => {
                let machine = Machine { kind: MachineKind::Jvm, created_at_ms: crate::now_ms(), revoked: true };
                state.machines.insert(host.to_owned(), machine);
                Ok(())
            }
        })
    }

    /// `host`'s launch, binding it to runner boot `boot` unless another boot holds it. Another boot fails the host in
    /// the same commit, so control releases it and places new capacity.
    /// # Errors
    /// Rejects a host with no launch, or one another boot holds, as invalid, and reports a stopped store.
    pub fn boot_launch(&self, host: &str, boot: &str) -> Result<Launch> {
        let booted = self.update(|state| {
            let launch = state.launches.get(host).ok_or(Error::Invalid("core launches nothing on this host"))?;
            match &launch.boot {
                Some(bound) if bound != boot => {
                    crate::capacity::stop(state, host, Some("the host's machine booted again".into()));
                    Ok(None)
                }
                Some(_) => Ok(Some(launch.clone())),
                None => {
                    let launch = Launch { boot: Some(boot.to_owned()), ..launch.clone() };
                    state.launches.insert(host.to_owned(), launch.clone());
                    Ok(Some(launch))
                }
            }
        })?;
        booted.ok_or_else(|| {
            self.wake_capacity();
            Error::Invalid("the host was already booted; a machine that stopped fully must be replaced")
        })
    }

    /// `host`'s launch, if one is recorded. None once the store stops.
    #[must_use]
    pub fn launch(&self, host: &str) -> Option<Launch> {
        self.state().ok()?.launches.get(host).cloned()
    }

    /// The hosts with a recorded launch.
    /// # Errors
    /// Reports a stopped store.
    pub fn launched_hosts(&self) -> Result<BTreeSet<String>> {
        Ok(self.state()?.launches.keys().cloned().collect())
    }

    /// Whether a runner may still run on `host`: its launch is recorded, or its JVM machine credential holds.
    /// # Errors
    /// Reports a stopped store.
    pub fn launch_may_run(&self, host: &str) -> Result<bool> {
        let state = self.state()?;
        let machine =
            state.machines.get(host).is_some_and(|machine| machine.kind == MachineKind::Jvm && !machine.revoked);
        Ok(machine || state.launches.contains_key(host))
    }

    /// Forgets `host`'s launch once no runner for it runs.
    /// # Errors
    /// Reports a stopped store.
    pub fn remove_launch(&self, host: &str) -> Result<()> {
        self.update(|state| {
            state.launches.remove(host);
            Ok(())
        })
    }
}
