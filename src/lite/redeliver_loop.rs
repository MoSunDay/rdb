//! Background task wiring of the idle auto-redelivery sweep (the
//! spawn/rhythm half; the scan/decision half lives in
//! [`super::redeliver`]): one [`sweep_from`] round every 200ms on the
//! blocking pool, cursors rotating between rounds (the group-discovery
//! cursor plus the per-group PEL resume map -- both owned here and
//! lent to each round); no task at all unless
//! `lite.redelivery_idle_ms` is configured.

use std::sync::Arc;
use std::time::Duration;

use crate::ds::expire;
use crate::state;

use super::redeliver::{sweep_from, ResumeMap};

/// Sweep rhythm, ms (flusher cadence).
const PERIOD_MS: u64 = 200;

/// Background sweep task: one [`sweep_from`] round every 200ms on the
/// blocking pool, cursors rotating between rounds; no task at all
/// unless configured.
pub fn spawn_redelivery_sweep(shared: Arc<state::Shared>) {
    if shared.conf.lite.redelivery_idle_ms == 0 {
        return;
    }
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_millis(PERIOD_MS));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker.tick().await; // consume the immediate first tick
        let mut cursor: Vec<u8> = Vec::new();
        let mut resumes = ResumeMap::new();
        loop {
            ticker.tick().await;
            // The blocking round owns both cursors for its duration
            // (a 'static closure), then hands them back with the tally.
            let (sh, from, mut round) = (
                Arc::clone(&shared),
                cursor.clone(),
                std::mem::take(&mut resumes),
            );
            // Sync scans + one synced write: blocking pool, never a
            // tokio worker; a JoinError keeps the cursors, next tick.
            if let Ok((_, _, next, back)) = tokio::task::spawn_blocking(move || {
                let (r, d, c) = sweep_from(&sh, expire::now_ms(), &from, &mut round);
                (r, d, c, round)
            })
            .await
            {
                cursor = next;
                resumes = back;
            }
        }
    });
}
