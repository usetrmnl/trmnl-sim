//! Drives a [`Machine`] on its own thread: paces virtual time to wall-clock time,
//! applies front-end commands, models power states (deep sleep), and publishes
//! status/console output through `sim_api`.

use std::path::Path;
use std::time::{Duration, Instant};

use sim_api::{Command, CoverageSummary, Faults, RunState, SavePointInfo, SavePointSource, SimPorts, Status};

use crate::coverage::Reporter;
use crate::savepoint::{FirmwareId, SavePoint, SavedPower, StateReader, StateWriter};
use crate::soc::{Machine, Output, ResetKind, SliceExit};

pub struct RunnerOptions {
    /// Stop after this much virtual time (headless runs).
    pub max_virtual_ns: Option<u64>,
    pub turbo: bool,
    /// Fast-forward deep sleeps instead of waiting them out in real time.
    pub fast_sleep: bool,
    /// Also copy console output to stdout.
    pub echo_console: bool,
    /// Print a CPU profile when stopping.
    pub profile: bool,
    pub firmware_name: String,
    /// Stop the run when the machine halts (headless runs without a controller).
    pub exit_on_halt: bool,
    /// The build being run (save points only restore onto the same one).
    pub firmware: FirmwareId,
    /// Start from this save point instead of a power-on boot.
    pub restore: Option<SavePoint>,
    /// Write code coverage when the run ends (and on request); the machine records it.
    pub coverage: Option<Reporter>,
    /// Faults injected from the start (`--faults`).
    pub faults: Faults,
}

/// How a run ended.
pub struct RunOutcome {
    pub halted: Option<String>,
}

enum Power {
    On,
    DeepSleep { wake_at: Option<u64>, gpio_low_mask: u64 },
    Halted,
}

/// In-memory save points (encoded), oldest first.
#[derive(Default)]
struct Slots {
    next_id: u32,
    list: Vec<(SavePointInfo, Vec<u8>)>,
}

impl Slots {
    const MAX: usize = 16;

    fn add(&mut self, sp: &SavePoint, data: Vec<u8>, path: Option<&Path>) -> SavePointInfo {
        self.next_id += 1;
        let info = SavePointInfo {
            id: self.next_id,
            label: sp.label.clone(),
            deep_sleep: sp.deep_sleep(),
            sim_time_ns: sp.soc.now_ns,
            wake_at_ns: match sp.power {
                SavedPower::DeepSleep { wake_at, .. } => wake_at,
                SavedPower::Off => None,
            },
            path: path.map(Path::to_path_buf),
            bytes: data.len(),
        };
        // A file loaded again replaces its previous slot.
        self.list.retain(|(i, _)| path.is_none() || i.path.as_deref() != path);
        self.list.push((info.clone(), data));
        if self.list.len() > Self::MAX {
            self.list.remove(0);
        }
        info
    }

    fn infos(&self) -> Vec<SavePointInfo> {
        self.list.iter().map(|(i, _)| i.clone()).collect()
    }
}

/// Capture the device. In deep sleep that is everything that survives it; otherwise
/// only what survives a battery pull.
fn take_savepoint(
    m: &mut dyn Machine,
    power: &Power,
    st: &Status,
    firmware: &FirmwareId,
    label: Option<String>,
) -> Result<SavePoint, String> {
    let now = m.now_ns();
    if m.board().display_status(now).0 {
        return Err("the display is refreshing; take the save point once it is idle".into());
    }
    let (saved, powered) = match *power {
        Power::DeepSleep { wake_at, gpio_low_mask } => (SavedPower::DeepSleep { wake_at, gpio_low_mask }, true),
        Power::On | Power::Halted => (SavedPower::Off, false),
    };
    let soc = m.save_soc(powered);
    let mut w = StateWriter::new();
    m.board().save_state(&mut w, powered);
    let label = label.filter(|l| !l.trim().is_empty()).unwrap_or_else(|| {
        let what = match saved {
            SavedPower::DeepSleep { .. } => "deep sleep",
            SavedPower::Off => "power-off",
        };
        format!("{what} at {:.1} s, boot {}", now as f64 / 1e9, soc.boots)
    });
    Ok(SavePoint {
        firmware: firmware.clone(),
        board: m.board().info().name,
        label,
        created: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs()),
        power: saved,
        soc,
        board_state: w.into_bytes(),
        battery_mv: st.battery_mv,
        docked: st.docked,
        wifi_available: st.wifi_available,
    })
}

/// Replace the machine's state with a save point; returns the power state to continue in.
fn apply_savepoint(m: &mut dyn Machine, sp: &SavePoint, firmware: &FirmwareId) -> anyhow::Result<Power> {
    sp.check_compatible(firmware, &m.board().info().name)?;
    m.restore_soc(&sp.soc)?;
    let mut r = StateReader::new(&sp.board_state);
    let restored = m.board().restore_state(&mut r, sp.deep_sleep()).and_then(|_| r.finish());
    if let Err(e) = restored {
        // Don't leave a half-restored device behind.
        m.reset(ResetKind::PowerOn);
        return Err(e.context("restoring the board (the device was power-cycled instead)"));
    }
    m.set_wifi_available(sp.wifi_available);
    Ok(match sp.power {
        SavedPower::DeepSleep { wake_at, gpio_low_mask } => Power::DeepSleep { wake_at, gpio_low_mask },
        SavedPower::Off => {
            m.reset(ResetKind::PowerOn);
            Power::On
        }
    })
}

fn describe_restore(sp: &SavePoint) -> String {
    match sp.power {
        SavedPower::DeepSleep { wake_at: Some(t), .. } => format!(
            "restored save point \"{}\": deep sleep, wakes in {:.1} s",
            sp.label,
            t.saturating_sub(sp.soc.now_ns) as f64 / 1e9
        ),
        SavedPower::DeepSleep { wake_at: None, .. } => {
            format!("restored save point \"{}\": deep sleep (no timer)", sp.label)
        }
        SavedPower::Off => format!("restored save point \"{}\": powering on", sp.label),
    }
}

pub fn run(mut m: Box<dyn Machine>, ports: SimPorts, mut opts: RunnerOptions) -> RunOutcome {
    let started = Instant::now();
    let mut turbo = opts.turbo;
    let mut paused = false;
    let mut power = Power::On;
    let mut anchor_wall = Instant::now();
    let mut anchor_virt = m.now_ns();
    let mut last_status = Instant::now();
    let mut last_rate = (Instant::now(), m.instructions(), m.now_ns());
    let mut mips = 0.0;
    let mut ratio = 0.0;
    let mut button = false;
    let mut halted_msg = String::new();
    let mut last_portal: Option<String> = None;
    let mut was_realtime = true;
    let mut release_at: Option<u64> = None;
    // A PressRepeat in progress: the next press starts at `press_at`; (ms, gap, presses left).
    let mut press_at: Option<u64> = None;
    let mut repeat: Option<(u64, u64, u32)> = None;
    let mut presses_done = 0u64;
    let mut touch_release_at: Option<(u64, sim_api::TouchZone)> = None;
    let mut touches_done = 0u64;
    let mut slots = Slots::default();
    let mut faults = Faults::default();
    let mut power_losses = 0u64;
    ports.status.lock().board = m.board().info();
    if !opts.faults.is_empty() {
        faults = set_faults(m.as_mut(), &ports, opts.faults.clone());
    }
    {
        let mut st = ports.status.lock();
        st.firmware = opts.firmware_name.clone();
        st.turbo = turbo;
        st.partitions = m.partitions();
    }
    if let Some(sp) = &opts.restore {
        // The power-on boot being replaced printed its ROM banner already.
        m.take_output();
        match apply_savepoint(m.as_mut(), sp, &opts.firmware) {
            Ok(p) => {
                power = p;
                // Faults are the environment, not device state: they stay.
                let _ = m.set_faults(&faults);
                ports.console.lock().push_sim(&describe_restore(sp));
                let mut st = ports.status.lock();
                st.battery_mv = sp.battery_mv;
                st.docked = sp.docked;
                st.wifi_available = sp.wifi_available;
                st.charging = m.board().charging();
                anchor_virt = m.now_ns();
                last_rate = (Instant::now(), m.instructions(), m.now_ns());
            }
            Err(e) => {
                let msg = format!("restoring the save point failed: {e:#}");
                ports.console.lock().push_sim(&msg);
                return RunOutcome { halted: Some(msg) };
            }
        }
    }

    'main: loop {
        let bluetooth_now = m.now_ns();
        m.bluetooth().pump(bluetooth_now);
        // ---- commands ----
        while let Ok(cmd) = ports.commands.try_recv() {
            let mut rebase = false;
            match cmd {
                Command::Bluetooth { operation, reply } => {
                    let now = m.now_ns();
                    m.bluetooth().operate(operation, reply, now);
                }
                Command::Quit => break 'main,
                Command::Button(down) => {
                    button = down;
                    release_at = None;
                    m.board().set_button(down);
                    ports.status.lock().button_down = down;
                }
                Command::Press { ms } => {
                    button = true;
                    release_at = Some(m.now_ns() + ms * 1_000_000);
                    (press_at, repeat) = (None, None);
                    m.board().set_button(true);
                    ports.status.lock().button_down = true;
                }
                Command::PressRepeat { ms, gap_ms, count } => {
                    button = true;
                    release_at = Some(m.now_ns() + ms * 1_000_000);
                    press_at = None;
                    repeat = Some((ms, gap_ms, count.max(1) - 1));
                    m.board().set_button(true);
                    ports.status.lock().button_down = true;
                }
                Command::Touch { zone, ms } => {
                    touch_release_at = Some((m.now_ns() + ms.max(1) * 1_000_000, zone));
                    m.board().set_touch(zone, true);
                    let mut st = ports.status.lock();
                    st.touching = Some(zone);
                    st.touch_mask |= zone.bit();
                }
                Command::TouchDown(zone) => {
                    m.board().set_touch(zone, true);
                    let mut st = ports.status.lock();
                    st.touching = Some(zone);
                    st.touch_mask |= zone.bit();
                }
                Command::TouchUp(zone) => {
                    m.board().set_touch(zone, false);
                    let mut st = ports.status.lock();
                    st.touch_mask &= !zone.bit();
                    if st.touching == Some(zone) {
                        st.touching = None;
                    }
                }
                Command::Gesture(g) => {
                    let accepted = m.board().gesture(g);
                    ports
                        .console
                        .lock()
                        .push_sim(&format!("touch bar {g:?}{}", if accepted { "" } else { " (not enabled: ignored)" }));
                }
                Command::SetDocked(docked) => {
                    m.board().set_docked(docked);
                    ports.console.lock().push_sim(if docked { "placed on dock" } else { "removed from dock" });
                    let charging = m.board().charging();
                    let mut st = ports.status.lock();
                    st.docked = docked;
                    st.charging = charging;
                }
                Command::SetRefreshFlashing(on) => {
                    m.board().set_refresh_flashing(on);
                    ports.console.lock().push_sim(if on { "refresh flashing on" } else { "refresh flashing off" });
                }
                Command::SetBatteryMv(mv) => {
                    m.board().set_battery_mv(mv);
                    let charging = m.board().charging();
                    let mut st = ports.status.lock();
                    st.battery_mv = mv;
                    st.charging = charging;
                }
                Command::Reset => {
                    ports.console.lock().push_sim("reset button pressed");
                    m.reset(ResetKind::ResetPin);
                    power = Power::On;
                    rebase = true;
                }
                Command::PowerCycle => {
                    ports.console.lock().push_sim("power cycled");
                    m.reset(ResetKind::PowerOn);
                    power = Power::On;
                    rebase = true;
                }
                Command::WakeFromSleep => {
                    if let Power::DeepSleep { wake_at, .. } = power {
                        if let Some(t) = wake_at {
                            let now = m.now_ns();
                            m.advance_time(t.saturating_sub(now));
                        }
                        ports.console.lock().push_sim("woken from deep sleep (timer)");
                        m.reset(ResetKind::DeepSleepWake { by_timer: true, by_gpio: false });
                        power = Power::On;
                        rebase = true;
                    }
                }
                Command::SetWifiAvailable(on) => {
                    m.set_wifi_available(on);
                    ports.console.lock().push_sim(if on {
                        "WiFi network in range"
                    } else {
                        "WiFi network out of range"
                    });
                    ports.status.lock().wifi_available = on;
                }
                Command::SetWifiNetworks(nets) => {
                    m.set_wifi_networks(&nets);
                    let names: Vec<&str> = nets.iter().map(|n| n.ssid.as_str()).collect();
                    ports.console.lock().push_sim(&format!("WiFi networks in range: {}", names.join(", ")));
                }
                Command::SetPortalClient(on) => {
                    m.set_portal_client(on);
                    ports.console.lock().push_sim(if on {
                        "portal client joins the setup access point"
                    } else {
                        "portal client stays off the setup access point"
                    });
                }
                Command::SetTurbo(t) => {
                    turbo = t;
                    ports.status.lock().turbo = t;
                    rebase = true;
                }
                Command::SetFaults(f) => faults = set_faults(m.as_mut(), &ports, f),
                Command::DumpDebug => {
                    let now = m.now_ns();
                    let text = format!("{}\n{}", m.debug_dump(), m.board().diagnostics(now));
                    let mut c = ports.console.lock();
                    for line in text.lines() {
                        c.push_sim(line);
                    }
                }
                Command::ReadPreferences(reply) => {
                    let mut snapshot = m.preferences();
                    snapshot.editable = matches!(power, Power::DeepSleep { .. });
                    let _ = reply.send(snapshot);
                }
                Command::ChangePreference { change, reply } => {
                    let result = if matches!(power, Power::DeepSleep { .. }) {
                        m.change_preference(&change).map(|()| {
                            let mut snapshot = m.preferences();
                            snapshot.editable = true;
                            snapshot
                        })
                    } else {
                        Err("preferences can only be edited during deep sleep; running, paused, and light-sleep firmware may cache NVS".into())
                    };
                    let _ = reply.send(result);
                }
                Command::Memcheck(reply) => {
                    let _ = reply.send(m.memcheck_json().unwrap_or_else(|| r#"{"enabled": false}"#.into()));
                }
                Command::Pause(p) => {
                    paused = p;
                    rebase = true;
                }
                Command::SavePoint { label, path, reply } => {
                    let st = ports.status.lock().clone();
                    let result = take_savepoint(m.as_mut(), &power, &st, &opts.firmware, label).and_then(|sp| {
                        let data = sp.encode();
                        if let Some(p) = &path {
                            std::fs::write(p, &data).map_err(|e| format!("writing {}: {e}", p.display()))?;
                        }
                        let info = slots.add(&sp, data, path.as_deref());
                        ports.console.lock().push_sim(&format!(
                            "save point #{} \"{}\" taken ({} KB){}",
                            info.id,
                            info.label,
                            info.bytes / 1024,
                            path.as_ref().map(|p| format!(", saved to {}", p.display())).unwrap_or_default()
                        ));
                        ports.status.lock().savepoints = slots.infos();
                        Ok(info)
                    });
                    if let Err(e) = &result {
                        ports.console.lock().push_sim(&format!("save point not taken: {e}"));
                    }
                    if let Some(tx) = reply {
                        let _ = tx.send(result);
                    }
                }
                Command::RestoreSavePoint { from, reply } => {
                    let loaded = match &from {
                        SavePointSource::Slot(id) => slots
                            .list
                            .iter()
                            .find(|(i, _)| i.id == *id)
                            .ok_or_else(|| format!("no save point #{id}"))
                            .and_then(|(i, d)| {
                                SavePoint::decode(d).map(|sp| (sp, i.clone())).map_err(|e| format!("{e:#}"))
                            }),
                        SavePointSource::File(p) => {
                            std::fs::read(p).map_err(|e| format!("reading {}: {e}", p.display())).and_then(|d| {
                                let sp = SavePoint::decode(&d).map_err(|e| format!("{}: {e:#}", p.display()))?;
                                sp.check_compatible(&opts.firmware, &m.board().info().name)
                                    .map_err(|e| format!("{e:#}"))?;
                                let info = slots.add(&sp, d, Some(p));
                                Ok((sp, info))
                            })
                        }
                    };
                    let result = loaded.and_then(|(sp, info)| {
                        power = apply_savepoint(m.as_mut(), &sp, &opts.firmware).map_err(|e| format!("{e:#}"))?;
                        let _ = m.set_faults(&faults);
                        ports.console.lock().push_sim(&describe_restore(&sp));
                        // Inputs in progress end with the old device.
                        if release_at.take().is_some() || press_at.take().is_some() {
                            presses_done += 1;
                        }
                        repeat = None;
                        if touch_release_at.take().is_some() {
                            touches_done += 1;
                        }
                        button = false;
                        m.board().set_button(false);
                        let now = m.now_ns();
                        let charging = m.board().charging();
                        let refreshes = m.board().display_status(now).1;
                        let mut st = ports.status.lock();
                        // Publish the restored device right away, so a caller never sees the old one.
                        st.sim_time_ns = now;
                        st.boot_count = m.boot_count();
                        st.display_refreshes = refreshes;
                        st.state = match power {
                            Power::DeepSleep { wake_at, .. } => RunState::DeepSleep { wake_at_ns: wake_at },
                            _ => RunState::Running,
                        };
                        st.battery_mv = sp.battery_mv;
                        st.docked = sp.docked;
                        st.charging = charging;
                        st.wifi_available = sp.wifi_available;
                        st.button_down = false;
                        st.touching = None;
                        st.touch_mask = 0;
                        st.presses_done = presses_done;
                        st.touches_done = touches_done;
                        st.savepoints = slots.infos();
                        Ok(info)
                    });
                    match &result {
                        Ok(_) => rebase = true,
                        Err(e) => ports.console.lock().push_sim(&format!("save point not restored: {e}")),
                    }
                    if let Some(tx) = reply {
                        let _ = tx.send(result);
                    }
                }
                Command::AddApp { elf, reply } => {
                    let result = crate::firmware::ExtraApp::from_elf(&elf).map_err(|e| format!("{e:#}")).map(|a| {
                        m.add_app(a.elf_sha256, a.symbols, a.name.clone());
                        a.name
                    });
                    if let Err(e) = &result {
                        ports.console.lock().push_sim(&format!("OTA firmware ELF not loaded: {e}"));
                    }
                    if let Some(tx) = reply {
                        let _ = tx.send(result);
                    }
                }
                Command::WriteCoverage { path, reset, reply } => {
                    let _ = reply.send(write_coverage(m.as_mut(), opts.coverage.as_mut(), path.as_deref(), reset));
                    // Don't make up for the time spent writing it.
                    rebase = true;
                }
            }
            if rebase {
                anchor_wall = Instant::now();
                anchor_virt = m.now_ns();
            }
        }

        // ---- timed button release ----
        if let Some(t) = release_at
            && m.now_ns() >= t
        {
            release_at = None;
            button = false;
            m.board().set_button(false);
            match repeat {
                Some((_, gap, left)) if left > 0 => press_at = Some(m.now_ns() + gap * 1_000_000),
                _ => {
                    repeat = None;
                    presses_done += 1;
                }
            }
            let mut st = ports.status.lock();
            st.button_down = false;
            st.presses_done = presses_done;
        }
        if let Some(t) = press_at
            && m.now_ns() >= t
            && let Some((ms, gap, left)) = repeat
        {
            press_at = None;
            repeat = Some((ms, gap, left - 1));
            button = true;
            release_at = Some(m.now_ns() + ms * 1_000_000);
            m.board().set_button(true);
            ports.status.lock().button_down = true;
        }

        if let Some((t, zone)) = touch_release_at
            && m.now_ns() >= t
        {
            touch_release_at = None;
            m.board().set_touch(zone, false);
            touches_done += 1;
            let mut st = ports.status.lock();
            st.touching = None;
            st.touch_mask &= !zone.bit();
            st.touches_done = touches_done;
        }

        // ---- run ----
        let now_v = m.now_ns();
        let wall_target = anchor_virt + anchor_wall.elapsed().as_nanos() as u64;
        match power {
            _ if paused => std::thread::sleep(Duration::from_millis(10)),
            Power::Halted => std::thread::sleep(Duration::from_millis(20)),
            Power::On => {
                // Turbo fast-forwards, except while the host network is in use.
                let realtime = !turbo || m.realtime_required();
                if turbo && realtime && !was_realtime {
                    anchor_wall = Instant::now();
                    anchor_virt = now_v;
                }
                was_realtime = realtime;
                let wall_target = anchor_virt + anchor_wall.elapsed().as_nanos() as u64;
                let mut target = if realtime { wall_target.min(now_v + 20_000_000) } else { now_v + 20_000_000 };
                for t in [release_at, press_at, touch_release_at.map(|t| t.0)].into_iter().flatten() {
                    target = target.min(t.max(now_v + 1));
                }
                if target <= now_v {
                    std::thread::sleep(Duration::from_micros(500));
                } else {
                    let exit = m.run_slice(target);
                    if !realtime {
                        // Keep the pacing anchor current so real-time phases start from "now".
                        anchor_wall = Instant::now();
                        anchor_virt = m.now_ns();
                    }
                    match exit {
                        // Idle until a command (dock, touch...) arrives: nap rather than spin
                        // through tiny slices; the next slice catches up with wall time.
                        SliceExit::Reached if realtime && m.waiting_for_external() => {
                            std::thread::sleep(Duration::from_millis(5))
                        }
                        SliceExit::Reached => {}
                        SliceExit::DeepSleep { timer_ns, gpio_low_mask } => {
                            let wake_at = timer_ns.map(|t| m.now_ns() + t);
                            ports.console.lock().push_sim(&match timer_ns {
                                Some(t) => format!("deep sleep for {:.1} s", t as f64 / 1e9),
                                None => "deep sleep (no timer)".into(),
                            });
                            power = Power::DeepSleep { wake_at, gpio_low_mask };
                            anchor_wall = Instant::now();
                            anchor_virt = m.now_ns();
                        }
                        SliceExit::PowerLoss(what) => {
                            // Power comes straight back, like PowerCycle.
                            ports.console.lock().push_sim(&format!("power lost: {what}"));
                            power_losses += 1;
                            faults.power_loss = None;
                            {
                                let mut st = ports.status.lock();
                                st.faults = faults.clone();
                                st.power_losses = power_losses;
                            }
                            m.reset(ResetKind::PowerOn);
                            anchor_wall = Instant::now();
                            anchor_virt = m.now_ns();
                        }
                        SliceExit::Halted(msg) => {
                            ports.console.lock().push_sim(&format!("HALTED: {msg}"));
                            halted_msg = msg;
                            power = Power::Halted;
                            if opts.exit_on_halt {
                                break 'main;
                            }
                        }
                    }
                }
            }
            Power::DeepSleep { wake_at, gpio_low_mask } => {
                // Time passes; the board (display) keeps animating.
                // Deep sleep runs in wall-clock time (so it can be observed, and ended with
                // Wake / the button) unless --fast-sleep.
                let step = if opts.fast_sleep { 1_000_000_000 } else { wall_target.saturating_sub(now_v) };
                let step = match wake_at {
                    Some(t) => step.min(t.saturating_sub(now_v)),
                    None => step,
                };
                if step > 0 {
                    m.advance_time(step);
                }
                let now = m.now_ns();
                let low = !m.board().gpio_in(now).0;
                let by_gpio = gpio_low_mask & low != 0 || (button && gpio_low_mask != 0);
                let by_timer = wake_at.is_some_and(|t| m.now_ns() >= t);
                if by_gpio || by_timer {
                    ports.console.lock().push_sim(if by_gpio { "woken by button" } else { "woken by timer" });
                    m.reset(ResetKind::DeepSleepWake { by_timer, by_gpio });
                    power = Power::On;
                    anchor_wall = Instant::now();
                    anchor_virt = m.now_ns();
                } else if !opts.fast_sleep {
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        }

        // ---- output ----
        let out = m.take_output();
        if !out.is_empty() {
            let mut c = ports.console.lock();
            let mut so = opts.echo_console.then(|| std::io::stdout().lock());
            for item in out {
                use std::io::Write;
                match item {
                    Output::Serial(b) => {
                        if let Some(so) = so.as_mut() {
                            let _ = so.write_all(&b);
                        }
                        c.push_bytes(&b);
                    }
                    Output::Sim(s) => {
                        if let Some(so) = so.as_mut() {
                            let _ = writeln!(so, "[sim] {s}");
                        }
                        c.push_sim(&s);
                    }
                }
            }
        }

        if last_status.elapsed() > Duration::from_millis(50) {
            last_status = Instant::now();
            let (t0, i0, v0) = last_rate;
            let dt = t0.elapsed().as_secs_f64();
            if dt > 0.5 {
                mips = (m.instructions() - i0) as f64 / dt / 1e6;
                ratio = (m.now_ns() - v0) as f64 / 1e9 / dt;
                last_rate = (Instant::now(), m.instructions(), m.now_ns());
            }
            let now = m.now_ns();
            let (busy, refreshes) = m.board().display_status(now);
            let charging = m.board().charging();
            let net = m.net_status();
            let bluetooth = m.bluetooth().snapshot();
            let (programs, erases) = m.flash_stats();
            if net.portal_url != last_portal {
                if let Some(u) = &net.portal_url {
                    ports
                        .console
                        .lock()
                        .push_sim(&format!("device is running a WiFi access point; captive portal forwarded to {u}"));
                }
                last_portal = net.portal_url.clone();
            }
            let mut st = ports.status.lock();
            st.bluetooth = bluetooth;
            st.wifi_connected = net.connected;
            st.ip = net.ip;
            st.portal_url = net.portal_url.clone();
            st.sim_time_ns = now;
            st.mips = mips;
            st.speed_ratio = ratio;
            st.display_busy = busy;
            st.charging = charging;
            st.display_refreshes = refreshes;
            st.boot_count = m.boot_count();
            st.flash_programs = programs;
            st.flash_erases = erases;
            st.state = match power {
                _ if paused => RunState::Paused,
                Power::On => match m.light_sleep() {
                    Some(wake_at) => RunState::LightSleep { wake_at_ns: wake_at },
                    None => RunState::Running,
                },
                Power::DeepSleep { wake_at, .. } => RunState::DeepSleep { wake_at_ns: wake_at },
                Power::Halted => RunState::Halted(halted_msg.clone()),
            };
        }

        if let Some(max) = opts.max_virtual_ns
            && m.now_ns() >= max
        {
            break;
        }
    }
    m.flush();
    if opts.echo_console {
        let wall = started.elapsed().as_secs_f64();
        eprintln!(
            "\n[sim] ran {:.1} s virtual in {:.1} s wall; {:.0} M instructions ({:.0} MIPS avg)",
            m.now_ns() as f64 / 1e9,
            wall,
            m.instructions() as f64 / 1e6,
            m.instructions() as f64 / 1e6 / wall
        );
        if let Some(summary) = m.memcheck_summary() {
            eprint!("\n{summary}");
        }
    }
    if opts.coverage.is_some() {
        match write_coverage(m.as_mut(), opts.coverage.as_mut(), None, false) {
            Ok(s) => eprintln!(
                "[sim] coverage: {} of {} lines ({:.1}%), {} of {} functions in {} files -> {}",
                s.lines_hit,
                s.lines_found,
                100.0 * s.lines_hit as f64 / s.lines_found.max(1) as f64,
                s.functions_hit,
                s.functions_found,
                s.files,
                s.path
            ),
            Err(e) => eprintln!("[sim] coverage: {e}"),
        }
    }
    if opts.profile {
        println!("\n=== profile (samples per function) ===");
        for (name, n) in m.profile().into_iter().take(25) {
            println!("{n:>8}  {name}");
        }
        println!("\n=== cpu ===\n{}", m.debug_dump());
        let now = m.now_ns();
        println!("\n=== board ===\n{}", m.board().diagnostics(now));
    }
    RunOutcome { halted: (!halted_msg.is_empty()).then_some(halted_msg) }
}

fn write_coverage(
    m: &mut dyn Machine,
    reporter: Option<&mut Reporter>,
    path: Option<&std::path::Path>,
    reset: bool,
) -> Result<CoverageSummary, String> {
    let (Some(reporter), Some(cov)) = (reporter, m.coverage()) else {
        return Err("coverage is not being recorded (run with --coverage FILE)".into());
    };
    let summary = reporter.write(cov, path).map_err(|e| format!("{e:#}"))?;
    if reset {
        cov.clear();
    }
    Ok(summary)
}

/// Apply faults to the machine and publish them; returns what is in effect (without a
/// power-loss trigger that couldn't be armed).
fn set_faults(m: &mut dyn Machine, ports: &SimPorts, mut f: Faults) -> Faults {
    if let Err(e) = m.set_faults(&f) {
        ports.console.lock().push_sim(&format!("fault not armed: {e}"));
        f.power_loss = None;
    }
    ports.console.lock().push_sim(&format!("faults: {}", f.summary()));
    ports.status.lock().faults = f.clone();
    f
}
