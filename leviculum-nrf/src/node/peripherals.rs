//! Optional peripherals every LNode can carry: the GNSS receiver and the
//! battery sampler. Which of them a unit has is the board's pin map
//! ([`Board::wire`]); what they do once running is the same everywhere.

use embassy_executor::Spawner;

#[allow(unused_imports)]
use crate::info;
use crate::node::board::Board;

/// Spawn the GNSS and battery tasks the build has features for.
#[allow(unused_variables)]
pub fn start<B: Board>(
    spawner: &Spawner,
    #[cfg(feature = "gnss")] gnss: crate::gnss::GnssWiring,
    #[cfg(feature = "battery")] battery: crate::node::board::BatteryWiring,
) {
    #[cfg(feature = "gnss")]
    {
        crate::gnss::init(spawner, gnss);
        info!("{}", B::GNSS_SPAWNED_LINE);
    }

    #[cfg(feature = "battery")]
    {
        crate::battery::init(
            spawner,
            battery.saadc,
            battery.pin,
            battery.divider_enable,
            battery.scale,
        );
        info!("battery task spawned");
    }
}
