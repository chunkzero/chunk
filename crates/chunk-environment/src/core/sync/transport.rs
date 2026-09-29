//! Charges for outgoing messages that last until HTTP/2 frees their bytes. A response carrying a [`Ledger`] in its
//! extensions hands each data frame the charges pushed since the frame before, through [`Bytes::from_owner`], so the
//! charges drop with the last copy of the frame in hyper's or h2's send buffers. A response carrying [`Updates`] sends
//! each of them first, in a frame of exactly its message that holds its charge the same way.

use super::streams::Stream;
use bytes::Bytes;
use chunk_backend::SendCharge;
use chunk_proto::sync::v1::Update;
use http_body::{Body, Frame, SizeHint};
use prost::Message;
use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex, PoisonError},
    task::{Context, Poll, ready},
};
use tokio_stream::Stream as _;
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

/// A stream's updates, which a response sends before the body tonic encoded. Tonic encodes into a buffer each stream
/// keeps at the size of its largest message, while these frames are freed once sent.
#[derive(Clone)]
pub(super) struct Updates(Arc<Mutex<Option<Stream>>>);

impl Updates {
    pub fn new(stream: Stream) -> Self {
        Self(Arc::new(Mutex::new(Some(stream))))
    }

    pub fn take(&self) -> Option<Stream> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).take()
    }
}

/// A service whose response bodies send the [`Updates`] in the response's extensions, then hand their data frames the
/// charges of the ledger there.
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
            let extensions = response.extensions_mut();
            let ledger = extensions.remove::<Ledger>();
            let updates = extensions.remove::<Updates>().and_then(|updates| updates.take());
            Ok(response.map(|inner| ChargedBody { inner, ledger, updates }))
        })
    }
}

/// A response body that sends its updates, then its inner body's frames, whose data holds the charges its ledger took,
/// or is untouched without one.
pub(super) struct ChargedBody<B> {
    inner: B,
    ledger: Option<Ledger>,
    updates: Option<Stream>,
}

impl<B: Body<Data = Bytes> + Unpin> Body for ChargedBody<B> {
    type Data = Bytes;
    type Error = B::Error;

    fn poll_frame(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Result<Frame<Bytes>, B::Error>>> {
        if let Some(updates) = &mut self.updates {
            if let Some((update, charge)) = ready!(Pin::new(updates).poll_next(context)) {
                return Poll::Ready(Some(Ok(Frame::data(message(&update, charge)))));
            }
            self.updates = None;
        }
        let frame = ready!(Pin::new(&mut self.inner).poll_frame(context));
        let Some(ledger) = &self.ledger else {
            return Poll::Ready(frame);
        };
        Poll::Ready(frame.map(|frame| frame.map(|frame| frame.map_data(|data| charged(data, ledger.take())))))
    }

    fn is_end_stream(&self) -> bool {
        self.updates.is_none() && self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        if self.updates.is_some() { SizeHint::default() } else { self.inner.size_hint() }
    }
}

/// `update` as an uncompressed gRPC message, in bytes of exactly its length that hold `charge`.
fn message(update: &Update, charge: Option<SendCharge>) -> Bytes {
    let len = update.encoded_len();
    let mut data = Vec::with_capacity(PREFIX_BYTES + len);
    data.push(0);
    data.extend_from_slice(&u32::try_from(len).expect("updates fit the message limit").to_be_bytes());
    update.encode(&mut data).expect("a Vec grows to fit");
    charged(data.into(), charge.into_iter().collect())
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
    use super::{super::streams, *};
    use chunk_backend::SendBudget;
    use chunk_proto::sync::v1::{Entry, entry::State};
    use http_body_util::{BodyExt, Full};

    async fn frame(ledger: Option<Ledger>, data: &Bytes) -> Bytes {
        let mut body = ChargedBody { inner: Full::new(data.clone()), ledger, updates: None };
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

    #[tokio::test]
    async fn each_update_goes_out_in_a_frame_holding_only_its_message() {
        let budget = SendBudget::new(4 * 1024 * 1024);
        let (sender, stream) = streams::channel(budget.clone());
        let inner = Full::new(Bytes::from_static(b"end"));
        let mut body = ChargedBody { inner, ledger: None, updates: Some(stream) };
        let mut next = async || body.frame().await.unwrap().unwrap().into_data().unwrap();
        let update = |value: Vec<u8>| Update {
            upserts: vec![Entry { key: "a".into(), state: Some(State::Value(value.into())) }],
            ..Update::default()
        };

        let large = update(vec![1; 1024 * 1024]);
        sender.send(large.clone());
        let first = next().await;
        let len = u32::try_from(large.encoded_len()).unwrap().to_be_bytes();
        assert_eq!((first[0], &first[1..PREFIX_BYTES]), (0, &len[..]));
        assert_eq!(Update::decode(&first[PREFIX_BYTES..]).unwrap(), large);
        assert_eq!(budget.bytes(), first.len());

        sender.send(update(vec![2; 16]));
        let second = next().await;
        assert_eq!(second.len(), PREFIX_BYTES + update(vec![2; 16]).encoded_len());
        drop((first, second));
        assert_eq!(budget.bytes(), 0);

        drop(sender);
        assert_eq!(next().await, "end");
    }
}
