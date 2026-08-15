// Copyright 2026 Ryan Daum
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Pure unit tests for the wgpu timestamp tick-to-duration conversion used by
//! `examples/wgpu_timestamp_backend.rs`. The support module is private to
//! that example, so this test target includes it by path; no GPU or wgpu
//! instance is involved.

#![allow(dead_code)]

#[path = "../examples/support/wgpu_timestamp.rs"]
mod wgpu_timestamp;

use std::time::Duration;
use wgpu_timestamp::{TimestampConversionError, ticks_to_duration};

#[test]
fn ordinary_positive_delta() {
    assert_eq!(
        ticks_to_duration(1_000, 2_000, 1.0).unwrap(),
        Duration::from_nanos(1_000)
    );
}

#[test]
fn fractional_nanosecond_period() {
    assert_eq!(
        ticks_to_duration(0, 4, 0.5).unwrap(),
        Duration::from_nanos(2)
    );
}

#[test]
fn non_integral_period_conversion() {
    assert_eq!(
        ticks_to_duration(10, 14, 0.75).unwrap(),
        Duration::from_nanos(3)
    );
}

#[test]
fn equal_timestamps_are_rejected() {
    assert_eq!(
        ticks_to_duration(7, 7, 1.0).unwrap_err(),
        TimestampConversionError::ZeroTicks
    );
}

#[test]
fn stop_before_start_is_rejected() {
    assert_eq!(
        ticks_to_duration(2_000, 1_000, 1.0).unwrap_err(),
        TimestampConversionError::StopBeforeStart
    );
}

#[test]
fn non_finite_periods_are_rejected() {
    for period in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        assert_eq!(
            ticks_to_duration(0, 10, period).unwrap_err(),
            TimestampConversionError::InvalidPeriod
        );
    }
}

#[test]
fn zero_and_negative_periods_are_rejected() {
    for period in [0.0, -1.0, -0.5] {
        assert_eq!(
            ticks_to_duration(0, 10, period).unwrap_err(),
            TimestampConversionError::InvalidPeriod
        );
    }
}

#[test]
fn conversion_overflow_is_rejected() {
    assert_eq!(
        ticks_to_duration(0, u64::MAX, f32::MAX).unwrap_err(),
        TimestampConversionError::DurationOutOfRange
    );
}

#[test]
fn large_but_in_range_duration_is_accepted() {
    // 1e18 ticks at 1 ns/tick = 1e9 seconds, comfortably inside Duration's
    // range; guards against the overflow check being too aggressive.
    assert_eq!(
        ticks_to_duration(0, 1_000_000_000_000_000_000, 1.0).unwrap(),
        Duration::from_secs(1_000_000_000)
    );
}

#[test]
fn sub_nanosecond_truncation_is_rejected() {
    assert_eq!(
        ticks_to_duration(0, 1, 0.000_000_5).unwrap_err(),
        TimestampConversionError::DurationOutOfRange
    );
}
