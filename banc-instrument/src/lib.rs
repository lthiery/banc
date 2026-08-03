//! Bench instruments for banc.
//!
//! An instrument speaks in its own physical units (dB, volts, samples) —
//! never in the DUT's domain language. A programmable attenuator sets dB;
//! whether that means "link margin" is the downstream suite's business.
//!
//! Drivers live here when they are domain-neutral (RCDAT dB set/get, SCPI
//! pass-through, Saleae automation). Suites bind them to `[[instrument]]`
//! entries in the rig config by `kind`.

use async_trait::async_trait;

/// A piece of bench equipment with a health check. Object-safe so rigs can
/// hold heterogeneous instruments.
#[async_trait]
pub trait Instrument: Send + Sync {
    /// Instance name, matching the rig config entry.
    fn name(&self) -> &str;
    /// Driver key, e.g. "rcdat".
    fn kind(&self) -> &str;
    /// Cheap liveness/sanity check, run at fixture setup.
    async fn healthcheck(&mut self) -> anyhow::Result<()>;
}

/// Run `body` once per setpoint, applying each via `apply` first. Collects
/// per-setpoint results instead of stopping at the first failure, so a sweep
/// reports its whole profile.
pub async fn for_each_setpoint<S, B>(
    setpoints: impl IntoIterator<Item = S>,
    mut apply: impl AsyncFnMut(&S) -> anyhow::Result<()>,
    mut body: impl AsyncFnMut(&S) -> anyhow::Result<B>,
) -> Vec<(S, anyhow::Result<B>)> {
    let mut results = Vec::new();
    for sp in setpoints {
        let result = match apply(&sp).await {
            Ok(()) => body(&sp).await,
            Err(e) => Err(e.context("applying setpoint")),
        };
        results.push((sp, result));
    }
    results
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn sweep_collects_all_results() {
        let applied = std::sync::Mutex::new(Vec::new());
        let results = for_each_setpoint(
            [10u32, 20, 30],
            async |sp| {
                applied.lock().unwrap().push(*sp);
                Ok(())
            },
            async |sp| {
                if *sp == 20 {
                    anyhow::bail!("mid failure");
                }
                Ok(*sp * 2)
            },
        )
        .await;
        assert_eq!(*applied.lock().unwrap(), vec![10, 20, 30]);
        assert_eq!(results.len(), 3);
        assert_eq!(*results[0].1.as_ref().unwrap(), 20);
        assert!(results[1].1.is_err());
        assert_eq!(*results[2].1.as_ref().unwrap(), 60);
    }
}
