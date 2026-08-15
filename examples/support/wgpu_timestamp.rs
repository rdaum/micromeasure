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

//! Private support module for `examples/wgpu_timestamp_backend.rs`.
//!
//! `TimestampPair` is an application-facing timing utility, **not** a
//! `micromeasure::MeasurementBackend`. It is constructed from the
//! application's existing [`wgpu::Device`] and [`wgpu::Queue`] and never
//! discovers adapters, creates devices, submits work, or blocks on the
//! device. The application retains ownership of command encoding,
//! submission, synchronization, and device-loss handling: it must submit
//! the encoder containing `writes` plus `encode_resolve_copy`, call
//! [`TimestampPair::map_async`] before its own exact-submission wait, drive
//! the device to completion, and only then call
//! [`TimestampPair::finish_read`], which is nonblocking.
//!
//! Applications with different ownership or pipelining should copy this
//! module beside their own operation/runtime code rather than importing it
//! from micromeasure: this crate deliberately exposes no wgpu types in its
//! public API because wgpu releases are not type-compatible across versions.

use std::fmt;
use std::mem::size_of;
use std::sync::{Mutex, mpsc};
use std::time::Duration;

/// Number of timestamp query slots reserved per timing pair: one for the
/// beginning of the compute pass and one for its end.
pub const TIMESTAMP_QUERY_COUNT: u32 = 2;

/// Resolve/readback payload size: one `u64` tick per query slot.
const BYTE_COUNT: u64 = TIMESTAMP_QUERY_COUNT as u64 * size_of::<u64>() as u64;

/// Pure tick-to-duration conversion failure modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimestampConversionError {
    /// `stop < start`: the sample crossed a timestamp wrap boundary and is
    /// invalid. Retry it or fail the benchmark; do not guess.
    StopBeforeStart,
    /// `stop == start`: no measurable work happened in the timed pass.
    ZeroTicks,
    /// The timestamp period is non-finite, zero, or negative.
    InvalidPeriod,
    /// The converted duration is non-finite, overflows `Duration`, or
    /// truncates to zero.
    DurationOutOfRange,
}

impl fmt::Display for TimestampConversionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StopBeforeStart => {
                write!(f, "timestamp stop is before start (wrap boundary sample)")
            }
            Self::ZeroTicks => write!(f, "timestamp start and stop are equal (zero ticks)"),
            Self::InvalidPeriod => {
                write!(f, "timestamp period must be finite and greater than zero")
            }
            Self::DurationOutOfRange => {
                write!(
                    f,
                    "timestamp delta converts to a non-finite or overflowing duration"
                )
            }
        }
    }
}

impl std::error::Error for TimestampConversionError {}

/// Convert raw timestamp query ticks into a device elapsed [`Duration`].
///
/// Validation follows the timing rules from
/// `book/src/gpu-sharp-edges.md`:
///
/// 1. `stop.checked_sub(start)` must succeed (no wrap boundary);
/// 2. zero ticks are rejected rather than passed onward as
///    `Duration::ZERO`;
/// 3. `period_ns` must be finite and greater than zero;
/// 4. the conversion computes in `f64` to avoid premature precision loss;
/// 5. non-finite, overflowing, or zero-truncating durations are rejected.
///
/// The final conversion uses [`Duration::try_from_secs_f64`] (not
/// `from_secs_f64`), so values that round up to the overflow boundary return
/// an error instead of panicking.
pub fn ticks_to_duration(
    start: u64,
    stop: u64,
    period_ns: f32,
) -> Result<Duration, TimestampConversionError> {
    if !period_ns.is_finite() || period_ns <= 0.0 {
        return Err(TimestampConversionError::InvalidPeriod);
    }
    let ticks = stop
        .checked_sub(start)
        .ok_or(TimestampConversionError::StopBeforeStart)?;
    if ticks == 0 {
        return Err(TimestampConversionError::ZeroTicks);
    }
    let seconds = f64::from(period_ns) * (ticks as f64) / 1_000_000_000.0;
    let duration = Duration::try_from_secs_f64(seconds)
        .map_err(|_| TimestampConversionError::DurationOutOfRange)?;
    if duration.is_zero() {
        // Sub-nanosecond conversion truncation would otherwise hand the
        // runner a `Duration::ZERO`, which `with_primary_duration` rejects.
        return Err(TimestampConversionError::DurationOutOfRange);
    }
    Ok(duration)
}

/// Resource creation, mapping, and readback failures for [`TimestampPair`].
#[derive(Debug)]
pub enum TimestampError {
    Conversion(TimestampConversionError),
    /// The timing payload would exceed the device's `max_buffer_size`.
    InvalidBufferSize(u64),
    /// The device reported a lost/failed state while polling.
    DevicePoll(String),
    /// The device finished mapping the readback buffer with an error.
    MapCallback(String),
    /// The map callback has not been delivered yet; the application must
    /// drive device progress before calling `finish_read`.
    MapNotReady,
    /// The map callback channel was closed without delivering a result.
    MapChannelClosed,
    MapLockPoisoned,
    GetMappedRange(String),
}

impl fmt::Display for TimestampError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Conversion(error) => write!(f, "timestamp conversion failed: {error}"),
            Self::InvalidBufferSize(bytes) => {
                write!(
                    f,
                    "timestamp timing needs {bytes} bytes but the device max_buffer_size is smaller"
                )
            }
            Self::DevicePoll(error) => write!(f, "device poll failed: {error}"),
            Self::MapCallback(error) => write!(f, "readback buffer mapping failed: {error}"),
            Self::MapNotReady => {
                write!(f, "readback map is not ready; drive device progress first")
            }
            Self::MapChannelClosed => write!(f, "readback map channel closed without a result"),
            Self::MapLockPoisoned => write!(f, "readback map receiver lock is poisoned"),
            Self::GetMappedRange(error) => write!(f, "readback mapped range unavailable: {error}"),
        }
    }
}

impl std::error::Error for TimestampError {}

/// A reusable begin/end timestamp pair for timing compute passes on an
/// application-owned wgpu device.
///
/// Resource set (all created on the application's device):
///
/// ```text
/// query_set: QuerySet  (QueryType::Timestamp, two entries)
/// resolve:   Buffer    (QUERY_RESOLVE | COPY_SRC, 16 bytes)
/// readback:  Buffer    (COPY_DST | MAP_READ, 16 bytes)
/// ```
///
/// The resolve and readback buffers are deliberately separate: portable
/// WebGPU requires resolving into a query-resolve buffer and copying into a
/// separate map-readable buffer.
pub struct TimestampPair {
    device: wgpu::Device,
    query_set: wgpu::QuerySet,
    resolve: wgpu::Buffer,
    readback: wgpu::Buffer,
    timestamp_period_ns: f32,
    map_sender: mpsc::SyncSender<Result<(), wgpu::BufferAsyncError>>,
    map_receiver: Mutex<mpsc::Receiver<Result<(), wgpu::BufferAsyncError>>>,
}

impl TimestampPair {
    /// Construct a timing pair from the application's existing device and
    /// queue.
    ///
    /// Returns `Ok(None)` when the device does not have
    /// [`wgpu::Features::TIMESTAMP_QUERY`] enabled — i.e. the application did
    /// not request it. That is availability, not error: the application must
    /// decide (before sampling) whether to run a device-timed benchmark at
    /// all, and never mix host-timed samples into the same population.
    ///
    /// Returns `Err` when the timing payload would exceed the device's
    /// `max_buffer_size`. wgpu 30's `create_buffer`/`create_query_set` have
    /// no error return — their validation failures panic, which the
    /// preflight in the example treats as a fatal setup failure.
    ///
    /// This function does not discover adapters and does not request a
    /// second device.
    pub fn new(device: &wgpu::Device, queue: &wgpu::Queue) -> Result<Option<Self>, TimestampError> {
        if !device.features().contains(wgpu::Features::TIMESTAMP_QUERY) {
            return Ok(None);
        }
        if BYTE_COUNT > device.limits().max_buffer_size {
            return Err(TimestampError::InvalidBufferSize(BYTE_COUNT));
        }
        let query_set = device.create_query_set(&wgpu::QuerySetDescriptor {
            label: Some("micromeasure timestamp pair"),
            ty: wgpu::QueryType::Timestamp,
            count: TIMESTAMP_QUERY_COUNT,
        });
        let resolve = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("micromeasure timestamp resolve"),
            size: BYTE_COUNT,
            usage: wgpu::BufferUsages::QUERY_RESOLVE | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("micromeasure timestamp readback"),
            size: BYTE_COUNT,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let (map_sender, map_receiver) = mpsc::sync_channel(1);
        Ok(Some(Self {
            device: device.clone(),
            query_set,
            resolve,
            readback,
            timestamp_period_ns: queue.get_timestamp_period(),
            map_sender,
            map_receiver: Mutex::new(map_receiver),
        }))
    }

    /// Timestamp writes for the beginning and end of a compute pass.
    ///
    /// Pass this through
    /// `ComputePassDescriptor::timestamp_writes` on the pass that contains
    /// all dispatches belonging to one sample.
    pub fn writes(&self) -> wgpu::ComputePassTimestampWrites<'_> {
        wgpu::ComputePassTimestampWrites {
            query_set: &self.query_set,
            beginning_of_pass_write_index: Some(0),
            end_of_pass_write_index: Some(1),
        }
    }

    /// Append query resolution and the resolve→readback copy to the
    /// application's command encoder. The application then submits the
    /// encoder as part of its own submission plan; this helper never calls
    /// `queue.submit`.
    pub fn encode_resolve_copy(&self, encoder: &mut wgpu::CommandEncoder) {
        encoder.resolve_query_set(&self.query_set, 0..TIMESTAMP_QUERY_COUNT, &self.resolve, 0);
        encoder.copy_buffer_to_buffer(&self.resolve, 0, &self.readback, 0, Some(BYTE_COUNT));
    }

    /// Request mapping of the readback buffer.
    ///
    /// Call this after the application submitted the encoder containing
    /// `encode_resolve_copy` — ideally *before* the application blocks on
    /// that submission, so the submission wait delivers the map callback
    /// for free. The application then drives device progress (its own fence
    /// or an exact-submission `Device::poll`) before calling
    /// [`Self::finish_read`]. Splitting the two steps lets an application
    /// collect several pairs' worth of timestamps asynchronously.
    pub fn map_async(&self) {
        let slice = self.readback.slice(..BYTE_COUNT);
        let sender = self.map_sender.clone();
        slice.map_async(wgpu::MapMode::Read, move |result| {
            let _ = sender.send(result);
        });
    }

    /// Finish a pending map, validate the timestamps, and convert them to a
    /// device elapsed [`Duration`].
    ///
    /// Only valid after the application submitted the timed work, waited for
    /// that exact submission, and called [`Self::map_async`].
    ///
    /// This method **never blocks** and never waits on the device: the
    /// application owns synchronization. If the map callback has not been
    /// delivered yet, it performs at most one bounded, non-blocking
    /// `Device::poll(PollType::Poll)` drain (some backends deliver map
    /// callbacks one poll cycle after the submission wait completes) and
    /// then reports [`TimestampError::MapNotReady`] rather than waiting.
    pub fn finish_read(&self) -> Result<Duration, TimestampError> {
        let receiver = self
            .map_receiver
            .lock()
            .map_err(|_| TimestampError::MapLockPoisoned)?;
        match receiver.try_recv() {
            Ok(Ok(())) => {}
            Ok(Err(error)) => return Err(TimestampError::MapCallback(error.to_string())),
            Err(mpsc::TryRecvError::Empty) => {
                drop(receiver);
                self.device
                    .poll(wgpu::PollType::Poll)
                    .map_err(|error| TimestampError::DevicePoll(error.to_string()))?;
                let receiver = self
                    .map_receiver
                    .lock()
                    .map_err(|_| TimestampError::MapLockPoisoned)?;
                match receiver.try_recv() {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => {
                        return Err(TimestampError::MapCallback(error.to_string()));
                    }
                    Err(mpsc::TryRecvError::Empty) => return Err(TimestampError::MapNotReady),
                    Err(mpsc::TryRecvError::Disconnected) => {
                        return Err(TimestampError::MapChannelClosed);
                    }
                }
            }
            Err(mpsc::TryRecvError::Disconnected) => return Err(TimestampError::MapChannelClosed),
        }
        let mapped = self
            .readback
            .slice(..BYTE_COUNT)
            .get_mapped_range()
            .map_err(|error| TimestampError::GetMappedRange(error.to_string()))?;
        let mut raw = [0_u8; BYTE_COUNT as usize];
        raw.copy_from_slice(&mapped);
        drop(mapped);
        self.readback.unmap();
        let start = u64::from_le_bytes(raw[..8].try_into().expect("eight start bytes"));
        let stop = u64::from_le_bytes(raw[8..].try_into().expect("eight stop bytes"));
        ticks_to_duration(start, stop, self.timestamp_period_ns).map_err(TimestampError::Conversion)
    }
}
