//! Charges for outgoing messages that last until HTTP/2 frees their bytes. A response carrying a [`Ledger`] in its
//! extensions hands each data frame the charges pushed since the frame before, through [`Bytes::from_owner`], so the
//! charges drop with the last copy of the frame in hyper's or h2's send buffers.

use bytes::Bytes;
use chunk_backend::SendCharge;
use http_body::{Body, Frame, SizeHint};
use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex, PoisonError},
    task::{Context, Poll, ready},
};
use tonic::{
    codegen::{Service, http},
    server::NamedService,
};

/// The length prefix gRPC adds to each message.
pub(super) const PREFIX_BYTES: usize = 5;
/// tonic hands on what it encoded once it reaches this much.
const YIELD_BYTES: usize = 32 * 1024;

/// Charges for messages a response body encoded but has yet to hand on in a frame.
#[derive(Clone, Default)]
pub(super) struct Ledger(Arc<Mutex<Vec<SendCharge>>>);

impl Ledger {
    /// Adds the charge of a message the response body encodes next.
    pub fn push(&self, charge: SendCharge) {
        let mut charges = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let held = charges.iter().map(SendCharge::bytes).sum::<usize>();
        debug_assert!(held < YIELD_BYTES, "no response body takes the ledger's charges");
        charges.push(charge);
    }

    fn take(&self) -> Vec<SendCharge> {
        std::mem::take(&mut self.0.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

/// A service whose response bodies hand their data frames the charges of the ledger in the response's extensions.
#[derive(Clone)]
pub(super) struct ChargeBodies<S>(pub S);

impl<S: NamedService> NamedService for ChargeBodies<S> {
    const NAME: &'static str = S::NAME;
}

impl<S, Request, B> Service<Request> for ChargeBodies<S>
where
    S: Service<Request, Response = http::Response<B>>,
    S::Future: Send + 'static,
{
    type Response = http::Response<ChargedBody<B>>;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, S::Error>> + Send>>;

    fn poll_ready(&mut self, context: &mut Context<'_>) -> Poll<Result<(), S::Error>> {
        self.0.poll_ready(context)
    }

    fn call(&mut self, request: Request) -> Self::Future {
        let response = self.0.call(request);
        Box::pin(async move {
            let mut response = response.await?;
            let ledger = response.extensions_mut().remove::<Ledger>();
            Ok(response.map(|inner| ChargedBody { inner, ledger }))
        })
    }
}

/// A response body whose data frames hold the charges its ledger took, or its inner body's frames untouched without
/// one.
pub(super) struct ChargedBody<B> {
    inner: B,
    ledger: Option<Ledger>,
}

impl<B: Body<Data = Bytes> + Unpin> Body for ChargedBody<B> {
    type Data = Bytes;
    type Error = B::Error;

    fn poll_frame(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Result<Frame<Bytes>, B::Error>>> {
        let frame = ready!(Pin::new(&mut self.inner).poll_frame(context));
        let Some(ledger) = &self.ledger else {
            return Poll::Ready(frame);
        };
        Poll::Ready(frame.map(|frame| frame.map(|frame| frame.map_data(|data| charged(data, ledger.take())))))
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

fn charged(data: Bytes, charges: Vec<SendCharge>) -> Bytes {
    if charges.is_empty() {
        return data;
    }
    Bytes::from_owner(Charged { data, _charges: charges })
}

/// A frame's bytes and the charges they hold.
struct Charged {
    data: Bytes,
    _charges: Vec<SendCharge>,
}

impl AsRef<[u8]> for Charged {
    fn as_ref(&self) -> &[u8] {
        &self.data
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chunk_backend::SendBudget;
    use http_body_util::{BodyExt, Full};

    async fn frame(ledger: Option<Ledger>, data: &Bytes) -> Bytes {
        let mut body = ChargedBody { inner: Full::new(data.clone()), ledger };
        body.frame().await.unwrap().unwrap().into_data().unwrap()
    }

    #[tokio::test]
    async fn a_frame_holds_its_charges_while_any_part_of_it_lives() {
        let budget = SendBudget::new(1024);
        let ledger = Ledger::default();
        ledger.push(budget.charge(100).unwrap());
        ledger.push(budget.charge(20).unwrap());
        let data = frame(Some(ledger.clone()), &Bytes::from_static(b"two messages")).await;
        assert_eq!(&data[..], b"two messages");
        assert!(ledger.take().is_empty());

        let slice = data.slice(4..);
        drop(data);
        let copy = slice.clone();
        drop(slice);
        assert_eq!(budget.bytes(), 120);
        drop(copy);
        assert_eq!(budget.bytes(), 0);
    }

    #[tokio::test]
    async fn frames_without_charges_pass_through() {
        let data = Bytes::from_static(b"message");
        assert_eq!(frame(None, &data).await.as_ptr(), data.as_ptr());
        assert_eq!(frame(Some(Ledger::default()), &data).await.as_ptr(), data.as_ptr());
    }
}
