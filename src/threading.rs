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

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

const CPU_SYSFS_ROOT: &str = "/sys/devices/system/cpu";
const MIN_HETEROGENEITY_RATIO: f64 = 0.10;
const PERFORMANCE_THRESHOLD_RATIO: f64 = 0.90;

#[derive(Debug, Clone)]
pub struct PerformanceCoreSelection {
    pub logical_processor_ids: Vec<usize>,
}

#[derive(Debug, Clone)]
pub enum DetectionResult {
    PerformanceCores(PerformanceCoreSelection),
    NoSelection,
}

#[derive(Debug, Clone)]
struct PhysicalCoreMetrics {
    logical_processor_ids: Vec<usize>,
    capacity: Option<u32>,
    max_freq_khz: Option<u32>,
}

pub fn pin_current_thread_to_core(core_id: usize) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        let mut cpuset: libc::cpu_set_t = unsafe { std::mem::zeroed() };
        unsafe {
            libc::CPU_ZERO(&mut cpuset);
            libc::CPU_SET(core_id, &mut cpuset);
            let thread = libc::pthread_self();
            let result = libc::pthread_setaffinity_np(
                thread,
                std::mem::size_of::<libc::cpu_set_t>(),
                &cpuset,
            );
            if result != 0 {
                return Err(io::Error::from_raw_os_error(result));
            }
        }
    }

    Ok(())
}

pub fn detect_performance_cores() -> io::Result<DetectionResult> {
    #[cfg(not(target_os = "linux"))]
    {
        Ok(DetectionResult::NoSelection)
    }

    #[cfg(target_os = "linux")]
    {
        let cores = read_physical_core_metrics()?;
        if cores.is_empty() {
            return Ok(DetectionResult::NoSelection);
        }

        if let Some(selection) = select_performance_cores_by_metric(&cores, |core| core.capacity) {
            return Ok(DetectionResult::PerformanceCores(selection));
        }

        if let Some(selection) =
            select_performance_cores_by_metric(&cores, |core| core.max_freq_khz)
        {
            return Ok(DetectionResult::PerformanceCores(selection));
        }

        Ok(DetectionResult::NoSelection)
    }
}

#[cfg(target_os = "linux")]
fn read_physical_core_metrics() -> io::Result<Vec<PhysicalCoreMetrics>> {
    let mut physical_core_map: HashMap<(usize, usize), PhysicalCoreMetrics> = HashMap::new();
    for logical_processor_id in read_logical_processor_ids()? {
        let topology_path = cpu_path(logical_processor_id).join("topology");
        let package_id = read_u32(topology_path.join("physical_package_id"))
            .ok()
            .map_or(0usize, |v| v as usize);
        let core_id = read_u32(topology_path.join("core_id"))
            .ok()
            .map_or(logical_processor_id, |v| v as usize);
        let key = (package_id, core_id);

        let entry = physical_core_map
            .entry(key)
            .or_insert_with(|| PhysicalCoreMetrics {
                logical_processor_ids: Vec::new(),
                capacity: None,
                max_freq_khz: None,
            });

        entry.logical_processor_ids.push(logical_processor_id);

        if let Ok(capacity) = read_u32(cpu_path(logical_processor_id).join("cpu_capacity")) {
            entry.capacity = Some(
                entry
                    .capacity
                    .map_or(capacity, |current| current.max(capacity)),
            );
        }

        if let Ok(max_freq_khz) = read_u32(
            cpu_path(logical_processor_id)
                .join("cpufreq")
                .join("cpuinfo_max_freq"),
        ) {
            entry.max_freq_khz = Some(
                entry
                    .max_freq_khz
                    .map_or(max_freq_khz, |current| current.max(max_freq_khz)),
            );
        }
    }

    let mut cores: Vec<_> = physical_core_map.into_values().collect();
    for core in &mut cores {
        core.logical_processor_ids.sort_unstable();
    }
    cores.sort_by(|left, right| {
        left.logical_processor_ids
            .first()
            .cmp(&right.logical_processor_ids.first())
    });

    Ok(cores)
}

#[cfg(target_os = "linux")]
fn read_logical_processor_ids() -> io::Result<Vec<usize>> {
    let mut logical_processor_ids = Vec::new();
    for entry in fs::read_dir(CPU_SYSFS_ROOT)? {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Some(cpu_suffix) = name.strip_prefix("cpu") else {
            continue;
        };
        let Ok(lp_id) = cpu_suffix.parse::<usize>() else {
            continue;
        };
        let topology_path = entry.path().join("topology");
        if topology_path.exists() {
            logical_processor_ids.push(lp_id);
        }
    }
    logical_processor_ids.sort_unstable();
    Ok(logical_processor_ids)
}

#[cfg(target_os = "linux")]
fn select_performance_cores_by_metric(
    cores: &[PhysicalCoreMetrics],
    metric: impl Fn(&PhysicalCoreMetrics) -> Option<u32>,
) -> Option<PerformanceCoreSelection> {
    let mut values = Vec::new();
    for core in cores {
        if let Some(value) = metric(core) {
            values.push(value);
        }
    }

    if values.len() < 2 {
        return None;
    }

    values.sort_unstable();
    values.dedup();

    if values.len() < 2 {
        return None;
    }

    let min_metric = *values.first()?;
    let max_metric = *values.last()?;
    if max_metric == 0 {
        return None;
    }

    let heterogeneity_ratio = (max_metric - min_metric) as f64 / max_metric as f64;
    if heterogeneity_ratio < MIN_HETEROGENEITY_RATIO {
        return None;
    }

    let threshold = (max_metric as f64 * PERFORMANCE_THRESHOLD_RATIO).round() as u32;
    let mut logical_processor_ids = Vec::new();
    let mut selected_cores = 0usize;
    for core in cores {
        let Some(core_metric) = metric(core) else {
            continue;
        };
        if core_metric < threshold {
            continue;
        }
        selected_cores += 1;
        logical_processor_ids.extend_from_slice(&core.logical_processor_ids);
    }

    if logical_processor_ids.is_empty() || selected_cores == cores.len() {
        return None;
    }

    logical_processor_ids.sort_unstable();
    logical_processor_ids.dedup();

    Some(PerformanceCoreSelection {
        logical_processor_ids,
    })
}

#[cfg(target_os = "linux")]
fn read_u32(path: PathBuf) -> io::Result<u32> {
    let value = fs::read_to_string(&path)?;
    value
        .trim()
        .parse::<u32>()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{path:?}: {e}")))
}

#[cfg(target_os = "linux")]
fn cpu_path(logical_processor_id: usize) -> PathBuf {
    Path::new(CPU_SYSFS_ROOT).join(format!("cpu{logical_processor_id}"))
}
