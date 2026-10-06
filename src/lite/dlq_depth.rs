//! DLQ depth metrics (`rdb_lite_dlq_depth`): the gauge half of the
//! dead-letter module (transfer/commit logic lives in [`super::dlq`]).
//! Refreshed by the lite background loop -- point reads over the
//! configured target set, never a latched scan.

use crate::monitor;
use crate::state;

use super::{dlq, model, offset};

/// Sum entry depth of configured DLQ targets (cached groups; point reads): `rdb_lite_dlq_depth`.
pub(crate) fn refresh_dlq_depth(shared: &state::Shared) {
    let mut total = 0u64;
    for name in offset::dlq_streams(&shared.lite.offsets) {
        if let Some(p) = model::stream_prefix(&name) {
            if let Ok(Some(meta)) = dlq::read_dlq_meta(shared, &p, &name) {
                total += meta.len;
            }
        }
    }
    monitor::set_lite_dlq_depth(&shared.monitor, total as f64);
}
