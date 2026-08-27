//! Event-stream assertion helpers: wait for a matching event within a
//! deadline, or assert silence over a window. The negative form succeeds only
//! via the timeout path: silence is a verdict, but loss of observation is not.
//! A source that closes mid-window (dropped subscription, dead forwarding
//! task, crashed assistant) yields [`ExpectError::Closed`], never a pass — we
//! cannot certify quiet on a channel we stopped watching.

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
///
/// Only the timeout elapsing is a pass. If the source closes before the window
/// is up we return [`ExpectError::Closed`]: an assertion of silence is only
/// meaningful while we are actually observing, and a closed source means we
/// stopped. Treating that as a pass would let a dead subscription satisfy a
/// negative hardware assertion, the worst failure mode for test infrastructure.
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
                None => return Err(ExpectError::Closed),
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

    #[tokio::test]
    async fn closed_source_fails_quiet_assertion() {
        // A dropped sender (dead subscription) must not read as silence.
        let (tx, mut rx) = tokio::sync::mpsc::channel::<u32>(8);
        drop(tx);
        let err = expect_quiet(&mut rx, Duration::from_secs(60), |v| *v == 7).await;
        assert!(matches!(err, Err(ExpectError::Closed)));
    }
}
