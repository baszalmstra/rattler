use elsa::sync::FrozenMap;
use parking_lot::Mutex;
use stable_deref_trait::StableDeref;
use std::collections::HashMap;
use std::{
    borrow::Borrow,
    future::Future,
    hash::Hash,
    sync::{Arc, Weak},
};
use thiserror::Error;
use tokio::sync::broadcast;

pub struct CacheMap<K, V, E> {
    inner: Arc<CacheMapInner<K, V, E>>,
}

type ResultChannel<E> = Weak<broadcast::Sender<Result<(), E>>>;

struct CacheMapInner<K, V, E> {
    values: FrozenMap<K, V>,
    in_flight: Mutex<HashMap<K, ResultChannel<Arc<Mutex<Option<E>>>>>>,
}

#[derive(Error, Clone)]
pub enum CoalescingError<E> {
    #[error(transparent)]
    CacheError(E),

    #[error("a concurrent running operation failed")]
    CoalescedOperationFailed,

    #[error("cancelled")]
    Cancelled,
}

impl<K, V, E> Default for CacheMap<K, V, E> {
    fn default() -> Self {
        Self {
            inner: Arc::new(CacheMapInner {
                values: Default::default(),
                in_flight: Mutex::new(Default::default()),
            }),
        }
    }
}

impl<K: Eq + Hash + Clone, V: StableDeref, E> CacheMap<K, V, E> {
    #[allow(clippy::await_holding_lock)]
    pub async fn get_or_cache<Q: ?Sized, F, Fut>(
        &self,
        key: &Q,
        f: F,
    ) -> Result<&V::Target, CoalescingError<E>>
    where
        K: Borrow<Q> + Send + Sync + 'static,
        Q: Hash + Eq + ToOwned<Owned = K>,
        E: Send + 'static,
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<V, E>> + Send + 'static,
        V: Send + Sync + 'static,
    {
        let inner = self.inner.as_ref();

        // Fast path, check if this value was previously already cached.
        if let Some(cached_value) = inner.values.get(key) {
            return Ok(cached_value);
        }

        // Otherwise, lock the in-flight map to check if there is an ongoing request
        let mut in_flight = inner.in_flight.lock();

        // If there is an ongoing request, subscribe to its output. Otherwise start a new request.
        let mut receiver = if let Some(sender) = in_flight.get(key).and_then(Weak::upgrade) {
            // There is an ongoing request, just wait for that request to finish.
            sender.subscribe()
        } else {
            let (tx, rx) = broadcast::channel::<Result<(), Arc<Mutex<Option<E>>>>>(1);
            let tx = Arc::new(tx);
            let key = key.to_owned();

            // Only store a weak reference in our map to ensure that if something panics we don't
            // create a deadlock.
            in_flight.insert(key.clone(), Arc::downgrade(&tx));

            // Call the closure first, so we don't send the entire closure across threads, just the
            // future it returns.
            let fut = f();

            let inner = self.inner.clone();
            tokio::spawn(async move {
                // Wait for the request to finish.
                let res = fut.await;

                // Broadcast the result to additional receivers.
                let mut in_flight = inner.in_flight.lock();
                let broadcast = match res {
                    Ok(value) => {
                        inner.values.insert(key.clone(), value);
                        Ok(())
                    }
                    Err(e) => Err(Arc::new(Mutex::new(Some(e)))),
                };

                let _ = tx.send(broadcast);
                in_flight.remove(key.borrow());
            });

            rx
        };

        // Drop the lock
        drop(in_flight);

        // Wait for the task to finish
        let result = receiver
            .recv()
            .await
            .map_err(|_| CoalescingError::Cancelled)?;

        // Get the result from the frozen set.
        match result {
            Err(err) => match Mutex::lock_arc(&err).take() {
                Some(e) => Err(CoalescingError::CacheError(e)),
                None => Err(CoalescingError::CoalescedOperationFailed),
            },
            Ok(_) => Ok(inner
                .values
                .get(key)
                .expect("value must be present in the frozen map")),
        }
    }
}
