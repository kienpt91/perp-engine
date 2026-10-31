//! Command implementations.

use crate::args::Args;
use crate::render;
use crate::CliError;
use perp_app::fakes::{InMemoryEventLog, InMemorySnapshotStore, ManualClock};
use perp_app::ports::{EventLog, SnapshotStore};
use perp_app::service::{replay_log, ExchangeService, ServiceConfig};
use perp_app::simulation::{
    run_simulation, NoProbe, ResourceProbe, SimulationConfig, SimulationReport,
};
use perp_domain::engine::Engine;
use perp_domain::fixed::Money;
use perp_domain::traffic::{TrafficConfig, TrafficGenerator};
use perp_domain::types::{EngineConfig, EventBody, MarketId};
use perp_infra::clock::SystemClock;
use perp_infra::probe::ProcStatusProbe;
use perp_infra::server::{Server, ServerConfig};
use perp_infra::snapshot::FileSnapshotStore;
use perp_infra::token;
use perp_infra::wal::{self, FileEventLog, FsyncPolicy, ReadOnlyFileLog, WalConfig};
use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

type CliResult = Result<(), CliError>;

fn fail<E: std::fmt::Display>(what: &str) -> impl Fn(E) -> CliError + '_ {
    move |e| CliError::Failure(format!("{what}: {e}"))
}

fn usage(msg: impl Into<String>) -> CliError {
    CliError::Usage(msg.into())
}

pub(crate) fn dispatch(args: &Args, out: &mut dyn Write, err: &mut dyn Write) -> CliResult {
    let cmd = args.command.as_deref().unwrap_or("help");
    match cmd {
        "help" => {
            let _ = out.write_all(crate::USAGE.as_bytes());
            Ok(())
        }
        "simulate" => simulate(args, out),
        "replay" => replay(args, out),
        "verify" => verify(args, out),
        "book" => book(args, out),
        "positions" => positions(args, out),
        "serve" => serve(args, err),
        "gen-token" => gen_token(args),
        other => Err(usage(format!("unknown command '{other}'"))),
    }
}

fn one_path(args: &Args) -> Result<PathBuf, CliError> {
    match args.positional.as_slice() {
        [p] => Ok(PathBuf::from(p)),
        _ => Err(usage("expected exactly one log path")),
    }
}

fn parse_fsync(args: &Args) -> Result<FsyncPolicy, CliError> {
    match args.value("fsync").unwrap_or("always") {
        "always" => Ok(FsyncPolicy::Always),
        "never" => Ok(FsyncPolicy::Never),
        other => match other.strip_prefix("every:").map(str::parse::<u32>) {
            Some(Ok(n)) if n > 0 => Ok(FsyncPolicy::EveryN(n)),
            _ => Err(usage("--fsync must be always, never or every:N with N > 0")),
        },
    }
}

fn narrow<T: TryFrom<u64>>(v: u64, what: &str) -> Result<T, CliError> {
    T::try_from(v).map_err(|_| usage(format!("--{what} is out of range")))
}

// ---------------------------------------------------------------------------

fn simulate(args: &Args, out: &mut dyn Write) -> CliResult {
    args.only(&[
        "seed",
        "events",
        "accounts",
        "markets",
        "dir",
        "fsync",
        "snapshot-every",
        "checkpoints",
        "check-every",
    ])
    .map_err(usage)?;
    if !args.positional.is_empty() {
        return Err(usage("simulate takes no positional arguments"));
    }
    let accounts: u32 = narrow(args.number("accounts", 200).map_err(usage)?, "accounts")?;
    let markets: u32 = narrow(args.number("markets", 1).map_err(usage)?, "markets")?;
    if accounts == 0 || accounts > 100_000 || markets == 0 || markets > 8 {
        return Err(usage("--accounts must be 1..=100000 and --markets 1..=8"));
    }
    let events = args.number("events", 200_000).map_err(usage)?;
    let cfg = SimulationConfig {
        traffic: TrafficConfig {
            seed: args.number("seed", 1).map_err(usage)?,
            accounts,
            markets,
            ..TrafficConfig::default()
        },
        events,
        checkpoint_every: args.number("checkpoints", 0).map_err(usage)?,
        check_invariants_every: args.number("check-every", 0).map_err(usage)?,
    };
    let service_cfg = ServiceConfig {
        snapshot_every: args.number("snapshot-every", 0).map_err(usage)?,
        keep_snapshots: 3,
    };
    let fsync = parse_fsync(args)?;
    let probe = ProcStatusProbe;
    let report = if let Some(dir) = args.value("dir") {
        let dir = PathBuf::from(dir);
        std::fs::create_dir_all(&dir).map_err(fail("create --dir"))?;
        let log = FileEventLog::open(
            dir.join("engine.wal"),
            WalConfig {
                fsync,
                ..WalConfig::default()
            },
        )
        .map_err(fail("open log"))?;
        let snaps = FileSnapshotStore::new(dir.join("snapshots"));
        writeln!(out, "log: {}", dir.join("engine.wal").display()).map_err(fail("write"))?;
        simulate_with(log, snaps, &cfg, service_cfg, &probe)?
    } else {
        writeln!(out, "log: in memory").map_err(fail("write"))?;
        simulate_with(
            InMemoryEventLog::new(),
            InMemorySnapshotStore::new(),
            &cfg,
            service_cfg,
            &NoProbe,
        )?
    };
    print_report(out, &cfg, &report).map_err(fail("write"))
}

fn simulate_with<L: EventLog, S: SnapshotStore>(
    log: L,
    snaps: S,
    cfg: &SimulationConfig,
    service_cfg: ServiceConfig,
    probe: &dyn ResourceProbe,
) -> Result<SimulationReport, CliError> {
    let (mut svc, _) = ExchangeService::open(log, snaps, ManualClock::new(0), service_cfg)
        .map_err(fail("open service"))?;
    run_simulation(&mut svc, cfg, probe).map_err(fail("simulation"))
}

fn print_report(
    out: &mut dyn Write,
    cfg: &SimulationConfig,
    r: &SimulationReport,
) -> std::io::Result<()> {
    writeln!(
        out,
        "simulation: seed {} accounts {} markets {} events {}",
        cfg.traffic.seed, cfg.traffic.accounts, cfg.traffic.markets, cfg.events
    )?;
    writeln!(out, "bootstrap events      {}", r.bootstrap_events)?;
    writeln!(
        out,
        "generated events      {} (accepted {}, rejected {})",
        r.events,
        r.accepted,
        r.rejected_total()
    )?;
    if !r.rejected.is_empty() {
        let list: Vec<String> = r.rejected.iter().map(|(k, v)| format!("{k}={v}")).collect();
        writeln!(out, "rejections            {}", list.join(" "))?;
    }
    writeln!(out, "orders rested         {}", r.rested)?;
    writeln!(out, "fills                 {}", r.fills)?;
    writeln!(out, "self-trade cancels    {}", r.self_trade_cancels)?;
    writeln!(out, "funding settlements   {}", r.funding_settlements)?;
    writeln!(out, "liquidation steps     {}", r.liquidation_steps)?;
    writeln!(out, "auto-deleveraging     {}", r.adl_fills)?;
    writeln!(
        out,
        "insurance fund        {}",
        render::money(r.insurance_fund)
    )?;
    writeln!(out, "fee pool              {}", render::money(r.fee_pool))?;
    writeln!(out, "bad debt              {}", render::money(r.bad_debt))?;
    for c in &r.checkpoints {
        let rss = c
            .resident_bytes
            .map_or_else(|| "n/a".to_string(), |b| format!("{} KiB", b / 1024));
        writeln!(
            out,
            "checkpoint {:>9} events: resting orders {:>6}, accounts with positions {:>4}, rss {}",
            c.events, c.open_orders, c.accounts_with_positions, rss
        )?;
    }
    writeln!(
        out,
        "state hash            {}",
        render::short_hash(&r.state_hash)
    )
}

// ---------------------------------------------------------------------------

fn replay_engine(path: &Path) -> Result<(Engine, perp_app::service::ReplayReport), CliError> {
    let mut log = ReadOnlyFileLog::open(path).map_err(fail("open log"))?;
    replay_log(&mut log).map_err(fail("replay"))
}

fn replay(args: &Args, out: &mut dyn Write) -> CliResult {
    args.only(&[]).map_err(usage)?;
    let path = one_path(args)?;
    let (engine, report) = replay_engine(&path)?;
    let s = engine.summary();
    let w = |e| CliError::Failure(format!("write: {e}"));
    writeln!(out, "records replayed      {}", report.records).map_err(w)?;
    writeln!(out, "torn tail bytes       {}", report.repaired_bytes).map_err(w)?;
    writeln!(out, "accounts              {}", s.accounts).map_err(w)?;
    writeln!(out, "markets               {}", s.markets).map_err(w)?;
    writeln!(out, "resting orders        {}", s.open_orders).map_err(w)?;
    writeln!(out, "fills                 {}", s.fills).map_err(w)?;
    writeln!(out, "liquidation steps     {}", s.liquidation_steps).map_err(w)?;
    writeln!(out, "auto-deleveraging     {}", s.adl_fills).map_err(w)?;
    writeln!(
        out,
        "total collateral      {}",
        render::money(s.total_collateral.0)
    )
    .map_err(w)?;
    writeln!(
        out,
        "insurance fund        {}",
        render::money(s.insurance_fund.0)
    )
    .map_err(w)?;
    writeln!(out, "fee pool              {}", render::money(s.fee_pool.0)).map_err(w)?;
    writeln!(out, "bad debt              {}", render::money(s.bad_debt.0)).map_err(w)?;
    writeln!(out, "halted                {}", s.halted).map_err(w)?;
    writeln!(
        out,
        "state hash            {}",
        render::short_hash(&engine.state_hash())
    )
    .map_err(w)
}

fn verify(args: &Args, out: &mut dyn Write) -> CliResult {
    args.only(&["strict"]).map_err(usage)?;
    let path = one_path(args)?;
    let w = |e| CliError::Failure(format!("write: {e}"));
    let report = wal::verify(&path).map_err(fail("hash chain verification"))?;
    writeln!(out, "records               {}", report.records).map_err(w)?;
    writeln!(out, "file length           {} bytes", report.file_len).map_err(w)?;
    writeln!(
        out,
        "chain head            {}",
        render::short_hash(&report.chain_head)
    )
    .map_err(w)?;
    writeln!(out, "torn tail             {} bytes", report.torn_bytes).map_err(w)?;
    if report.torn_bytes > 0 && args.has("strict") {
        return Err(CliError::Failure(
            "the log ends with a torn record (it is truncated when opened for append)".into(),
        ));
    }
    let (engine, _) = replay_engine(&path)?;
    engine
        .check_invariants()
        .map_err(|e| CliError::Failure(format!("invariant violated: {e}")))?;
    if engine.is_halted() {
        return Err(CliError::Failure(format!(
            "engine halted during replay: {}",
            engine.halt_reason().unwrap_or("unknown")
        )));
    }
    writeln!(
        out,
        "state hash            {}",
        render::short_hash(&engine.state_hash())
    )
    .map_err(w)?;
    writeln!(
        out,
        "OK: chain valid, replay deterministic, invariants hold"
    )
    .map_err(w)
}

fn book(args: &Args, out: &mut dyn Write) -> CliResult {
    args.only(&["market", "depth"]).map_err(usage)?;
    let path = one_path(args)?;
    let depth: usize = narrow(args.number("depth", 10).map_err(usage)?, "depth")?;
    let (engine, _) = replay_engine(&path)?;
    let markets: Vec<MarketId> = match args.value("market") {
        Some(_) => vec![narrow(args.number("market", 0).map_err(usage)?, "market")?],
        None => engine.market_ids(),
    };
    for m in markets {
        let view = engine
            .book_view(m, depth)
            .ok_or_else(|| CliError::Failure(format!("unknown market {m}")))?;
        write!(out, "{}", render::book(&view)).map_err(fail("write"))?;
    }
    Ok(())
}

fn positions(args: &Args, out: &mut dyn Write) -> CliResult {
    args.only(&["account"]).map_err(usage)?;
    let path = one_path(args)?;
    let (engine, _) = replay_engine(&path)?;
    let ids = match args.value("account") {
        Some(_) => vec![args.number("account", 0).map_err(usage)?],
        None => engine.account_ids(),
    };
    for id in ids {
        let view = engine
            .account_view(id)
            .ok_or_else(|| CliError::Failure(format!("unknown account {id}")))?;
        if args.value("account").is_none() && view.positions.is_empty() {
            continue;
        }
        write!(out, "{}", render::account(&view)).map_err(fail("write"))?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------

fn gen_token(args: &Args) -> CliResult {
    args.only(&["out"]).map_err(usage)?;
    if !args.positional.is_empty() {
        return Err(usage("gen-token takes no positional arguments"));
    }
    let out = args.value("out").map(PathBuf::from);
    token::cmd_gen_token(out.as_deref()).map_err(fail("gen-token"))
}

fn serve(args: &Args, err: &mut dyn Write) -> CliResult {
    args.only(&[
        "dir",
        "listen",
        "token-env",
        "token-file",
        "allow-non-loopback",
        "init",
        "markets",
        "fsync",
        "snapshot-every",
    ])
    .map_err(usage)?;
    if !args.positional.is_empty() {
        return Err(usage("serve takes no positional arguments"));
    }
    let dir = PathBuf::from(
        args.value("dir")
            .ok_or_else(|| usage("serve needs --dir"))?,
    );
    let listen: SocketAddr = args
        .value("listen")
        .unwrap_or("127.0.0.1:7878")
        .parse()
        .map_err(|_| usage("--listen must be ip:port"))?;
    if args.has("token-env") && args.has("token-file") {
        return Err(usage("use either --token-env or --token-file"));
    }
    let token = match args.value("token-file") {
        Some(p) => token::load_from_file(Path::new(p)),
        None => token::load_from_env(args.value("token-env").unwrap_or("PERP_ENGINE_TOKEN")),
    }
    .map_err(fail("auth token"))?;
    let markets: u32 = narrow(args.number("markets", 1).map_err(usage)?, "markets")?;
    if markets == 0 || markets > 8 {
        return Err(usage("--markets must be 1..=8"));
    }
    let fsync = parse_fsync(args)?;
    let service_cfg = ServiceConfig {
        snapshot_every: args.number("snapshot-every", 10_000).map_err(usage)?,
        keep_snapshots: 3,
    };

    std::fs::create_dir_all(&dir).map_err(fail("create --dir"))?;
    let log = FileEventLog::open(
        dir.join("engine.wal"),
        WalConfig {
            fsync,
            ..WalConfig::default()
        },
    )
    .map_err(fail("open log"))?;
    let snaps = FileSnapshotStore::new(dir.join("snapshots"));
    let (mut svc, rec) =
        ExchangeService::open(log, snaps, SystemClock, service_cfg).map_err(fail("recover"))?;
    let _ = writeln!(
        err,
        "recovered: snapshot {:?}, {} log records, {} replayed, {} torn bytes repaired",
        rec.snapshot_seq, rec.log_records, rec.replayed, rec.repaired_bytes
    );
    if svc.engine().config().is_none() {
        if !args.has("init") {
            return Err(CliError::Failure(
                "the log is empty; start once with --init to configure the engine and markets"
                    .into(),
            ));
        }
        svc.submit(EventBody::Configure(EngineConfig::default()))
            .map_err(fail("configure"))?;
        for m in 1..=markets {
            svc.submit(EventBody::AddMarket(TrafficGenerator::market_config(m)))
                .map_err(fail("add market"))?;
        }
        svc.submit(EventBody::FundInsurance {
            amount: Money(1_000_000_000),
        })
        .map_err(fail("fund insurance"))?;
    }

    let mut server_cfg = ServerConfig::new(token);
    server_cfg.listen = listen;
    server_cfg.allow_non_loopback = args.has("allow-non-loopback");
    let server = Server::bind(server_cfg).map_err(fail("bind"))?;
    let addr = server.local_addr().map_err(fail("local address"))?;
    let _ = writeln!(
        err,
        "listening on {addr} (plaintext JSON lines; send {{\"op\":\"auth\",\"token\":...}} first)"
    );
    if args.has("allow-non-loopback") && !addr.ip().is_loopback() {
        let _ = writeln!(
            err,
            "warning: listening on a non-loopback address without TLS"
        );
    }
    let shutdown = Arc::new(AtomicBool::new(false));
    let stats = server.run(svc, shutdown).map_err(fail("server"))?;
    let _ = writeln!(err, "server stopped: {stats:?}");
    Ok(())
}
