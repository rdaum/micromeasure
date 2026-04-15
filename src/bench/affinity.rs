use crate::threading::{DetectionResult, detect_performance_cores, pin_current_thread_to_core};
use std::{
    hint::black_box,
    io,
    sync::atomic::{AtomicBool, Ordering},
};

#[cfg(target_os = "linux")]
use perf_event::{Builder, events::Hardware};

pub(super) fn warn_affinity_once(message: impl Into<String>) {
    static WARNED: AtomicBool = AtomicBool::new(false);
    if !WARNED.swap(true, Ordering::Relaxed) {
        eprintln!("⚠️  {}", message.into());
    }
}

#[cfg(target_os = "linux")]
fn core_has_usable_pmu(core_id: usize) -> bool {
    let original_affinity = capture_current_thread_affinity().ok();
    if pin_current_thread_to_core(core_id).is_err() {
        return false;
    }

    let mut counter = match Builder::new(Hardware::INSTRUCTIONS).build() {
        Ok(counter) => counter,
        Err(_) => return false,
    };
    if counter.enable().is_err() {
        return false;
    }

    let mut acc = 0_u64;
    for i in 0..100_000 {
        acc = acc.wrapping_add(i);
    }
    black_box(acc);

    let _ = counter.disable();
    let usable = match counter.read_count_and_time() {
        Ok(cat) => cat.count > 0 || cat.time_running > 0,
        Err(_) => false,
    };

    if let Some(mask) = original_affinity.as_ref() {
        let _ = restore_current_thread_affinity(mask);
    }

    usable
}

#[cfg(target_os = "linux")]
fn choose_default_pin_core(allowed_core_ids: &[usize]) -> Option<usize> {
    let mut candidates = candidate_pin_cores(allowed_core_ids);
    for core_id in &candidates {
        if core_has_usable_pmu(*core_id) {
            return Some(*core_id);
        }
    }
    candidates.drain(..1).next()
}

#[cfg(target_os = "linux")]
fn candidate_pin_cores(allowed_core_ids: &[usize]) -> Vec<usize> {
    let mut candidates = Vec::new();
    if let Ok(DetectionResult::PerformanceCores(selection)) = detect_performance_cores() {
        for core_id in selection.logical_processor_ids {
            if allowed_core_ids.contains(&core_id) && !candidates.contains(&core_id) {
                candidates.push(core_id);
            }
        }
    }
    for core_id in allowed_core_ids {
        if !candidates.contains(core_id) {
            candidates.push(*core_id);
        }
    }
    candidates
}

#[cfg(target_os = "linux")]
fn choose_concurrent_pin_cores(allowed_core_ids: &[usize], count: usize) -> Vec<usize> {
    let candidates = candidate_pin_cores(allowed_core_ids);
    let mut selected = Vec::new();

    for core_id in &candidates {
        if core_has_usable_pmu(*core_id) {
            selected.push(*core_id);
            if selected.len() == count {
                return selected;
            }
        }
    }

    for core_id in candidates {
        if !selected.contains(&core_id) {
            selected.push(core_id);
            if selected.len() == count {
                break;
            }
        }
    }

    selected
}

#[cfg(target_os = "linux")]
fn fallback_allowed_core_ids() -> Vec<usize> {
    let count = std::thread::available_parallelism()
        .map(|p| p.get())
        .unwrap_or(1);
    (0..count).collect()
}

#[cfg(target_os = "linux")]
fn current_allowed_core_ids() -> Vec<usize> {
    capture_current_thread_affinity()
        .ok()
        .map(|mask| core_ids_from_mask(&mask))
        .filter(|core_ids| !core_ids.is_empty())
        .unwrap_or_else(fallback_allowed_core_ids)
}

#[cfg(target_os = "linux")]
fn requested_pin_core() -> Option<usize> {
    std::env::var("BENCH_UTILS_PIN_CORE")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
}

#[cfg(target_os = "linux")]
fn selected_pin_core(allowed_core_ids: &[usize]) -> Option<usize> {
    requested_pin_core()
        .filter(|core_id| allowed_core_ids.contains(core_id))
        .or_else(|| choose_default_pin_core(allowed_core_ids))
}

#[cfg(target_os = "linux")]
fn pin_current_thread(core_id: usize, context: &str) -> bool {
    if let Err(error) = pin_current_thread_to_core(core_id) {
        warn_affinity_once(format!(
            "Could not pin {context} to core {core_id}: {error}. Continuing without pinning"
        ));
        return false;
    }
    true
}

#[cfg(target_os = "linux")]
fn capture_current_thread_affinity() -> io::Result<libc::cpu_set_t> {
    let mut cpuset: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    let result = unsafe {
        libc::pthread_getaffinity_np(
            libc::pthread_self(),
            std::mem::size_of::<libc::cpu_set_t>(),
            &mut cpuset,
        )
    };
    if result != 0 {
        return Err(io::Error::from_raw_os_error(result));
    }
    Ok(cpuset)
}

#[cfg(target_os = "linux")]
fn restore_current_thread_affinity(mask: &libc::cpu_set_t) -> io::Result<()> {
    let result = unsafe {
        libc::pthread_setaffinity_np(
            libc::pthread_self(),
            std::mem::size_of::<libc::cpu_set_t>(),
            mask,
        )
    };
    if result != 0 {
        return Err(io::Error::from_raw_os_error(result));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn core_ids_from_mask(mask: &libc::cpu_set_t) -> Vec<usize> {
    let mut core_ids = Vec::new();
    for core_id in 0..(libc::CPU_SETSIZE as usize) {
        let is_set = unsafe { libc::CPU_ISSET(core_id, mask) };
        if is_set {
            core_ids.push(core_id);
        }
    }
    core_ids
}

pub(super) struct BenchAffinityGuard {
    #[cfg(target_os = "linux")]
    restore_mask: Option<libc::cpu_set_t>,
    #[cfg(target_os = "linux")]
    did_pin: bool,
}

impl BenchAffinityGuard {
    pub(super) fn acquire() -> Self {
        #[cfg(target_os = "linux")]
        {
            let restore_mask = match capture_current_thread_affinity() {
                Ok(mask) => Some(mask),
                Err(error) => {
                    warn_affinity_once(format!(
                        "Could not capture existing benchmark thread affinity: {error}. Continuing with best effort pinning"
                    ));
                    None
                }
            };

            let allowed_core_ids = restore_mask
                .as_ref()
                .map(core_ids_from_mask)
                .filter(|core_ids| !core_ids.is_empty())
                .unwrap_or_else(fallback_allowed_core_ids);

            let Some(core_id) = selected_pin_core(&allowed_core_ids) else {
                warn_affinity_once(
                    "No logical cores detected for pinning; benchmark will run without CPU pinning",
                );
                return Self {
                    restore_mask: None,
                    did_pin: false,
                };
            };

            let did_pin = pin_current_thread(core_id, "benchmark thread");
            return Self {
                restore_mask: did_pin.then_some(restore_mask).flatten(),
                did_pin,
            };
        }

        #[cfg(not(target_os = "linux"))]
        {
            Self {}
        }
    }
}

impl Drop for BenchAffinityGuard {
    fn drop(&mut self) {
        #[cfg(target_os = "linux")]
        {
            if !self.did_pin {
                return;
            }

            let Some(mask) = &self.restore_mask else {
                return;
            };

            if let Err(error) = restore_current_thread_affinity(mask) {
                warn_affinity_once(format!(
                    "Could not restore benchmark thread affinity after run: {error}"
                ));
            }
        }
    }
}

#[cfg(target_os = "linux")]
pub(super) fn concurrent_worker_pin_cores(total_threads: usize) -> Vec<usize> {
    choose_concurrent_pin_cores(&current_allowed_core_ids(), total_threads)
}
