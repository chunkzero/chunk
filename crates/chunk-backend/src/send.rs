//! The send budget: memory outgoing sync messages hold from encoding until the transport frees them.

use std::sync::Arc;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::{Result, limits::Limit};

/// A byte budget [`SendCharge`]s draw from. Clones share it.
#[derive(Clone)]
pub struct SendBudget {
    memory: Arc<Semaphore>,
    total: usize,
}

/// Bytes of a [`SendBudget`] held until the charge drops.
pub struct SendCharge(OwnedSemaphorePermit);

impl SendBudget {
    #[must_use]
    pub fn new(bytes: usize) -> Self {
        Self { memory: Arc::new(Semaphore::new(bytes)), total: bytes }
    }

    /// Charges `bytes`, failing at once when the budget has no room for them.
    /// # Errors
    /// Reports exhausted send memory.
    pub fn charge(&self, bytes: usize) -> Result<SendCharge> {
        acquire(&self.memory, bytes).map(SendCharge)
    }

    /// Bytes held by charges.
    #[must_use]
    pub fn bytes(&self) -> usize {
        self.total - self.memory.available_permits()
    }

    /// Bytes left to charge.
    #[must_use]
    pub fn available(&self) -> usize {
        self.memory.available_permits()
    }
}

impl SendCharge {
    #[must_use]
    pub fn bytes(&self) -> usize {
        self.0.num_permits()
    }

    /// Moves `bytes` of this charge into a new one, or none when it holds fewer.
    pub fn split(&mut self, bytes: usize) -> Option<Self> {
        self.0.split(bytes).map(Self)
    }

    /// Grows or shrinks the charge to exactly `bytes`. A charge its budget has no room to grow stays as it was.
    /// # Errors
    /// Reports exhausted send memory.
    pub fn resize(&mut self, bytes: usize) -> Result<()> {
        let held = self.bytes();
        if bytes > held {
            self.0.merge(acquire(self.0.semaphore(), bytes - held)?);
        } else {
            drop(self.0.split(held - bytes));
        }
        Ok(())
    }
}

fn acquire(memory: &Arc<Semaphore>, bytes: usize) -> Result<OwnedSemaphorePermit> {
    let bytes = u32::try_from(bytes).map_err(|_| Limit::SendMemory.exceeded())?;
    memory.clone().try_acquire_many_owned(bytes).map_err(|_| Limit::SendMemory.exceeded())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_and_resized_charges_hold_exactly_their_bytes() {
        let budget = SendBudget::new(100);
        let mut charge = budget.charge(40).unwrap();
        let part = charge.split(15).unwrap();
        assert_eq!((charge.bytes(), part.bytes(), budget.bytes()), (25, 15, 40));
        assert!(charge.split(26).is_none());

        charge.resize(70).unwrap();
        assert_eq!(budget.bytes(), 85);
        assert!(charge.resize(86).is_err());
        assert_eq!((charge.bytes(), budget.bytes()), (70, 85));
        charge.resize(10).unwrap();
        assert_eq!(budget.bytes(), 25);

        drop((charge, part));
        assert_eq!(budget.bytes(), 0);
    }
}
