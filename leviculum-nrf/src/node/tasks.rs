//! Housekeeping tasks every LNode runs: periodic banners a late-attached
//! serial reader can still learn the build and state from.

use embassy_time::{Duration, Timer};

use crate::{info, log_critical};

#[embassy_executor::task]
pub async fn boot_log_repeater(initial_len: usize) {
    for _ in 0..6 {
        Timer::after(Duration::from_secs(10)).await;
        info!("[BOOT] path_table_initial_len={}", initial_len);
    }
}

/// Re-emit the firmware build banner periodically so a debug-serial
/// reader can verify the running git_sha at any time, not only inside the
/// short boot window. The CI auto-flash verify reads this back. Uses the
/// embassy time driver (same timing infra the other periodic tasks use);
/// no SD-reserved peripheral is touched directly.
///
/// The `[MEDIA]` and `[NAME ]` lines ride along for the same reason and
/// read their state live, so a runtime change shows up here within five
/// seconds and a capture attached after the boot window still learns
/// which carriers this board is on and what it is called.
#[embassy_executor::task]
pub async fn fw_build_banner(media_src: crate::media::Source) {
    loop {
        Timer::after(Duration::from_secs(5)).await;
        log_critical!("[FW_BUILD] {}", crate::FW_BUILD_STAMP);
        log_critical!("[TIME_SOURCE] source={}", crate::time_source_str());
        crate::media::log_banner(media_src);
        crate::name::log_banner();
        crate::identity::log_banner();
    }
}

/// Diagnostic: log stack and heap headroom every 5 s. Lets us see if
/// the executor-hang under load correlates with stack creeping toward
/// zero (overflow → HardFault) or heap exhaustion. Logs once per period
/// regardless; cheap.
#[embassy_executor::task]
pub async fn diag_mem_log() {
    loop {
        Timer::after(Duration::from_secs(5)).await;
        let stack = crate::stack_min_free();
        let (hu, hf) = crate::heap_stats();
        info!(
            "[DIAG_MEM] stack_min_free={} heap_used={} heap_free={}",
            stack, hu, hf
        );
    }
}
