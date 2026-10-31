use std::time::Duration;

/// Exponential backoff with jitter between reconnect attempts.
#[derive(Debug, Clone)]
pub struct Backoff {
    initial: Duration,
    max: Duration,
    current: Duration,
}

impl Backoff {
    pub fn new(initial: Duration, max: Duration) -> Self {
        Self { initial, max, current: initial }
    }

    /// The next delay: a random point in the upper half of the current step, so agents that
    /// lost the control plane together do not all come back at the same moment. Then doubles
    /// the step, up to the maximum.
    pub fn next_delay(&mut self) -> Duration {
        let step = self.current;
        self.current = (self.current * 2).min(self.max);
        step / 2 + step.mul_f64(rand::random::<f64>() / 2.0)
    }

    /// Starts over from the initial delay, after a connection that worked.
    pub fn reset(&mut self) {
        self.current = self.initial;
    }
}

impl Default for Backoff {
    fn default() -> Self {
        Self::new(Duration::from_secs(1), Duration::from_secs(60))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grows_to_the_maximum_with_jitter_and_resets() {
        let mut backoff = Backoff::new(Duration::from_secs(1), Duration::from_secs(8));
        let delays: Vec<_> = (0..6).map(|_| backoff.next_delay()).collect();
        backoff.reset();

        for (delay, step) in delays.iter().zip([1, 2, 4, 8, 8, 8]) {
            let step = Duration::from_secs(step);
            assert!(*delay >= step / 2 && *delay <= step, "{delay:?} outside {step:?}");
        }
        assert!(backoff.next_delay() <= Duration::from_secs(1));
    }
}
