//! Drives a [`Machine`] on its own thread: paces virtual time to wall-clock time,
//! applies front-end commands, models power states (deep sleep), and publishes
//! status/console output through `sim_api`.

use std::time::{Duration, Instant};

use sim_api::{Command, RunState, SimPorts};

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

pub fn run(mut m: Box<dyn Machine>, ports: SimPorts, opts: RunnerOptions) -> RunOutcome {
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
    let mut presses_done = 0u64;
    let mut touch_release_at: Option<(u64, sim_api::TouchZone)> = None;
    let mut touches_done = 0u64;
    ports.status.lock().board = m.board().info();
    {
        let mut st = ports.status.lock();
        st.firmware = opts.firmware_name.clone();
        st.turbo = turbo;
    }

    'main: loop {
        // ---- commands ----
        while let Ok(cmd) = ports.commands.try_recv() {
            let mut rebase = false;
            match cmd {
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
                Command::SetDocked(docked) => {
                    m.board().set_docked(docked);
                    ports.console.lock().push_sim(if docked { "placed on dock" } else { "removed from dock" });
                    let charging = m.board().charging();
                    let mut st = ports.status.lock();
                    st.docked = docked;
                    st.charging = charging;
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
                Command::SetTurbo(t) => {
                    turbo = t;
                    ports.status.lock().turbo = t;
                    rebase = true;
                }
                Command::DumpDebug => {
                    let now = m.now_ns();
                    let text = format!("{}\n{}", m.debug_dump(), m.board().diagnostics(now));
                    let mut c = ports.console.lock();
                    for line in text.lines() {
                        c.push_sim(line);
                    }
                }
                Command::Pause(p) => {
                    paused = p;
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
            presses_done += 1;
            let mut st = ports.status.lock();
            st.button_down = false;
            st.presses_done = presses_done;
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
                for t in [release_at, touch_release_at.map(|t| t.0)].into_iter().flatten() {
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
