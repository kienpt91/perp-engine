//! Use case: run a deterministic simulated trading session through the
//! durable [`ExchangeService`], so every event is logged before it is applied.

use crate::ports::{Clock, EventLog, SnapshotStore};
use crate::service::{ExchangeService, ServiceError};
use perp_domain::error::Error;
use perp_domain::sha256::Digest;
use perp_domain::traffic::{TrafficConfig, TrafficGenerator};
use perp_domain::types::Output;
use std::collections::BTreeMap;
use std::fmt;

/// Reads resource usage of the running process (a port: the domain and
/// application never touch `/proc` themselves).
pub trait ResourceProbe {
    /// Resident memory in bytes, if the platform can tell.
    fn resident_bytes(&self) -> Option<u64>;
}

/// A probe that reports nothing.
pub struct NoProbe;

impl ResourceProbe for NoProbe {
    fn resident_bytes(&self) -> Option<u64> {
        None
    }
}

/// Parameters of a simulation run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimulationConfig {
    /// Population and seed.
    pub traffic: TrafficConfig,
    /// Generated events after the bootstrap events.
    pub events: u64,
    /// Record a [`Checkpoint`] every this many events (0 = never).
    pub checkpoint_every: u64,
    /// Run the full invariant check every this many events (0 = never).
    pub check_invariants_every: u64,
}

/// A periodic sample of size indicators.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkpoint {
    /// Generated events so far.
    pub events: u64,
    /// Resident memory, when a probe provided it.
    pub resident_bytes: Option<u64>,
    /// Resting orders over all books.
    pub open_orders: u64,
    /// Accounts holding a position.
    pub accounts_with_positions: u32,
}

/// What a run did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimulationReport {
    /// Bootstrap events applied.
    pub bootstrap_events: u64,
    /// Generated events applied or rejected.
    pub events: u64,
    /// Events the engine accepted.
    pub accepted: u64,
    /// Events the engine rejected, by error code.
    pub rejected: BTreeMap<&'static str, u64>,
    /// Fills.
    pub fills: u64,
    /// Liquidation steps.
    pub liquidation_steps: u64,
    /// Auto-deleveraging fills.
    pub adl_fills: u64,
    /// Funding settlements.
    pub funding_settlements: u64,
    /// Orders cancelled by self-trade prevention.
    pub self_trade_cancels: u64,
    /// Orders that rested on a book.
    pub rested: u64,
    /// Insurance fund at the end.
    pub insurance_fund: i128,
    /// Fee pool at the end.
    pub fee_pool: i128,
    /// Uncovered losses at the end.
    pub bad_debt: i128,
    /// Final state hash.
    pub state_hash: Digest,
    /// Periodic samples.
    pub checkpoints: Vec<Checkpoint>,
}

impl SimulationReport {
    /// Total rejected events.
    pub fn rejected_total(&self) -> u64 {
        self.rejected.values().sum()
    }
}

/// Why a run stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SimulationError {
    /// The service failed (storage error or halted engine).
    Service(ServiceError),
    /// The engine already holds state; a simulation must start from an empty log.
    NotEmpty,
    /// An invariant check failed.
    InvariantViolated(String),
}

impl fmt::Display for SimulationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SimulationError::Service(e) => write!(f, "{e}"),
            SimulationError::NotEmpty => write!(f, "the log already contains events"),
            SimulationError::InvariantViolated(s) => write!(f, "invariant violated: {s}"),
        }
    }
}

impl std::error::Error for SimulationError {}

impl From<ServiceError> for SimulationError {
    fn from(e: ServiceError) -> Self {
        SimulationError::Service(e)
    }
}

/// Run a simulation on a fresh service. Business rejections are counted, not
/// fatal; storage failures and a halted engine stop the run.
pub fn run_simulation<L: EventLog, S: SnapshotStore, C: Clock>(
    service: &mut ExchangeService<L, S, C>,
    cfg: &SimulationConfig,
    probe: &dyn ResourceProbe,
) -> Result<SimulationReport, SimulationError> {
    if service.log_len() != 0 || service.engine().config().is_some() {
        return Err(SimulationError::NotEmpty);
    }
    let mut gen = TrafficGenerator::new(cfg.traffic.clone());
    let bootstrap = gen.bootstrap();
    for ev in &bootstrap {
        service.apply_event(ev)?;
    }
    let mut report = SimulationReport {
        bootstrap_events: bootstrap.len() as u64,
        events: 0,
        accepted: 0,
        rejected: BTreeMap::new(),
        fills: 0,
        liquidation_steps: 0,
        adl_fills: 0,
        funding_settlements: 0,
        self_trade_cancels: 0,
        rested: 0,
        insurance_fund: 0,
        fee_pool: 0,
        bad_debt: 0,
        state_hash: [0; 32],
        checkpoints: Vec::new(),
    };
    for i in 1..=cfg.events {
        let ev = gen.next_event(service.engine());
        match service.apply_event(&ev) {
            Ok(outs) => {
                report.accepted += 1;
                for o in &outs {
                    match o {
                        Output::FundingSettled { .. } => report.funding_settlements += 1,
                        Output::Rested { .. } => report.rested += 1,
                        Output::Canceled { reason, .. }
                            if *reason == perp_domain::types::CancelReason::SelfTrade =>
                        {
                            report.self_trade_cancels += 1;
                        }
                        _ => {}
                    }
                }
            }
            Err(ServiceError::Engine(Error::Halted)) => {
                return Err(SimulationError::Service(ServiceError::Engine(
                    Error::Halted,
                )));
            }
            Err(ServiceError::Engine(e)) => {
                *report.rejected.entry(e.code()).or_insert(0) += 1;
            }
            Err(e) => return Err(e.into()),
        }
        report.events = i;
        if service.engine().is_halted() {
            return Err(SimulationError::Service(ServiceError::Engine(
                Error::Halted,
            )));
        }
        if cfg.check_invariants_every > 0 && i % cfg.check_invariants_every == 0 {
            service
                .engine()
                .check_invariants()
                .map_err(SimulationError::InvariantViolated)?;
        }
        if cfg.checkpoint_every > 0 && i % cfg.checkpoint_every == 0 {
            let s = service.engine().summary();
            report.checkpoints.push(Checkpoint {
                events: i,
                resident_bytes: probe.resident_bytes(),
                open_orders: s.open_orders,
                accounts_with_positions: s.accounts_with_positions,
            });
        }
    }
    service.sync()?;
    let s = service.engine().summary();
    report.fills = s.fills;
    report.liquidation_steps = s.liquidation_steps;
    report.adl_fills = s.adl_fills;
    report.insurance_fund = s.insurance_fund.0;
    report.fee_pool = s.fee_pool.0;
    report.bad_debt = s.bad_debt.0;
    report.state_hash = service.engine().state_hash();
    Ok(report)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::fakes::{InMemoryEventLog, InMemorySnapshotStore, ManualClock};
    use crate::service::{replay_log, ServiceConfig};
    use std::cell::Cell;

    type Svc = ExchangeService<InMemoryEventLog, InMemorySnapshotStore, ManualClock>;

    fn service(log: InMemoryEventLog, snapshot_every: u64) -> Svc {
        let cfg = ServiceConfig {
            snapshot_every,
            keep_snapshots: 2,
            ..ServiceConfig::default()
        };
        ExchangeService::open(log, InMemorySnapshotStore::new(), ManualClock::new(0), cfg)
            .unwrap()
            .0
    }

    fn cfg(seed: u64, events: u64) -> SimulationConfig {
        SimulationConfig {
            traffic: TrafficConfig {
                seed,
                accounts: 40,
                ..TrafficConfig::default()
            },
            events,
            checkpoint_every: 0,
            check_invariants_every: 0,
        }
    }

    #[test]
    fn the_same_seed_produces_the_same_report_and_log() {
        let (la, lb) = (InMemoryEventLog::new(), InMemoryEventLog::new());
        let ra = run_simulation(&mut service(la.clone(), 0), &cfg(7, 3_000), &NoProbe).unwrap();
        let rb = run_simulation(&mut service(lb.clone(), 0), &cfg(7, 3_000), &NoProbe).unwrap();
        assert_eq!(ra, rb);
        assert_eq!(la.events(), lb.events());
    }

    #[test]
    fn every_generated_event_is_accepted_or_rejected_and_only_prechecked_ones_skip_the_log() {
        let log = InMemoryEventLog::new();
        let mut svc = service(log.clone(), 0);
        let r = run_simulation(&mut svc, &cfg(1, 2_000), &NoProbe).unwrap();
        assert_eq!(r.events, 2_000);
        assert_eq!(r.accepted + r.rejected_total(), 2_000);
        // Events the pre-check refuses are neither logged nor applied; the
        // log holds exactly what the engine counted.
        let logged = log.events().len() as u64;
        assert_eq!(logged, svc.engine().events_applied());
        assert!(logged >= r.bootstrap_events + r.accepted);
        assert!(logged < r.bootstrap_events + 2_000);
        assert!(
            r.rejected_total() > 0,
            "the generator emits some invalid events"
        );
        assert!(r.fills > 0 && r.rested > 0);
    }

    #[test]
    fn the_logged_run_replays_to_the_reported_state_hash() {
        let log = InMemoryEventLog::new();
        let mut svc = service(log.clone(), 500);
        let r = run_simulation(&mut svc, &cfg(3, 4_000), &NoProbe).unwrap();
        let mut again = log;
        let (engine, report) = replay_log(&mut again).unwrap();
        assert_eq!(engine.state_hash(), r.state_hash);
        assert_eq!(report.records, svc.engine().events_applied());
        assert!(report.records <= r.bootstrap_events + 4_000);
    }

    #[test]
    fn checkpoints_are_sampled_with_the_probe() {
        struct Fixed(Cell<u64>);
        impl ResourceProbe for Fixed {
            fn resident_bytes(&self) -> Option<u64> {
                self.0.set(self.0.get() + 1);
                Some(self.0.get())
            }
        }
        let mut c = cfg(2, 1_000);
        c.checkpoint_every = 250;
        let probe = Fixed(Cell::new(0));
        let r = run_simulation(&mut service(InMemoryEventLog::new(), 0), &c, &probe).unwrap();
        let events: Vec<u64> = r.checkpoints.iter().map(|c| c.events).collect();
        assert_eq!(events, vec![250, 500, 750, 1_000]);
        assert_eq!(
            r.checkpoints
                .iter()
                .map(|c| c.resident_bytes)
                .collect::<Vec<_>>(),
            vec![Some(1), Some(2), Some(3), Some(4)]
        );
    }

    #[test]
    fn no_probe_reports_nothing() {
        assert_eq!(NoProbe.resident_bytes(), None);
        let mut c = cfg(2, 100);
        c.checkpoint_every = 50;
        let r = run_simulation(&mut service(InMemoryEventLog::new(), 0), &c, &NoProbe).unwrap();
        assert!(r.checkpoints.iter().all(|c| c.resident_bytes.is_none()));
    }

    #[test]
    fn invariant_checks_pass_on_a_healthy_run() {
        let mut c = cfg(5, 5_000);
        c.check_invariants_every = 500;
        assert!(run_simulation(&mut service(InMemoryEventLog::new(), 0), &c, &NoProbe).is_ok());
    }

    #[test]
    fn a_used_log_is_refused() {
        let log = InMemoryEventLog::new();
        let mut svc = service(log, 0);
        run_simulation(&mut svc, &cfg(1, 10), &NoProbe).unwrap();
        assert_eq!(
            run_simulation(&mut svc, &cfg(1, 10), &NoProbe),
            Err(SimulationError::NotEmpty)
        );
    }

    #[test]
    fn storage_failure_stops_the_run_with_a_service_error() {
        let log = InMemoryEventLog::with_capacity_limit(200);
        let err = run_simulation(&mut service(log, 0), &cfg(1, 1_000), &NoProbe).unwrap_err();
        assert!(
            matches!(err, SimulationError::Service(ServiceError::ReadOnly(_))),
            "{err:?}"
        );
    }

    #[test]
    fn error_messages_are_readable() {
        assert_eq!(
            SimulationError::NotEmpty.to_string(),
            "the log already contains events"
        );
        assert!(SimulationError::InvariantViolated("x".into())
            .to_string()
            .contains("x"));
        let e: SimulationError = ServiceError::Engine(Error::Halted).into();
        assert_eq!(e.to_string(), "halted");
    }

    #[test]
    fn long_runs_trigger_liquidations_and_funding() {
        let c = SimulationConfig {
            traffic: TrafficConfig {
                seed: 1,
                accounts: 200,
                ..TrafficConfig::default()
            },
            events: 100_000,
            checkpoint_every: 0,
            check_invariants_every: 10_000,
        };
        let r = run_simulation(&mut service(InMemoryEventLog::new(), 0), &c, &NoProbe).unwrap();
        assert!(r.liquidation_steps > 0, "{r:?}");
        assert!(r.funding_settlements > 0, "{r:?}");
        assert_eq!(r.bad_debt, 0);
    }
}
