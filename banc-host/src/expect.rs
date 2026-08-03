//! Event-stream assertion helpers: wait for a matching event within a
//! deadline, or assert silence over a window. The negative form succeeds via
//! the timeout path — "nothing observed" is a verdict, not an error.

use std::future::Future;
use std::time::Duration;

/// Anything that yields events asynchronously (a postcard-rpc subscription,
/// an mpsc receiver, an RTT line stream...). `None` means the source closed.
pub trait EventSource<T> {
    fn next(&mut self) -> impl Future<Output = Option<T>> + Send;
}

impl<T: Send> EventSource<T> for tokio::sync::mpsc::Receiver<T> {
    async fn next(&mut self) -> Option<T> {
        self.recv().await
    }
}

impl<T: Send> EventSource<T> for tokio::sync::mpsc::UnboundedReceiver<T> {
    async fn next(&mut self) -> Option<T> {
        self.recv().await
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ExpectError {
    #[error("deadline ({0:?}) elapsed without a matching event")]
    Deadline(Duration),
    #[error("event source closed without a matching event")]
    Closed,
    #[error("expected silence but observed: {0}")]
    Unexpected(String),
}

/// Wait until `pred` matches an event, discarding non-matching events.
pub async fn expect_matching<T, S: EventSource<T>>(
    source: &mut S,
    deadline: Duration,
    mut pred: impl FnMut(&T) -> bool,
) -> Result<T, ExpectError> {
    let result = tokio::time::timeout(deadline, async {
        loop {
            match source.next().await {
                Some(ev) if pred(&ev) => return Ok(ev),
                Some(_) => continue,
                None => return Err(ExpectError::Closed),
            }
        }
    })
    .await;
    match result {
        Ok(inner) => inner,
        Err(_) => Err(ExpectError::Deadline(deadline)),
    }
}

/// Assert that no event matching `pred` arrives within `window`.
/// The timeout elapsing is the success path.
pub async fn expect_quiet<T: std::fmt::Debug, S: EventSource<T>>(
    source: &mut S,
    window: Duration,
    mut pred: impl FnMut(&T) -> bool,
) -> Result<(), ExpectError> {
    let result = tokio::time::timeout(window, async {
        loop {
            match source.next().await {
                Some(ev) if pred(&ev) => return Err(ExpectError::Unexpected(format!("{ev:?}"))),
                Some(_) => continue,
                // Source closing early counts as quiet: nothing more can match.
                None => return Ok(()),
            }
        }
    })
    .await;
    match result {
        Ok(inner) => inner,
        Err(_) => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn matching_event_found_among_noise() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        tx.send(1).await.unwrap();
        tx.send(7).await.unwrap();
        let got = expect_matching(&mut rx, Duration::from_millis(100), |v| *v == 7)
            .await
            .unwrap();
        assert_eq!(got, 7);
    }

    #[tokio::test]
    async fn deadline_elapses_without_match() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<u32>(8);
        tx.send(1).await.unwrap();
        let err = expect_matching(&mut rx, Duration::from_millis(50), |v| *v == 7).await;
        assert!(matches!(err, Err(ExpectError::Deadline(_))));
    }

    #[tokio::test]
    async fn quiet_window_passes_and_catches_offender() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        tx.send(1).await.unwrap();
        expect_quiet(&mut rx, Duration::from_millis(50), |v| *v == 7)
            .await
            .unwrap();
        tx.send(7).await.unwrap();
        let err = expect_quiet(&mut rx, Duration::from_millis(50), |v| *v == 7).await;
        assert!(matches!(err, Err(ExpectError::Unexpected(_))));
    }
}
