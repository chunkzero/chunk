//! The machines core minted credentials for, and whether each credential still holds.

use crate::{Control, Error, Result, state::Machine};

pub use crate::state::MachineKind;

impl Control {
    /// Records machine `id` of `kind`, whose credential then holds until it's revoked. Recording it again changes
    /// nothing.
    /// # Errors
    /// Rejects an ID that isn't 1 to 128 visible ASCII characters without `/`, one recorded for another kind or
    /// revoked, and reports a stopped store.
    pub fn add_machine(&self, id: &str, kind: MachineKind) -> Result<()> {
        if id.is_empty() || id.len() > 128 || !id.bytes().all(|byte| byte.is_ascii_graphic() && byte != b'/') {
            return Err(Error::Invalid("invalid machine ID"));
        }
        self.update(|state| match state.machines.get(id) {
            Some(machine) if machine.kind != kind => Err(Error::Invalid("the machine ID names another kind")),
            Some(machine) if machine.revoked => Err(Error::Invalid("the machine's credential was revoked")),
            Some(_) => Ok(()),
            None => {
                let machine = Machine { kind, created_at_ms: crate::now_ms(), revoked: false };
                state.machines.insert(id.to_owned(), machine);
                Ok(())
            }
        })
    }

    /// Revokes the credential of machine `id` of `kind` for good.
    /// # Errors
    /// Rejects an unknown machine or one of another kind, and reports a stopped store.
    pub fn revoke_machine(&self, id: &str, kind: MachineKind) -> Result<()> {
        self.update(|state| {
            let machine = state.machines.get_mut(id).filter(|machine| machine.kind == kind);
            machine.ok_or(Error::Invalid("unknown machine"))?.revoked = true;
            Ok(())
        })
    }

    /// Whether machine `id` of `kind` holds a credential that isn't revoked. False once the store stops.
    #[must_use]
    pub fn machine(&self, id: &str, kind: MachineKind) -> bool {
        let state = self.state();
        state.is_ok_and(|state| state.machines.get(id).is_some_and(|machine| machine.kind == kind && !machine.revoked))
    }
}
