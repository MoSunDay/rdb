//! Background task wiring of the idle auto-redelivery sweep (the
//! spawn/rhythm half; the scan/decision half lives in
//! [`super::redeliver`]): one [`sweep_from`] round every 200ms on the
//! blocking pool, cursor rotating between rounds; no task at all
//! unless `lite.redelivery_idle_ms` is configured.

use std::sync::Arc;
use std::time::Duration;

use crate::ds::expire;
use crate::state;

use super::redeliver::sweep_from;

/// Sweep rhythm, ms (flusher cadence).
const PERIOD_MS: u64 = 200;

/// Background sweep task: one [`sweep_from`] round every 200ms on the
/// blocking pool, cursor rotating; no task at all unless configured.
pub fn spawn_redelivery_sweep(shared: Arc<state::Shared>) {
    if shared.conf.lite.redelivery_idle_ms == 0 {
        return;
    }
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_millis(PERIOD_MS));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker.tick().await; // consume the immediate first tick
        let mut cursor: Vec<u8> = Vec::new();
        loop {
            ticker.tick().await;
            let (sh, from) = (Arc::clone(&shared), cursor.clone());
            // Sync scans + one synced write: blocking pool, never a
            // tokio worker; a JoinError keeps the cursor, next tick.
            if let Ok((_, _, next)) =
                tokio::task::spawn_blocking(move || sweep_from(&sh, expire::now_ms(), &from)).await
            {
                cursor = next;
            }
        }
    });
}
