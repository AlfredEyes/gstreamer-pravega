//
// Copyright (c) Dell Inc., or its subsidiaries. All Rights Reserved.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//

use gst::ClockTime;
use pravega_video::timestamp::{PravegaTimestamp};
use std::time::{SystemTime, UNIX_EPOCH};


pub fn clocktime_to_pravega(t: Option<ClockTime>) -> PravegaTimestamp {
    PravegaTimestamp::from_nanoseconds(t.map(ClockTime::nseconds))
}

pub fn pravega_to_clocktime(t: PravegaTimestamp) -> Option<ClockTime> {
    t.nanoseconds().map(ClockTime::from_nseconds)
}

/// Returns the current time as the number of nanoseconds since the NTP epoch, not including leap seconds.
pub fn now_ntp_clocktime() -> ClockTime {
    ClockTime::from_nseconds(SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos() as u64 + PravegaTimestamp::UNIX_TO_NTP_SECONDS * 1_000_000_000)
}
