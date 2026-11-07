//! Telemetry readings: what this unit's sensors say for one evaluation.

/// Read this board's sensors for one telemetry evaluation.
///
/// Returns the readings and whether GNSS presence is `Fix` — only that
/// state may contribute a position (#240), and the policy is told
/// separately rather than having to infer it from the numbers.
///
/// The per-board part of telemetry is exactly this function: which
/// peripherals exist. This board has an L76K like the T114's and a
/// battery sampler like both, so the two `cfg` arms below read the same
/// way they do there, and since 2026-09-16 both features are on (#233):
/// a pack voltage and a position from the same report.
///
/// The battery field is filled from the same ADC task the `BATTERY` log
/// line reads (#380). A pack voltage on the air is the one reading that
/// reaches an operator who is still in the field, which is the situation
/// #380 exists for, and on a XIAO kit in a winter it is the reading
/// that says whether the panel is keeping up.
///
/// The die temperature it does have: every nRF52840 carries one and the
/// SoftDevice is enabled on every board we build, so it is read here
/// through the only legal path
/// ([`crate::telemetry::die_temperature_quarter_c`]).
pub fn collect_readings<R, C, S>(
    node: &leviculum_core::node::NodeCore<R, C, S>,
    sd: &nrf_softdevice::Softdevice,
) -> (crate::telemetry::Readings, bool)
where
    R: rand_core::CryptoRngCore,
    C: leviculum_core::traits::Clock,
    S: leviculum_core::traits::Storage,
{
    // Without the gnss and battery features nothing mutates this; the die
    // temperature is set in the initialiser, so it does not lift the gate.
    #[cfg_attr(
        not(any(feature = "gnss", feature = "battery")),
        allow(unused_mut, clippy::let_and_return)
    )]
    let mut readings = crate::telemetry::Readings {
        // A timebase below the plausibility floor is uptime seconds, not a
        // calendar estimate — the anchor model's "never ahead" rule has
        // nothing to work with there. Which arm anchored it is reported
        // alongside every reading as `[TIME_SOURCE]`.
        unix_secs: node
            .has_plausible_wall_clock()
            .then(|| node.emission_secs()),
        die_temperature_quarter_c: crate::telemetry::die_temperature_quarter_c(sd),
        // Not feature-gated and not board-specific: every board we build
        // has the same SX1262, and what it last heard is the one sensor
        // that describes the mesh rather than the box.
        link: crate::telemetry::link_reading(),
        ..Default::default()
    };
    #[cfg(not(feature = "gnss"))]
    let has_fix = false;
    #[cfg(feature = "gnss")]
    let has_fix = {
        use crate::baseboard::{GnssPresence, GNSS_FIX, GNSS_PRESENCE};
        if let Some(fix) = GNSS_FIX.try_get() {
            readings.latitude = fix.latitude;
            readings.longitude = fix.longitude;
            readings.altitude_m = fix.altitude_m;
            readings.speed_mps = fix.speed_mps;
            readings.bearing_deg = fix.bearing_deg;
            readings.hdop = fix.hdop;
        }
        matches!(
            GNSS_PRESENCE.try_get().map(|p| p.state),
            Some(GnssPresence::Fix)
        )
    };
    #[cfg(feature = "battery")]
    {
        // `and_then`, not `map`: the published percentage is itself an
        // Option since #380, and a pack whose voltage its classification
        // cannot explain sends no battery sensor at all rather than a
        // number a collector cannot check.
        readings.battery_percent = crate::baseboard::BATTERY_STATE
            .try_get()
            .and_then(|b| b.percent);
    }
    (readings, has_fix)
}
