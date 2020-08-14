use std::error::Error as StdError;
use std::fmt::Debug;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use eventually_core::projection::Projection;
use eventually_core::store::{EventStore, Select};
use eventually_core::subscription::EventSubscriber;

use futures::stream::{Stream, StreamExt, TryStreamExt};

use tokio::sync::watch::{channel, Receiver, Sender};

/// Reusable builder for multiple [`Projector`] instances.
///
/// [`Projector`]: struct.Projector.html
pub struct ProjectorBuilder<Store, Subscriber> {
    store: Arc<Store>,
    subscriber: Arc<Subscriber>,
}

impl<Store, Subscriber> ProjectorBuilder<Store, Subscriber> {
    /// Creates a new builder instance using the provided [`EventStore`]
    /// and [`EventSubscriber`].
    ///
    /// [`EventStore`]: ../../../eventually-core/store/trait.EventStore.html
    /// [`EventSubscriber`]: ../../../eventually-core/subscription/trait.EventSubscriber.html
    pub fn new(store: Arc<Store>, subscriber: Arc<Subscriber>) -> Self {
        Self { store, subscriber }
    }

    /// Builds a new [`Projector`] for the [`Projection`]
    /// specified in the function type.
    ///
    /// [`Projector`]: struct.Projector.html
    /// [`Projection`]: ../../../eventually-core/projection/trait.Projection.html
    pub fn build<P>(&self) -> Projector<P, Store, Subscriber>
    where
        // NOTE: these bounds are required for Projector::run.
        P: Projection + Debug + Clone,
        Store: EventStore<SourceId = P::SourceId, Event = P::Event>,
        Subscriber: EventSubscriber<SourceId = P::SourceId, Event = P::Event>,
        <Store as EventStore>::Error: StdError + Send + Sync + 'static,
        <Subscriber as EventSubscriber>::Error: StdError + Send + Sync + 'static,
    {
        Projector::new(self.store.clone(), self.subscriber.clone())
    }
}

/// A `Projector` manages the state of a single [`Projection`]
/// by opening a long-running stream of all events coming from the [`EventStore`].
///
/// New instances of a `Projector` are obtainable through a [`ProjectorBuilder`]
/// instance.
///
/// The `Projector` will start updating the [`Projection`] state when [`run`]
/// is called.
///
/// At each update, the `Projector` will broadcast the latest version of the
/// [`Projection`] on a `Stream` obtainable through [`watch`].
///
/// [`Projection`]: ../../../eventually-core/projection/trait.Projection.html
/// [`EventStore`]: ../../../eventually-core/store/trait.EventStore.html
/// [`ProjectorBuilder`]: struct.ProjectorBuilder.html
/// [`run`]: struct.Projector.html#method.run
/// [`watch`]: struct.Projector.html#method.watch
pub struct Projector<P, Store, Subscriber>
where
    P: Projection,
{
    tx: Sender<P>,
    rx: Receiver<P>, // Keep the receiver to be able to clone it in watch().
    store: Arc<Store>,
    subscriber: Arc<Subscriber>,
    state: P,
    last_sequence_number: AtomicU32,
    projection: std::marker::PhantomData<P>,
}

impl<P, Store, Subscriber> Projector<P, Store, Subscriber>
where
    P: Projection + Debug + Clone,
    Store: EventStore<SourceId = P::SourceId, Event = P::Event>,
    Subscriber: EventSubscriber<SourceId = P::SourceId, Event = P::Event>,
    // NOTE: these bounds are needed for anyhow::Error conversion.
    <Store as EventStore>::Error: StdError + Send + Sync + 'static,
    <Subscriber as EventSubscriber>::Error: StdError + Send + Sync + 'static,
{
    fn new(store: Arc<Store>, subscriber: Arc<Subscriber>) -> Self {
        let state: P = Default::default();
        let (tx, rx) = channel(state.clone());

        Self {
            tx,
            rx,
            store,
            subscriber,
            state,
            last_sequence_number: Default::default(),
            projection: std::marker::PhantomData,
        }
    }

    /// Provides a `Stream` that receives the latest copy of the `Projection` state.
    pub fn watch(&self) -> impl Stream<Item = P> {
        self.rx.clone()
    }

    /// Starts the update of the `Projection` by processing all the events
    /// coming from the [`EventStore`].
    ///
    /// [`EventStore`]: ../../../eventually-core/store/trait.EventStore.html
    pub async fn run(&mut self) -> anyhow::Result<()> {
        // Create the Subscription first, so that once the future has been resolved
        // we'll start receiving events right away.
        //
        // This is to avoid losing events when waiting for the one-off stream
        // to resolve its future.
        //
        // The impact is that we _might_ get duplicated events from the one-off stream
        // and the subscription stream. Luckily, we can discard those by
        // keeping an internal state of the last processed sequence number,
        // and discard all those events that are found.
        let subscription = self.subscriber.subscribe_all().await?;
        let one_off_stream = self.store.stream_all(Select::All).await?;

        let mut stream = one_off_stream
            .map_err(anyhow::Error::from)
            .chain(subscription.map_err(anyhow::Error::from));

        while let Some(event) = stream.next().await {
            let event = event?;
            let expected_sequence_number = self.last_sequence_number.load(Ordering::SeqCst);
            let event_sequence_number = event.sequence_number();

            if event_sequence_number < expected_sequence_number {
                continue; // Duplicated event detected, let's skip it.
            }

            self.state = P::project(self.state.clone(), event);

            self.last_sequence_number.compare_and_swap(
                expected_sequence_number,
                event_sequence_number,
                Ordering::SeqCst,
            );

            // Notify watchers of the latest projection state.
            self.tx.broadcast(self.state.clone()).expect(
                "since this struct holds the original receiver, failures should not happen",
            );
        }

        Ok(())
    }
}