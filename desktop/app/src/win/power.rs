// SPDX-License-Identifier: GPL-3.0-or-later
//! Windows AC-power monitor: watches `WM_POWERBROADCAST` (`PBT_APMPOWERSTATUSCHANGE`)
//! via a hidden top-level window and reads `GetSystemPowerStatus`. When AC power
//! transitions from online (`1`) to offline (`0`) and persists past the debounce
//! window, alerts connected phones (loud alarm ring + notification banner, or
//! notification chime only) using stock NectarLink Android v0.1.5 capabilities.

use std::{
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicIsize, AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use nectarlink_core::{DeviceId, DeviceKind, LinkState};
use windows::{
    Win32::{
        Foundation::{HWND, LPARAM, LRESULT, WPARAM},
        System::{
            LibraryLoader::GetModuleHandleW,
            Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS},
        },
        UI::WindowsAndMessaging::{
            CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW, MSG,
            PostMessageW, PostQuitMessage, RegisterClassExW, WM_CLOSE, WM_DESTROY, WM_POWERBROADCAST,
            WNDCLASSEXW, WS_EX_TOOLWINDOW, WS_POPUP,
        },
    },
    core::w,
};

use crate::settings::PowerLossAlertMode;

/// `PBT_APMPOWERSTATUSCHANGE` from `winuser.h`.
const PBT_APMPOWERSTATUSCHANGE: usize = 0x0A;

/// How long an `Online -> Offline` transition must persist before warning phones,
/// filtering out brief charger bumps or USB-C PD renegotiation flickers.
pub const UNPLUG_DEBOUNCE: Duration = Duration::from_secs(2);

/// Maximum duration to keep the phone alarm ringing before automatically
/// sending `ring(peer, false)` (Android's `Ringer` also caps at 60 s).
pub const RING_AUTO_STOP: Duration = Duration::from_secs(30);

/// AC line state reported by Windows `SYSTEM_POWER_STATUS.ACLineStatus`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcState {
    Online,
    Offline,
    Unknown,
}

/// Snapshot of system AC power and battery percentage from `GetSystemPowerStatus`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PowerSnapshot {
    pub ac: AcState,
    pub battery_percent: Option<u8>,
    pub has_battery: bool,
}

impl PowerSnapshot {
    /// Parses raw Win32 `SYSTEM_POWER_STATUS` byte fields into a typed snapshot.
    pub fn from_raw(ac_line_status: u8, battery_flag: u8, battery_life_percent: u8) -> Self {
        let ac = match ac_line_status {
            0 => AcState::Offline,
            1 => AcState::Online,
            _ => AcState::Unknown,
        };
        // 128 = no system battery, 255 = unknown status.
        let has_battery = battery_flag != 128 && battery_flag != 255;
        let battery_percent = (battery_life_percent <= 100).then_some(battery_life_percent);
        Self { ac, battery_percent, has_battery }
    }
}

/// Confirmed AC power transition emitted by [`PowerMonitorState`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerTransition {
    /// Confirmed `Online -> Offline` after [`UNPLUG_DEBOUNCE`].
    Unplugged { battery_percent: Option<u8> },
    /// Confirmed `Offline -> Online` after running on battery.
    Replugged { battery_percent: Option<u8> },
}

/// Immediate result of feeding a [`PowerSnapshot`] into [`PowerMonitorState`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleOutcome {
    /// No transition or timer change required.
    None,
    /// `Online -> Offline` observed; wait `delay` and re-check with `generation`.
    ScheduleDebounce { generation: u64, delay: Duration },
    /// Pending unplug bounced back to AC before the debounce elapsed.
    CancelDebounce,
    /// Confirmed transition to dispatch immediately.
    Emit(PowerTransition),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PendingUnplug {
    battery_percent: Option<u8>,
    generation: u64,
}

/// Pure, deterministic AC transition and debounce state machine.
#[derive(Debug)]
pub struct PowerMonitorState {
    debounce: Duration,
    stable_ac: Option<AcState>,
    pending_unplug: Option<PendingUnplug>,
    generation: u64,
}

impl PowerMonitorState {
    /// Creates the state machine seeded with the system's initial power status
    /// so starting while already on battery does not trigger a false alarm.
    pub fn new(initial: Option<PowerSnapshot>, debounce: Duration) -> Self {
        let stable_ac = initial.map(|s| s.ac).filter(|&ac| ac != AcState::Unknown);
        Self { debounce, stable_ac, pending_unplug: None, generation: 0 }
    }

    /// Processes a new power status reading (e.g. on `PBT_APMPOWERSTATUSCHANGE`).
    pub fn on_sample(&mut self, sample: PowerSnapshot) -> SampleOutcome {
        match sample.ac {
            AcState::Unknown => SampleOutcome::None,
            AcState::Offline => match self.stable_ac {
                None | Some(AcState::Unknown) => {
                    self.stable_ac = Some(AcState::Offline);
                    SampleOutcome::None
                }
                Some(AcState::Offline) => SampleOutcome::None,
                Some(AcState::Online) => {
                    if let Some(pending) = self.pending_unplug.as_mut() {
                        if sample.battery_percent.is_some() {
                            pending.battery_percent = sample.battery_percent;
                        }
                        SampleOutcome::None
                    } else {
                        self.generation = self.generation.wrapping_add(1);
                        let generation = self.generation;
                        self.pending_unplug = Some(PendingUnplug {
                            battery_percent: sample.battery_percent,
                            generation,
                        });
                        SampleOutcome::ScheduleDebounce { generation, delay: self.debounce }
                    }
                }
            },
            AcState::Online => {
                if self.pending_unplug.take().is_some() {
                    self.generation = self.generation.wrapping_add(1);
                    return SampleOutcome::CancelDebounce;
                }
                match self.stable_ac {
                    None | Some(AcState::Unknown) => {
                        self.stable_ac = Some(AcState::Online);
                        SampleOutcome::None
                    }
                    Some(AcState::Online) => SampleOutcome::None,
                    Some(AcState::Offline) => {
                        self.stable_ac = Some(AcState::Online);
                        SampleOutcome::Emit(PowerTransition::Replugged {
                            battery_percent: sample.battery_percent,
                        })
                    }
                }
            }
        }
    }

    /// Finalizes a scheduled debounce timer once it fires, verifying that the
    /// generation still matches and the live power reading is still `Offline`.
    pub fn on_debounce_fired(
        &mut self,
        generation: u64,
        current: Option<PowerSnapshot>,
    ) -> Option<PowerTransition> {
        let pending = self.pending_unplug?;
        if pending.generation != generation {
            return None;
        }
        self.pending_unplug = None;
        match current {
            Some(snap) if snap.ac == AcState::Offline => {
                self.stable_ac = Some(AcState::Offline);
                Some(PowerTransition::Unplugged {
                    battery_percent: snap.battery_percent.or(pending.battery_percent),
                })
            }
            Some(snap) if snap.ac == AcState::Online => {
                self.stable_ac = Some(AcState::Online);
                None
            }
            _ => None,
        }
    }

    /// Returns the generation of a pending unplug debounce, if any.
    #[cfg(test)]
    pub fn pending_generation(&self) -> Option<u64> {
        self.pending_unplug.map(|p| p.generation)
    }
}

/// Formats the notification `(title, body)` sent to paired phones on AC power loss.
pub fn format_unplugged_notice(pc_name: &str, battery_percent: Option<u8>) -> (String, String) {
    let name = pc_name.trim();
    let label = if name.is_empty() { "PC" } else { name };
    let title = format!("{label} unplugged from power");
    let body = match battery_percent {
        Some(pct) => format!("Running on battery ({pct}% remaining)."),
        None => "Running on battery.".to_owned(),
    };
    (title, body)
}

/// Queries Windows for the current AC power and battery state.
pub fn query_system_power() -> Option<PowerSnapshot> {
    let mut status = SYSTEM_POWER_STATUS::default();
    // SAFETY: `status` is a live, writable stack struct.
    unsafe { GetSystemPowerStatus(&mut status) }.ok()?;
    Some(PowerSnapshot::from_raw(status.ACLineStatus, status.BatteryFlag, status.BatteryLifePercent))
}

#[derive(Debug, Default)]
struct ActiveAlert {
    generation: u64,
    notification_key: Option<String>,
    ringed_peers: Vec<DeviceId>,
}

static STARTED: OnceLock<()> = OnceLock::new();
static WINDOW: AtomicIsize = AtomicIsize::new(0);
static MONITOR_STATE: OnceLock<Mutex<PowerMonitorState>> = OnceLock::new();
static ACTIVE_ALERT: Mutex<Option<ActiveAlert>> = Mutex::new(None);
static ALERT_GEN: AtomicU64 = AtomicU64::new(0);

fn monitor_state() -> &'static Mutex<PowerMonitorState> {
    MONITOR_STATE.get_or_init(|| Mutex::new(PowerMonitorState::new(query_system_power(), UNPLUG_DEBOUNCE)))
}

/// Starts the hidden Windows power-broadcast monitor thread (idempotent).
pub fn start() {
    STARTED.get_or_init(|| {
        let initial = query_system_power();
        let _ = MONITOR_STATE.set(Mutex::new(PowerMonitorState::new(initial, UNPLUG_DEBOUNCE)));
        tracing::info!(
            ac = ?initial.map(|s| s.ac),
            battery_percent = ?initial.and_then(|s| s.battery_percent),
            has_battery = initial.is_some_and(|s| s.has_battery),
            "power monitor started"
        );
        if let Err(e) = std::thread::Builder::new().name("win-power-monitor".into()).spawn(run_loop) {
            tracing::warn!(error = %e, "can't start Windows power monitor thread");
        }
    });
}

/// Stops the hidden monitor window cleanly during app shutdown.
pub fn stop() {
    stop_active_ring();
    let raw = WINDOW.swap(0, Ordering::AcqRel);
    if raw != 0 {
        // SAFETY: posting WM_CLOSE to our own window ends its message loop cleanly.
        unsafe {
            let _ = PostMessageW(Some(HWND(raw as *mut _)), WM_CLOSE, WPARAM(0), LPARAM(0));
        }
    }
}

/// Stops any active power-loss alarm ring (for example when the user disables
/// the setting or switches to Notification chime mode mid-alarm).
pub fn stop_active_ring() {
    let peers = {
        let mut guard = ACTIVE_ALERT.lock().unwrap_or_else(|e| e.into_inner());
        match guard.as_mut() {
            Some(active) if !active.ringed_peers.is_empty() => std::mem::take(&mut active.ringed_peers),
            _ => return,
        }
    };
    let Some(node) = crate::core_host::node() else { return };
    crate::core_host::spawn(async move {
        for peer in peers {
            if let Err(e) = node.ring(peer, false).await {
                tracing::debug!(peer = %peer.short(), error = %e, "couldn't stop power-loss ring");
            }
        }
    });
}

fn run_loop() {
    // SAFETY: registers a hidden top-level tool window (NOT HWND_MESSAGE, which
    // does not receive WM_POWERBROADCAST broadcasts) and pumps its message queue.
    let hwnd = unsafe {
        let Ok(instance) = GetModuleHandleW(None) else {
            tracing::warn!("power monitor: GetModuleHandleW failed");
            return;
        };
        let class = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(window_proc),
            hInstance: instance.into(),
            lpszClassName: w!("Nectarlink.PowerMonitor"),
            ..Default::default()
        };
        RegisterClassExW(&class);
        match CreateWindowExW(
            WS_EX_TOOLWINDOW,
            w!("Nectarlink.PowerMonitor"),
            w!("NectarLink power monitor"),
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance.into()),
            None,
        ) {
            Ok(hwnd) => hwnd,
            Err(e) => {
                tracing::warn!(error = %e, "power monitor: CreateWindowExW failed");
                return;
            }
        }
    };
    WINDOW.store(hwnd.0 as isize, Ordering::Release);
    let mut msg = MSG::default();
    // SAFETY: plain message loop for this thread's window; `GetMessageW` returns
    // > 0 for messages, 0 on WM_QUIT, and -1 on error.
    unsafe {
        while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
            DispatchMessageW(&msg);
        }
    }
    WINDOW.store(0, Ordering::Release);
}

unsafe extern "system" fn window_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_POWERBROADCAST => {
            if wparam.0 == PBT_APMPOWERSTATUSCHANGE {
                on_power_status_broadcast();
            }
            LRESULT(1)
        }
        WM_CLOSE => {
            // SAFETY: destroying our own window triggers WM_DESTROY -> PostQuitMessage(0).
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            // SAFETY: ends this thread's GetMessageW loop.
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        // SAFETY: default handling for all other messages.
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

fn on_power_status_broadcast() {
    let Some(snapshot) = query_system_power() else { return };
    let outcome = {
        let mut state = monitor_state().lock().unwrap_or_else(|e| e.into_inner());
        state.on_sample(snapshot)
    };
    tracing::debug!(
        ac = ?snapshot.ac,
        battery_percent = ?snapshot.battery_percent,
        outcome = ?outcome,
        "power status broadcast"
    );
    match outcome {
        SampleOutcome::None => {}
        SampleOutcome::CancelDebounce => {
            tracing::info!("AC power restored during debounce; unplug alert cancelled");
        }
        SampleOutcome::ScheduleDebounce { generation, delay } => {
            if crate::core_host::try_host().is_some() {
                crate::core_host::spawn(async move {
                    tokio::time::sleep(delay).await;
                    let current = query_system_power();
                    let transition = {
                        let mut state = monitor_state().lock().unwrap_or_else(|e| e.into_inner());
                        state.on_debounce_fired(generation, current)
                    };
                    if let Some(transition) = transition {
                        dispatch_transition(transition);
                    }
                });
            }
        }
        SampleOutcome::Emit(transition) => dispatch_transition(transition),
    }
}

fn dispatch_transition(transition: PowerTransition) {
    match transition {
        PowerTransition::Unplugged { battery_percent } => dispatch_unplugged(battery_percent),
        PowerTransition::Replugged { battery_percent } => dispatch_replugged(battery_percent),
    }
}

fn pc_display_name() -> String {
    hostname::get()
        .ok()
        .and_then(|h| h.into_string().ok())
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "PC".to_owned())
}

fn online_phone_peers() -> Vec<DeviceId> {
    let Some(host) = crate::core_host::try_host() else {
        return Vec::new();
    };
    host.hub.read(|s| {
        s.devices
            .iter()
            .filter(|d| {
                matches!(d.link, LinkState::Online { .. })
                    && !matches!(d.info.kind, DeviceKind::Desktop | DeviceKind::Laptop)
            })
            .map(|d| d.id)
            .collect()
    })
}

fn dispatch_unplugged(battery_percent: Option<u8>) {
    let enabled = crate::settings::power_loss_alert();
    let mode = crate::settings::power_loss_alert_mode();
    if !enabled {
        tracing::debug!(?battery_percent, "AC power lost (power loss alert disabled)");
        return;
    }
    let Some(node) = crate::core_host::node() else {
        tracing::warn!("AC power lost, but core node is not ready");
        return;
    };
    let peers = online_phone_peers();
    let alert_gen = ALERT_GEN.fetch_add(1, Ordering::AcqRel) + 1;
    let key = format!("pc:power:unplugged:{alert_gen}");
    let pc_name = pc_display_name();
    let (title, text) = format_unplugged_notice(&pc_name, battery_percent);
    let when_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_millis()).ok())
        .unwrap_or(0);
    let notification =
        crate::pc_notifications::build_pc_notification(&key, "NectarLink Power", &title, &text, when_ms);

    let ring_peers = if mode == PowerLossAlertMode::Alarm { peers.clone() } else { Vec::new() };
    let previous = {
        let mut guard = ACTIVE_ALERT.lock().unwrap_or_else(|e| e.into_inner());
        guard.replace(ActiveAlert {
            generation: alert_gen,
            notification_key: notification.as_ref().map(|n| n.key.clone()),
            ringed_peers: ring_peers.clone(),
        })
    };

    tracing::info!(
        mode = mode.as_str(),
        ?battery_percent,
        peers = peers.len(),
        "AC power lost; alerting connected phones"
    );

    crate::core_host::spawn(async move {
        if let Some(prev) = previous
            && let Some(prev_key) = prev.notification_key
        {
            node.notification_removed(prev_key).await;
        }
        if let Some(notif) = notification {
            node.notification_posted(notif).await;
        }
        if !ring_peers.is_empty() {
            for peer in &ring_peers {
                if let Err(e) = node.ring(*peer, true).await {
                    tracing::warn!(peer = %peer.short(), error = %e, "failed to ring phone on AC power loss");
                }
            }
            tokio::time::sleep(RING_AUTO_STOP).await;
            stop_ringing_for_generation(alert_gen).await;
        }
    });
}

async fn stop_ringing_for_generation(generation: u64) {
    let peers = {
        let mut guard = ACTIVE_ALERT.lock().unwrap_or_else(|e| e.into_inner());
        match guard.as_mut() {
            Some(active) if active.generation == generation && !active.ringed_peers.is_empty() => {
                std::mem::take(&mut active.ringed_peers)
            }
            _ => return,
        }
    };
    let Some(node) = crate::core_host::node() else { return };
    tracing::info!(peers = peers.len(), "auto-stopping power-loss ring after timeout");
    for peer in peers {
        if let Err(e) = node.ring(peer, false).await {
            tracing::debug!(peer = %peer.short(), error = %e, "couldn't auto-stop power-loss ring");
        }
    }
}

fn dispatch_replugged(battery_percent: Option<u8>) {
    let active = ACTIVE_ALERT.lock().unwrap_or_else(|e| e.into_inner()).take();
    tracing::info!(?battery_percent, had_active_alert = active.is_some(), "AC power restored");
    let Some(active) = active else { return };
    let Some(node) = crate::core_host::node() else { return };
    crate::core_host::spawn(async move {
        for peer in active.ringed_peers {
            if let Err(e) = node.ring(peer, false).await {
                tracing::debug!(peer = %peer.short(), error = %e, "couldn't stop ring on replug");
            }
        }
        if let Some(key) = active.notification_key {
            node.notification_removed(key).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(ac: AcState, pct: Option<u8>) -> PowerSnapshot {
        PowerSnapshot { ac, battery_percent: pct, has_battery: true }
    }

    #[test]
    fn parses_raw_windows_power_status_fields() {
        let online = PowerSnapshot::from_raw(1, 1, 95);
        assert_eq!(online.ac, AcState::Online);
        assert_eq!(online.battery_percent, Some(95));
        assert!(online.has_battery);

        let offline = PowerSnapshot::from_raw(0, 2, 42);
        assert_eq!(offline.ac, AcState::Offline);
        assert_eq!(offline.battery_percent, Some(42));
        assert!(offline.has_battery);

        let desktop = PowerSnapshot::from_raw(1, 128, 255);
        assert_eq!(desktop.ac, AcState::Online);
        assert_eq!(desktop.battery_percent, None);
        assert!(!desktop.has_battery);

        let unknown = PowerSnapshot::from_raw(255, 255, 255);
        assert_eq!(unknown.ac, AcState::Unknown);
        assert_eq!(unknown.battery_percent, None);
        assert!(!unknown.has_battery);
    }

    #[test]
    fn unplug_requires_debounce_then_fires_once_until_replugged() {
        let mut state = PowerMonitorState::new(Some(snap(AcState::Online, Some(90))), UNPLUG_DEBOUNCE);

        // Online -> Offline schedules debounce.
        let out = state.on_sample(snap(AcState::Offline, Some(89)));
        let gen1 = match out {
            SampleOutcome::ScheduleDebounce { generation, delay } => {
                assert_eq!(delay, UNPLUG_DEBOUNCE);
                generation
            }
            other => panic!("expected ScheduleDebounce, got {other:?}"),
        };

        // Duplicate Offline broadcasts during debounce update battery % without resetting the timer.
        assert_eq!(state.on_sample(snap(AcState::Offline, Some(88))), SampleOutcome::None);
        assert_eq!(state.pending_generation(), Some(gen1));

        // Debounce fires while still Offline -> emits Unplugged with freshest battery %.
        assert_eq!(
            state.on_debounce_fired(gen1, Some(snap(AcState::Offline, Some(87)))),
            Some(PowerTransition::Unplugged { battery_percent: Some(87) })
        );

        // Further battery drops while still Offline do NOT re-trigger.
        assert_eq!(state.on_sample(snap(AcState::Offline, Some(86))), SampleOutcome::None);
        assert_eq!(state.on_sample(snap(AcState::Offline, Some(85))), SampleOutcome::None);

        // Replugging emits Replugged immediately.
        assert_eq!(
            state.on_sample(snap(AcState::Online, Some(85))),
            SampleOutcome::Emit(PowerTransition::Replugged { battery_percent: Some(85) })
        );

        // Subsequent Online samples are no-ops.
        assert_eq!(state.on_sample(snap(AcState::Online, Some(86))), SampleOutcome::None);
    }

    #[test]
    fn transient_flicker_within_debounce_cancels_alert_and_invalidates_old_timer() {
        let mut state = PowerMonitorState::new(Some(snap(AcState::Online, Some(100))), UNPLUG_DEBOUNCE);

        let gen1 = match state.on_sample(snap(AcState::Offline, Some(99))) {
            SampleOutcome::ScheduleDebounce { generation, .. } => generation,
            other => panic!("expected ScheduleDebounce, got {other:?}"),
        };

        // Charger bounces back to Online before debounce expires -> cancels debounce.
        assert_eq!(state.on_sample(snap(AcState::Online, Some(99))), SampleOutcome::CancelDebounce);
        // Stale timer callback from gen1 does nothing.
        assert_eq!(state.on_debounce_fired(gen1, Some(snap(AcState::Offline, Some(99)))), None);

        // Second real unplug gets a fresh generation; old gen1 still cannot trigger it.
        let gen2 = match state.on_sample(snap(AcState::Offline, Some(98))) {
            SampleOutcome::ScheduleDebounce { generation, .. } => generation,
            other => panic!("expected ScheduleDebounce, got {other:?}"),
        };
        assert_ne!(gen1, gen2);
        assert_eq!(state.on_debounce_fired(gen1, Some(snap(AcState::Offline, Some(98)))), None);
        assert_eq!(
            state.on_debounce_fired(gen2, Some(snap(AcState::Offline, Some(97)))),
            Some(PowerTransition::Unplugged { battery_percent: Some(97) })
        );
    }

    #[test]
    fn starting_on_battery_does_not_false_alarm_until_plugged_and_unplugged_again() {
        let mut state = PowerMonitorState::new(Some(snap(AcState::Offline, Some(60))), UNPLUG_DEBOUNCE);

        // Already on battery -> Offline broadcasts do nothing.
        assert_eq!(state.on_sample(snap(AcState::Offline, Some(59))), SampleOutcome::None);

        // Plugging in transitions Offline -> Online.
        assert_eq!(
            state.on_sample(snap(AcState::Online, Some(59))),
            SampleOutcome::Emit(PowerTransition::Replugged { battery_percent: Some(59) })
        );

        // Now unplugging schedules debounce and fires normally.
        let gen1 = match state.on_sample(snap(AcState::Offline, Some(58))) {
            SampleOutcome::ScheduleDebounce { generation, .. } => generation,
            other => panic!("expected ScheduleDebounce, got {other:?}"),
        };
        assert_eq!(
            state.on_debounce_fired(gen1, Some(snap(AcState::Offline, Some(58)))),
            Some(PowerTransition::Unplugged { battery_percent: Some(58) })
        );
    }

    #[test]
    fn unknown_samples_and_online_recheck_suppress_false_alarms() {
        let mut state = PowerMonitorState::new(None, UNPLUG_DEBOUNCE);

        // Unknown readings never arm or fire anything.
        assert_eq!(state.on_sample(snap(AcState::Unknown, None)), SampleOutcome::None);

        // First real reading seeds Online cleanly.
        assert_eq!(state.on_sample(snap(AcState::Online, Some(80))), SampleOutcome::None);

        // Unplug scheduled, Unknown in between is ignored, live re-check sees Online -> suppressed.
        let gen1 = match state.on_sample(snap(AcState::Offline, Some(79))) {
            SampleOutcome::ScheduleDebounce { generation, .. } => generation,
            other => panic!("expected ScheduleDebounce, got {other:?}"),
        };
        assert_eq!(state.on_sample(snap(AcState::Unknown, None)), SampleOutcome::None);
        assert_eq!(state.on_debounce_fired(gen1, Some(snap(AcState::Online, Some(80)))), None);
    }

    #[test]
    fn formats_unplugged_notice_with_and_without_battery_percent() {
        assert_eq!(
            format_unplugged_notice("WORKBOOK", Some(74)),
            ("WORKBOOK unplugged from power".to_owned(), "Running on battery (74% remaining).".to_owned())
        );
        assert_eq!(
            format_unplugged_notice("   ", None),
            ("PC unplugged from power".to_owned(), "Running on battery.".to_owned())
        );
    }
}
