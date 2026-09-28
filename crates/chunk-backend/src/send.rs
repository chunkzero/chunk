//! The send budget: memory outgoing sync messages hold from encoding until the transport frees them.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use crate::{Result, limits::Limit};

/// A byte budget [`SendCharge`]s draw from. Clones share it.
#[derive(Clone)]
pub struct SendBudget(Arc<Budget>);

struct Budget {
    held: AtomicUsize,
    total: usize,
}

/// Bytes of a [`SendBudget`] held until the charge drops.
pub struct SendCharge {
    budget: SendBudget,
    bytes: usize,
}

impl SendBudget {
    #[must_use]
    pub fn new(bytes: usize) -> Self {
        Self(Arc::new(Budget { held: AtomicUsize::new(0), total: bytes }))
    }

    /// Charges `bytes`, failing at once when the budget has no room for them.
    /// # Errors
    /// Reports exhausted send memory.
    pub fn charge(&self, bytes: usize) -> Result<SendCharge> {
        self.hold(bytes)?;
        Ok(SendCharge { budget: self.clone(), bytes })
    }

    /// Charges `bytes` even beyond the budget's room, which then refuses other charges until enough are released.
    #[must_use]
    pub fn overdraw(&self, bytes: usize) -> SendCharge {
        self.0.held.fetch_add(bytes, Ordering::AcqRel);
        SendCharge { budget: self.clone(), bytes }
    }

    /// Checks the budget has `bytes` left, without charging them.
    /// # Errors
    /// Reports exhausted send memory.
    pub fn check(&self, bytes: usize) -> Result<()> {
        if self.available() < bytes { Err(Limit::SendMemory.exceeded()) } else { Ok(()) }
    }

    /// Bytes held by charges, which overdrawn ones may take past the budget.
    #[must_use]
    pub fn bytes(&self) -> usize {
        self.0.held.load(Ordering::Acquire)
    }

    /// Bytes left to charge.
    #[must_use]
    pub fn available(&self) -> usize {
        self.0.total.saturating_sub(self.bytes())
    }

    fn hold(&self, bytes: usize) -> Result<()> {
        let held = self.0.held.fetch_update(Ordering::AcqRel, Ordering::Acquire, |held| {
            held.checked_add(bytes).filter(|&held| held <= self.0.total)
        });
        held.map(drop).map_err(|_| Limit::SendMemory.exceeded())
    }
}

impl SendCharge {
    #[must_use]
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// Moves `bytes` of this charge into a new one, or none when it holds fewer.
    pub fn split(&mut self, bytes: usize) -> Option<Self> {
        self.bytes = self.bytes.checked_sub(bytes)?;
        Some(Self { budget: self.budget.clone(), bytes })
    }

    /// Takes `other`'s bytes over.
    pub fn merge(&mut self, mut other: Self) {
        debug_assert!(Arc::ptr_eq(&self.budget.0, &other.budget.0), "charges of different budgets");
        self.bytes += std::mem::take(&mut other.bytes);
    }

    /// Grows or shrinks the charge to exactly `bytes`. A charge its budget has no room to grow stays as it was.
    /// # Errors
    /// Reports exhausted send memory.
    pub fn resize(&mut self, bytes: usize) -> Result<()> {
        if bytes > self.bytes {
            self.budget.hold(bytes - self.bytes)?;
        } else {
            self.budget.0.held.fetch_sub(self.bytes - bytes, Ordering::AcqRel);
        }
        self.bytes = bytes;
        Ok(())
    }
}

impl Drop for SendCharge {
    fn drop(&mut self) {
        self.budget.0.held.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_merged_and_resized_charges_hold_exactly_their_bytes() {
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
        charge.merge(part);
        assert_eq!((charge.bytes(), budget.bytes()), (25, 25));

        drop(charge);
        assert_eq!(budget.bytes(), 0);
    }

    #[test]
    fn an_overdrawn_budget_refuses_charges_until_it_has_room_again() {
        let budget = SendBudget::new(100);
        let held = budget.charge(60).unwrap();
        let overdrawn = budget.overdraw(80);
        assert_eq!((budget.bytes(), budget.available()), (140, 0));
        assert!(budget.charge(0).is_err());

        drop(held);
        assert_eq!(budget.available(), 20);
        let charge = budget.charge(20).unwrap();
        drop((charge, overdrawn));
        assert_eq!(budget.bytes(), 0);
    }
}
