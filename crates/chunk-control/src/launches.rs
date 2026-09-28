//! What remote runners start on their hosts. Each host is booted once: the first runner boot that asks binds it, and a
//! machine that stopped fully and boots again is refused.

use crate::{Control, Error, Result};

pub use crate::state::Launch;

impl Control {
    /// Records what a remote runner starts on `host`, with no boot bound yet. Recording the same launch again changes
    /// nothing.
    /// # Errors
    /// Rejects a launch that differs from the one recorded, and reports a stopped store.
    pub fn record_launch(&self, host: &str, launch: Launch) -> Result<()> {
        let launch = Launch { boot: None, ..launch };
        self.update(|state| match state.launches.get(host) {
            Some(recorded) if Launch { boot: None, ..recorded.clone() } == launch => Ok(()),
            Some(_) => Err(Error::Invalid("the host's launch was recorded differently")),
            None => {
                state.launches.insert(host.to_owned(), launch);
                Ok(())
            }
        })
    }

    /// `host`'s launch, binding it to runner boot `boot` unless another boot holds it.
    /// # Errors
    /// Rejects a host with no launch, or one another boot holds, as invalid, and reports a stopped store.
    pub fn boot_launch(&self, host: &str, boot: &str) -> Result<Launch> {
        self.update(|state| {
            let launch = state.launches.get_mut(host).ok_or(Error::Invalid("core launches nothing on this host"))?;
            match &launch.boot {
                Some(bound) if bound != boot => {
                    Err(Error::Invalid("the host was already booted; a machine that stopped fully must be replaced"))
                }
                Some(_) => Ok(launch.clone()),
                None => {
                    launch.boot = Some(boot.to_owned());
                    Ok(launch.clone())
                }
            }
        })
    }

    /// `host`'s launch, if one is recorded. None once the store stops.
    #[must_use]
    pub fn launch(&self, host: &str) -> Option<Launch> {
        self.state().ok()?.launches.get(host).cloned()
    }
}
