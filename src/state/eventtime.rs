//! Monotonic event clock. Klipper status updates carry an `eventtime` (seconds since
//! the host's reference clock); we expose a single monotonic source so every consumer agrees.

use std::sync::OnceLock;
use std::time::Instant;

static START: OnceLock<Instant> = OnceLock::new();

/// Seconds elapsed since the daemon started, as an f64. Monotonic.
pub fn eventtime() -> f64 {
    let start = START.get_or_init(Instant::now);
    start.elapsed().as_secs_f64()
}
