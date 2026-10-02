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
    // Group by identical sibling lists; a true SMT pair (2 members) is one
    // physical core, a singleton is one, and larger groups (some VMs report
    // all vCPUs sharing one thread_siblings_list) count as their size — the
    // oversubscription guard only needs the right ORDER of magnitude.
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
    groups
        .iter()
        .map(|g| match g.len() {
            0 => 0,
            2 => 1,
            n => n,
        })
        .sum()
}

/// R8: the L3 (last-level cache) sharing groups of the allowed cpus, in
/// topology order — `l3_groups()[g]` is the list of allowed cpus whose
/// index-3 cache is shared. Cross-L3 thread placement puts the pipeline's
/// per-batch handoffs (14KB+) on a cross-CCD path (~120-200ns latency on
/// multi-CCD server parts) instead of a shared L3 — measured as a 2.5x
/// consumer-side regression on one runner type vs its same-generation
/// sibling whose placement happened to be L3-local.
pub fn l3_groups() -> Vec<Vec<usize>> {
    let order = cpu_order();
    if order.is_empty() {
        return Vec::new();
    }
    let mut groups: Vec<(Vec<usize>, Vec<usize>)> = Vec::new(); // (members, allowed members)
    for &cpu in &order {
        let shared = read_l3_shared(cpu).unwrap_or_else(|| vec![cpu]);
        let key: Vec<usize> = {
            let mut k = shared.clone();
            k.sort_unstable();
            k.dedup();
            k
        };
        match groups.iter_mut().find(|(k, _)| *k == key) {
            Some((_, members)) => members.push(cpu),
            None => groups.push((key, vec![cpu])),
        }
    }
    groups.into_iter().map(|(_, m)| m).collect()
}

/// The index-3 cache sharing list of `cpu` (its L3 siblings), parsed from
/// sysfs. None if unavailable (singletons are fine — the cpu is its own
/// group).
fn read_l3_shared(cpu: usize) -> Option<Vec<usize>> {
    let path = format!("/sys/devices/system/cpu/cpu{cpu}/cache/index3/shared_cpu_list");
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
    (!group.is_empty()).then_some(group)
}

/// R8: placement for a pipelined arm — (main, rx) cpus that share an L3
/// but not a physical core when possible (the mailbox handoff is
/// latency-sensitive; SMT-sharing it with the busy main would timeslice
/// execution). Returns (main_cpu, rx_cpu).
pub fn pipeline_placement() -> (Option<usize>, Option<usize>) {
    let groups = l3_groups();
    // Prefer the L3 group with the most DISTINCT physical cores.
    let mut best: Option<&Vec<usize>> = None;
    let mut best_phys = 0usize;
    for g in &groups {
        let mut reps: Vec<usize> = Vec::new();
        for c in g {
            let is_rep = match read_sibling_group(*c) {
                Some(s) => s.iter().min() == Some(c),
                None => true,
            };
            if is_rep {
                reps.push(*c);
            }
        }
        if reps.len() > best_phys {
            best_phys = reps.len();
            best = Some(g);
        }
    }
    let group = match best {
        Some(g) if g.len() >= 2 => g,
        _ => {
            let order = cpu_order();
            if order.len() >= 2 {
                return (order.first().copied(), order.get(1).copied());
            }
            return (order.first().copied(), None);
        }
    };
    // main = first distinct-physical representative; rx = the next cpu NOT
    // in main's SMT pair.
    let main = group[0];
    let main_sibs = read_sibling_group(main).unwrap_or_else(|| vec![main]);
    let rx = group
        .iter()
        .find(|c| **c != main && !main_sibs.contains(c))
        .copied()
        .or(Some(group[1.min(group.len() - 1)]));
    (Some(main), rx)
}

#[cfg(test)]
mod l3_tests {
    use super::*;

    #[test]
    fn t_l3_groups_partition() {
        let groups = l3_groups();
        let order = cpu_order();
        let total: usize = groups.iter().map(|g| g.len()).sum();
        if !order.is_empty() {
            assert_eq!(total, order.len(), "L3 groups must partition the allowed set");
        }
    }

    #[test]
    fn t_pipeline_placement_distinct() {
        let (main, rx) = pipeline_placement();
        if let (Some(m), Some(r)) = (main, rx) {
            assert_ne!(m, r, "main and rx must be distinct cpus");
        }
    }
}

/// R8: FABRIC placement for SMT hosts — (main, rx, worker cpus). Main and
/// the RX thread are placed as SMT SIBLINGS on one physical core (the RX
/// is ~15% busy at target rates — the heavy main thread keeps ~85-90% of
/// the core under SMT, and the per-batch mailbox handoff becomes
/// L1/L2-local); the workers get the REMAINING physical cores whole (a
/// worker on main's hyperthread measured a degenerative feedback spiral:
/// the starved worker's idle-spin stole issue slots from the critical main
/// thread, collapsing the sustained arm to 14M msg/s).
///
/// Non-SMT hosts: main and rx take distinct cores (the span-arm
/// placement), workers the rest.
pub fn fabric_placement(workers: usize) -> (Option<usize>, Option<usize>, Vec<usize>) {
    let order = cpu_order();
    if order.is_empty() {
        return (None, None, Vec::new());
    }
    // Physical cores (representative + its sibling(s)) in topology order.
    let mut cores: Vec<Vec<usize>> = Vec::new();
    for &c in &order {
        let sibs = read_sibling_group(c).unwrap_or_else(|| vec![c]);
        let key = *sibs.iter().min().unwrap_or(&c);
        if !cores.iter().any(|k| *k.first().unwrap_or(&usize::MAX) == key) {
            let mut grp = sibs;
            grp.sort_unstable();
            cores.push(grp);
        }
    }
    if cores.len() >= 2 {
        // Core 0: main + RX (SMT siblings if present).
        let main = cores[0].first().copied();
        let rx = cores[0].get(1).copied().or_else(|| cores[1].first().copied());
        // Workers: the remaining cores' cpus, whole cores first.
        let mut pool: Vec<usize> = Vec::new();
        for core in cores.iter().skip(1) {
            pool.extend(core.iter().copied());
        }
        if pool.is_empty() {
            pool = order.clone();
        }
        let wcpus: Vec<usize> = (0..workers)
            .map(|i| pool[i % pool.len()])
            .collect();
        (main, rx, wcpus)
    } else {
        // Single physical core: everything wraps.
        let main = order.first().copied();
        let rx = order.get(1).copied();
        let wcpus: Vec<usize> = (0..workers)
            .map(|i| order[i % order.len()])
            .collect();
        (main, rx, wcpus)
    }
}

#[cfg(test)]
mod fabric_placement_tests {
    use super::*;

    #[test]
    fn t_fabric_placement_no_worker_on_main_sibling_when_possible() {
        let (main, rx, workers) = fabric_placement(2);
        if let (Some(m), Some(r)) = (main, rx) {
            assert_ne!(m, r);
        }
        // On multi-core hosts, workers must not land on main's cpu.
        if let Some(m) = main {
            for w in &workers {
                if crate::affinity::cpu_order().len() > 2 {
                    assert_ne!(*w, m, "worker pinned on main's cpu");
                }
            }
        }
    }
}

/// R8: SMT-polite spin — bounded PAUSE backoff, then `sched_yield()`.
/// Zen3's PAUSE hint is weak: an uncapped pause-loop on one hyperthread
/// measurably starves its sibling (the fabric's workers collapsed to ~1%
/// of their CRC ceiling through mutual spin-starvation on a shared
/// physical core). The yield deschedules the logical cpu for a
/// reschedule quantum (~1-3us), keeping the sibling fed; wake latency for
/// the yielder is immediate (spinners re-check after the yield).
pub fn polite_spin(iters: &mut u32) {
    if *iters < 6 {
        let n = 1u32 << *iters;
        for _ in 0..n {
            std::hint::spin_loop();
        }
    } else {
        std::thread::yield_now();
    }
    *iters += 1;
}
