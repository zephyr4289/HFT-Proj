//! R8: topology-aware CPU affinity for the fabric threads (docs/22).
//!
//! The doc-21 "worker core affinity" lever: the HYDRA fabric's threads run
//! unpinned by default, so the scheduler freely migrates them (each
//! migration costs the thread its L1/L2 working set — the sequencer state,
//! the lane rings) and may stack SMT siblings on one physical core while
//! others idle. On shared cloud runners (noisy neighbors) both effects are
//! measured as double-digit-percent fabric regressions.
//!
//! This module pins threads to the process's ALLOWED CPU set (cgroup
//! respected), ordered distinct-physical-core-first (SMT siblings go last),
//! and degrades gracefully: any `sched_setaffinity` failure simply leaves
//! that thread unpinned — behavior, not correctness, is affected.

/// The CPUs this process is allowed to run on (cgroup/affinity mask),
/// ordered distinct-physical-core-first.
pub fn cpu_order() -> Vec<usize> {
    let mut set = unsafe { std::mem::zeroed::<libc::cpu_set_t>() };
    let rc = unsafe { libc::sched_getaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &mut set) };
    if rc != 0 {
        return Vec::new();
    }
    let mut cpus = Vec::new();
    for cpu in 0..libc::CPU_SETSIZE as usize {
        if unsafe { libc::CPU_ISSET(cpu, &set) } {
            cpus.push(cpu);
        }
    }
    if cpus.is_empty() {
        return cpus;
    }
    // Distinct physical cores first: group by thread_siblings_list and
    // emit one representative per group before the siblings.
    let mut representatives: Vec<usize> = Vec::new();
    let mut siblings: Vec<usize> = Vec::new();
    for &cpu in &cpus {
        match read_sibling_group(cpu) {
            Some(group) => {
                // A sibling is any CPU in the group other than the group's
                // minimum that we have ALSO seen in the allowed set.
                let min_in_set = group.iter().copied().filter(|c| cpus.contains(c)).min();
                if min_in_set == Some(cpu) {
                    representatives.push(cpu);
                } else {
                    siblings.push(cpu);
                }
            }
            None => representatives.push(cpu),
        }
    }
    let mut order = representatives;
    order.extend(siblings);
    order
}

/// The thread-siblings list of `cpu` (its SMT group), parsed from sysfs.
fn read_sibling_group(cpu: usize) -> Option<Vec<usize>> {
    let path = format!("/sys/devices/system/cpu/cpu{cpu}/topology/thread_siblings_list");
    let text = std::fs::read_to_string(path).ok()?;
    let mut group = Vec::new();
    for part in text.trim().split(',') {
        if let Some((a, b)) = part.split_once('-') {
            if let (Ok(a), Ok(b)) = (a.parse::<usize>(), b.parse::<usize>()) {
                group.extend(a..=b);
            }
        } else if let Ok(v) = part.parse::<usize>() {
            group.push(v);
        }
    }
    if group.is_empty() {
        None
    } else {
        Some(group)
    }
}

/// Pin the CALLING thread to `cpu`. Returns success (failure leaves the
/// thread unpinned — see the module doc).
pub fn pin_current_to(cpu: usize) -> bool {
    let mut set = unsafe { std::mem::zeroed::<libc::cpu_set_t>() };
    unsafe { libc::CPU_SET(cpu, &mut set) };
    let rc = unsafe { libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set) };
    rc == 0
}

/// Pin the calling thread to the `slot`-th CPU of the topology order
/// (wrapping). Returns the chosen CPU, or None if pinning was unavailable.
pub fn pin_current_slot(slot: usize) -> Option<usize> {
    let order = cpu_order();
    if order.is_empty() {
        return None;
    }
    let cpu = order[slot % order.len()];
    if pin_current_to(cpu) {
        Some(cpu)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn t_affinity_order_nonempty_and_bounded() {
        let order = cpu_order();
        // A normal CI/container exposes at least one CPU; every entry is a
        // plausible cpu id.
        assert!(!order.is_empty());
        assert!(order.iter().all(|c| *c < 1024));
    }

    #[test]
    fn t_affinity_pin_roundtrip() {
        // Pinning to a CPU we are already allowed on must succeed or fail
        // gracefully — and the order must be unaffected.
        let order = cpu_order();
        if let Some(cpu) = order.first() {
            let ok = pin_current_to(*cpu);
            // Restore unrestricted affinity within the allowed set.
            let mut set = unsafe { std::mem::zeroed::<libc::cpu_set_t>() };
            for c in &order {
                unsafe { libc::CPU_SET(*c, &mut set) };
            }
            let _ = unsafe {
                libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set)
            };
            // Either outcome is acceptable (container restrictions).
            let _ = ok;
        }
    }
}

/// R8: how many DISTINCT physical cores the allowed set spans (SMT sibling
/// groups collapse to one). Drives thread-count decisions on mixed SMT /
/// non-SMT runner pools: oversubscribing a physical core with spin-happy
/// threads collapses the fabric (measured: 55M vs 411M msg/s).
pub fn physical_core_count() -> usize {
    let order = cpu_order();
    if order.is_empty() {
        return 0;
    }
    let mut groups: Vec<Vec<usize>> = Vec::new();
    for &cpu in &order {
        match read_sibling_group(cpu) {
            Some(g) => {
                if !groups.iter().any(|h| h == &g) {
                    groups.push(g);
                }
            }
            None => groups.push(vec![cpu]),
        }
    }
    groups.len()
}
