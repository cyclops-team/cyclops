//! Measurements, not correctness: benchmarks, performance counters, the
//! cold-start replay, and the vendor soak harness. Every test here is
//! `#[ignore]`; the scheduled and release lanes run them with `--ignored`
//! (`scripts/ci-performance.py`, `scripts/ci-reliability.sh`) and keep
//! their JSON artifacts. `scripts/check.sh` does not build this binary, the
//! same way the nextest filter skips the client crates' performance
//! binaries: a number is evidence for a trend line, not a gate a pull
//! request can fail.

// The shared rig lives beside the group directories, not inside this one.
#[path = "../common/mod.rs"]
mod common;

mod cold_start_replay_perf;
mod communication_benchmark;
mod concurrent_messaging_perf;
mod idle_observation_perf;
mod release_transport_benchmark;
mod stage_and_clear_soak;
