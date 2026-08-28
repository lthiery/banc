//! `banc` — rig utilities for the banc HIL framework.
//!
//! Everything here is config-driven: the rig described by `banc-rig.toml`
//! (located via `BANC_RIG` or upward search, exactly as suites locate it) is
//! the single source of truth, so what doctor checks is what a suite will
//! use.

use banc_host::config::RigConfig;
use banc_host::rig::{self, FlockStatus};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

const USAGE: &str = "\
banc — rig utilities for the banc HIL framework

USAGE:
    banc doctor                 check every element of the rig config
    banc rig status             who holds the rig right now
    banc probe <verb> [args..]  probe-rs against the configured target
                                (chip/probe/remote-host/token injected;
                                 verbs: reset, info, run, attach, erase, ...)

The rig config is located like suites locate it: $BANC_RIG, else
banc-rig.toml searched upward from the current directory.";

/// TCP reachability checks answer "is it up", not "is it fast"; keep the
/// wait short so doctor on a half-dead rig finishes in seconds.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let strs: Vec<&str> = args.iter().map(String::as_str).collect();
    match strs.as_slice() {
        ["doctor"] => doctor(),
        ["rig", "status"] => rig_status(),
        ["probe", verb, rest @ ..] => probe(verb, rest),
        ["--help"] | ["-h"] | ["help"] | [] => {
            println!("{USAGE}");
            ExitCode::SUCCESS
        }
        other => {
            eprintln!("unknown command: {}\n\n{USAGE}", other.join(" "));
            ExitCode::from(2)
        }
    }
}

/// Locate + load the rig config or explain why not. Exit code 2 ("no rig
/// here") is distinct from 1 ("rig present but broken") so callers can tell
/// a non-rig machine from a sick one.
fn load_config() -> Result<(RigConfig, PathBuf), ExitCode> {
    match RigConfig::locate() {
        Ok(Some(path)) => match RigConfig::load(&path) {
            Ok(config) => {
                let base = path.parent().unwrap_or(Path::new(".")).to_path_buf();
                Ok((config, base))
            }
            Err(e) => {
                eprintln!("rig config unusable: {e:#}");
                Err(ExitCode::FAILURE)
            }
        },
        Ok(None) => {
            eprintln!(
                "no rig here: banc-rig.toml not found (set BANC_RIG or run from a rig checkout)"
            );
            Err(ExitCode::from(2))
        }
        Err(e) => {
            eprintln!("locating rig config: {e:#}");
            Err(ExitCode::FAILURE)
        }
    }
}

fn rig_status() -> ExitCode {
    let (config, base_dir) = match load_config() {
        Ok(v) => v,
        Err(code) => return code,
    };
    if let Some(lease) = &config.rig.lease {
        let token = match rig::read_token(&lease.token_file, &base_dir) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("lease token: {e:#}");
                return ExitCode::FAILURE;
            }
        };
        match banc_host::net::lease::query_status(&lease.addr, &token) {
            Ok(status) => match status.holder {
                Some(holder) => println!(
                    "held by '{holder}' (lease at {}, expires in {}s unless renewed)",
                    lease.addr, status.expires_in_s
                ),
                None => println!("free (lease at {})", lease.addr),
            },
            Err(e) => {
                eprintln!(
                    "lease server {} did not answer a status query: {e:#}\n\
                     (an older rig daemon predates status; the rig may still be usable)",
                    lease.addr
                );
                return ExitCode::FAILURE;
            }
        }
    } else {
        let path = rig::lock_path(&config, &base_dir);
        match rig::flock_status(&path) {
            Ok(FlockStatus::Free) => println!("free (flock at {})", path.display()),
            Ok(FlockStatus::Held(Some(h))) => println!(
                "held by '{}' (pid {}, since unix {}, flock at {})",
                h.holder,
                h.pid,
                h.since,
                path.display()
            ),
            Ok(FlockStatus::Held(None)) => println!(
                "held by an unidentified process (flock at {})",
                path.display()
            ),
            Err(e) => {
                eprintln!("probing rig lock: {e:#}");
                return ExitCode::FAILURE;
            }
        }
    }
    ExitCode::SUCCESS
}

struct Doctor {
    failed: u32,
    passed: u32,
}

impl Doctor {
    fn ok(&mut self, what: impl std::fmt::Display) {
        self.passed += 1;
        println!("  ok    {what}");
    }
    fn fail(&mut self, what: impl std::fmt::Display) {
        self.failed += 1;
        println!("  FAIL  {what}");
    }
    fn note(&mut self, what: impl std::fmt::Display) {
        println!("  --    {what}");
    }
    fn check(&mut self, what: &str, r: anyhow::Result<String>) {
        match r {
            Ok(detail) if detail.is_empty() => self.ok(what),
            Ok(detail) => self.ok(format!("{what}: {detail}")),
            Err(e) => self.fail(format!("{what}: {e:#}")),
        }
    }
}

fn doctor() -> ExitCode {
    let (config, base_dir) = match load_config() {
        Ok(v) => v,
        Err(code) => return code,
    };
    let name = config.rig.name.as_deref().unwrap_or("unnamed");
    println!("banc doctor — rig '{name}' ({})", base_dir.display());
    let mut d = Doctor {
        failed: 0,
        passed: 0,
    };
    // Loading already validated; reaching here means the config is sound.
    d.ok(format!(
        "config: {} target, {} assistant(s), {} instrument(s)",
        config.target.iter().count(),
        config.assistants.len(),
        config.instruments.len()
    ));

    match (&config.rig.lease, &config.rig.lock_file) {
        (Some(lease), _) => {
            let holder = rig::read_token(&lease.token_file, &base_dir).and_then(|token| {
                let status = banc_host::net::lease::query_status(&lease.addr, &token)?;
                Ok(match status.holder {
                    Some(h) => format!("held by '{h}'"),
                    None => "free".to_owned(),
                })
            });
            d.check(&format!("lease server {}", lease.addr), holder);
        }
        (None, _) => {
            let path = rig::lock_path(&config, &base_dir);
            let status = rig::flock_status(&path).map(|s| match s {
                FlockStatus::Free => "free".to_owned(),
                FlockStatus::Held(Some(h)) => format!("held by '{}' (pid {})", h.holder, h.pid),
                FlockStatus::Held(None) => "held by an unidentified process".to_owned(),
            });
            d.check(&format!("rig lock {}", path.display()), status);
        }
    }

    if let Some(target) = &config.target {
        match which_probe_rs() {
            Some(path) => d.ok(format!("probe-rs at {}", path.display())),
            None => d.fail("probe-rs not on PATH"),
        }
        if let Some(host) = &target.probe_host {
            d.check(
                &format!("target '{}' probe server {host}", target.chip),
                tcp_reach(host).map(|()| String::new()),
            );
        } else {
            d.note(format!(
                "target '{}': local USB probe (not probed; use `banc probe info`)",
                target.chip
            ));
        }
        if let Some(token_file) = &target.token_file {
            d.check(
                "target probe token",
                rig::read_token(token_file, &base_dir).map(|_| String::new()),
            );
        }
    }

    for a in &config.assistants {
        if let Some(addr) = &a.addr {
            let reach = a
                .token_file
                .as_ref()
                .map(|t| rig::read_token(t, &base_dir).map(|_| ()))
                .unwrap_or(Ok(()))
                .and_then(|()| tcp_reach(addr));
            d.check(
                &format!("assistant '{}' at {addr}", a.name),
                reach.map(|()| String::new()),
            );
        } else {
            d.note(format!(
                "assistant '{}': USB (checked at suite runtime, not here)",
                a.name
            ));
        }
    }

    for i in &config.instruments {
        match &i.address {
            Some(addr) if addr.contains(':') && !addr.contains('/') => d.check(
                &format!("instrument '{}' at {addr}", i.name),
                tcp_reach(addr).map(|()| String::new()),
            ),
            _ => d.note(format!(
                "instrument '{}' ({}): no network address to probe",
                i.name, i.kind
            )),
        }
    }

    let artifacts = std::env::var_os("BANC_ARTIFACTS")
        .map(PathBuf::from)
        .unwrap_or_else(|| base_dir.join("target").join("banc-artifacts"));
    let writable = std::fs::create_dir_all(&artifacts)
        .and_then(|()| {
            let probe = artifacts.join(".banc-doctor");
            std::fs::write(&probe, b"")?;
            std::fs::remove_file(&probe)
        })
        .map(|()| String::new())
        .map_err(anyhow::Error::from);
    d.check(&format!("artifacts dir {}", artifacts.display()), writable);

    println!("{} ok, {} failed", d.passed, d.failed);
    if d.failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn probe(verb: &str, rest: &[&str]) -> ExitCode {
    let (config, base_dir) = match load_config() {
        Ok(v) => v,
        Err(code) => return code,
    };
    let Some(target) = &config.target else {
        eprintln!("rig config has no [target]; nothing to probe");
        return ExitCode::FAILURE;
    };
    let mut cmd = std::process::Command::new("probe-rs");
    cmd.arg(verb);
    // `list` enumerates probes rather than talking to a chip; the chip and
    // probe selectors are not valid arguments to it.
    if verb != "list" {
        cmd.args(["--chip", &target.chip]);
        if let Some(probe) = &target.probe {
            cmd.args(["--probe", probe]);
        }
    }
    if let Some(host) = &target.probe_host {
        cmd.args(["--host", host]);
        if let Some(token_file) = &target.token_file {
            match rig::read_token(token_file, &base_dir) {
                Ok(token) => {
                    cmd.args(["--token", &token]);
                }
                Err(e) => {
                    eprintln!("probe token: {e:#}");
                    return ExitCode::FAILURE;
                }
            }
        }
    }
    cmd.args(rest);
    match cmd.status() {
        Ok(status) => match status.code() {
            Some(code) => ExitCode::from(code.clamp(0, 255) as u8),
            None => {
                eprintln!("probe-rs terminated by signal");
                ExitCode::FAILURE
            }
        },
        Err(e) => {
            eprintln!("running probe-rs: {e} (is it installed and on PATH?)");
            ExitCode::FAILURE
        }
    }
}

fn which_probe_rs() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join("probe-rs"))
        .find(|p| p.is_file())
}

/// TCP-connect to `addr`, which may carry a URL scheme (`ws://host:port`,
/// `https://host:port`) or be a bare `host:port`.
fn tcp_reach(addr: &str) -> anyhow::Result<()> {
    let hostport = addr
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(addr)
        .trim_end_matches('/');
    use std::net::ToSocketAddrs;
    let addrs: Vec<_> = hostport
        .to_socket_addrs()
        .map_err(|e| anyhow::anyhow!("resolving {hostport}: {e}"))?
        .collect();
    let mut last = None;
    for sa in addrs {
        match std::net::TcpStream::connect_timeout(&sa, CONNECT_TIMEOUT) {
            Ok(_) => return Ok(()),
            Err(e) => last = Some(e),
        }
    }
    Err(match last {
        Some(e) => anyhow::anyhow!("connecting to {hostport}: {e}"),
        None => anyhow::anyhow!("{hostport} resolved to no addresses"),
    })
}
