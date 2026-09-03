//! Contains helper function to run operations across multiple services as the same time.

use futures::{Stream, StreamExt, TryStreamExt, stream::FuturesUnordered};

/// Runs a unary operation on all services.
///
/// The passed async function describes the operation to run on each service.
/// The control struct can be used to decide whether to ignore a response
/// (continuing with other backends) or return it.
///
/// Errors short-circuit.
///
/// FUTUREWORK: allow configuring errors to log only (and skip)
pub async fn race_unary<'a: 'f, 'f, SVC: 'a, SVCS, T, E, F, Fut>(
    services: SVCS,
    mut f: F,
) -> Result<T, Error<E>>
where
    F: FnMut(SVC) -> Fut + Copy + 'a,
    Fut: Future<Output = Option<Result<T, E>>> + 'a,
    SVCS: IntoIterator<Item = SVC> + 'a,
    T: Default,
{
    let mut requests = services
        .into_iter()
        .enumerate()
        .map(|(backend_idx, svc)| async move {
            f(svc)
                .await
                .map(|resp| resp.map_err(|err| Error::Backend(backend_idx, err)))
        })
        .collect::<FuturesUnordered<_>>();

    while let Some(resp) = requests.next().await {
        if let Some(resp) = resp {
            return resp;
        }
    }

    Ok(T::default())
}

/// Runs an operation returning a stream on all services.
///
/// The passed async function describes the operation to run on each service.
///
/// The returned `Option<_>` controls whether to ignore a response, such as an error,
/// or an empty stream (and continue with other backends) or return it.
pub fn race_stream<'a, SVC, SVCS, F, Fut, S, T, E>(
    services: SVCS,
    mut f: F,
) -> impl Stream<Item = Result<T, Error<E>>> + 'a
where
    F: FnMut(SVC) -> Fut + Copy + 'a,
    Fut: Future<Output = Option<S>> + 'a,
    SVCS: IntoIterator<Item = SVC> + 'a,
    SVC: 'a,
    S: Stream<Item = Result<T, E>>,
    E: Send + 'a,
    T: Send + 'a,
{
    let mut requests = services
        .into_iter()
        .enumerate()
        .map(|(backend_idx, svc)| async move {
            f(svc)
                .await
                .map(|stream| stream.map_err(move |err| Error::Backend(backend_idx, err)))
        })
        .collect::<FuturesUnordered<_>>();

    async_stream::stream! {
        while let Some(maybe_stream) = requests.next().await {
            if let Some(stream) = maybe_stream {
                // yield from the stream
                let mut stream = std::pin::pin!(stream);
                while let Some(elem) = stream.next().await {
                    yield elem
                }
            }
        }
    }
}

#[derive(thiserror::Error, Debug)]
pub enum Error<E> {
    #[error("error from service at index {0}")]
    Backend(usize, #[source] E),
}
