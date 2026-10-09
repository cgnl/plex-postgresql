use std::os::raw::c_void;
use std::sync::atomic::Ordering;

use crate::db_interpose_conn_utils::log_error;
use crate::pg_config::get_retry_delays_vec;

use super::super::threading::sleep_ms;
use super::pool_get_connection_inner_excluding;
use super::shared::{retry_count_get, retry_count_set, AcquireCtx};

pub(super) fn phase6_retry(ctx: &AcquireCtx<'_>) -> *mut c_void {
    let retry_count = retry_count_get();
    let delays = get_retry_delays_vec();
    let max_retries = delays.len() as i32;

    if retry_count < max_retries {
        let delay = delays[retry_count as usize];
        log_error(&format!(
            "Pool: no connection available, retry {}/{} in {}ms",
            retry_count + 1,
            max_retries,
            delay
        ));
        retry_count_set(retry_count + 1);
        sleep_ms(delay);

        let result = pool_get_connection_inner_excluding(ctx.db_path, ctx.exclude_conn);
        if !result.is_null() {
            retry_count_set(0);
        }
        return result;
    }

    log_error(&format!(
        "Pool: no available slots after {} retries (all {} slots busy)",
        max_retries,
        ctx.pm.configured_size.load(Ordering::Relaxed)
    ));
    retry_count_set(0);
    std::ptr::null_mut()
}
