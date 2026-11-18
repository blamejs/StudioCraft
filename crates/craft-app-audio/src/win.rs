//! Windows: the audio sessions on every active playback endpoint, their peak meters, and process
//! loopback capture.
//!
//! - Sessions and meters: `IAudioSessionManager2` and `IAudioMeterInformation`
//!   (https://learn.microsoft.com/en-us/windows/win32/coreaudio/audio-sessions).
//! - Capture: `ActivateAudioInterfaceAsync` with `AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK`
//!   (https://learn.microsoft.com/en-us/windows/win32/api/audioclientactivationparams/ns-audioclientactivationparams-audioclient_process_loopback_params),
//!   which needs Windows build 20348 or later. The format is set to 48 kHz stereo `f32` with the
//!   system's conversion, since such a client cannot report a mix format.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
use wasapi::{
    AudioClient, AudioMeterInformation, DeviceEnumerator, DeviceEventCallbacks, DeviceEventRegistration, Direction, EventCallbacks,
    EventRegistration, SampleType, SessionState, StreamMode, WaveFormat,
};

use crate::{AppAudioError, AppInfo, AppKey, Backend, CAPTURE_WAIT_MS, CHANNELS, CaptureShared, OnData, SAMPLE_RATE, Support, Target, Wake, Watcher};

/// The first Windows build with process loopback capture (Windows Server 2022; Windows 11 is
/// later).
const MIN_BUILD: u32 = 20_348;
/// How far up the parent chain a capture target is looked for.
const MAX_PARENT_DEPTH: usize = 16;
/// Bytes of one captured frame: two `f32` channels.
const FRAME_BYTES: usize = 4 * CHANNELS;
/// The capture buffer holds 250 ms to start with, and grows only for a larger packet.
const START_FRAMES: usize = SAMPLE_RATE as usize / 4;
/// The largest packet a capture accepts, 2 s; a larger one fails the capture.
const MAX_FRAMES: usize = SAMPLE_RATE as usize * 2;
/// The buffer the system keeps for a capture, 20 ms in 100 ns units.
const BUFFER_HNS: i64 = 200_000;
/// Unsignaled waits in a row that return before half their time, after which the capture counts as
/// failed. A signaled wait starts the count over.
const MAX_EARLY_WAITS: u32 = 50;

const UNSUPPORTED: &str = "Capturing one application needs Windows 11 or Windows Server 2022. Desktop audio still captures everything.";

fn failed(e: impl std::fmt::Display) -> AppAudioError {
    AppAudioError::Failed(e.to_string())
}

fn build() -> Option<u32> {
    System::kernel_version()?.trim().parse().ok()
}

pub(crate) struct WinBackend;

impl Backend for WinBackend {
    fn support(&self) -> Support {
        match build() {
            Some(b) if b >= MIN_BUILD => Support::yes(),
            _ => Support::no(UNSUPPORTED),
        }
    }

    fn watch(&self, wake: Wake) -> Result<Box<dyn Watcher>, AppAudioError> {
        // S_FALSE (already set up on this thread) is fine; a real failure shows in the next call.
        let _ = wasapi::initialize_mta();
        let enumerator = DeviceEnumerator::new().map_err(failed)?;
        let mut callbacks = DeviceEventCallbacks::new();
        let w = wake.clone();
        callbacks.set_device_added_callback(move |_| w.wake());
        let w = wake.clone();
        callbacks.set_device_removed_callback(move |_| w.wake());
        let w = wake.clone();
        callbacks.set_device_state_callback(move |_, _| w.wake());
        let w = wake.clone();
        callbacks.set_default_device_callback(move |_, _, _| w.wake());
        // Without endpoint notifications the timed rescan still finds new endpoints.
        let devices = enumerator.register_notification_callback(callbacks).ok();
        Ok(Box::new(WinWatcher {
            enumerator,
            _devices: devices,
            wake,
            sessions: BTreeMap::new(),
            system: System::new(),
            pids: BTreeSet::new(),
            new_session: false,
            scan: 0,
        }))
    }

    fn capture(&self, target: Target, shared: &CaptureShared, on_data: &mut OnData) -> Result<(), AppAudioError> {
        let _ = wasapi::initialize_mta();
        shared.beat();
        let mut client = AudioClient::new_application_loopback_client(target.pid, target.include_children)
            .map_err(|e| AppAudioError::Unsupported(format!("the system did not start the capture: {e}")))?;
        shared.beat();
        let format = WaveFormat::new(32, 32, SampleType::Float, SAMPLE_RATE as usize, CHANNELS, None);
        let mode = StreamMode::EventsShared { autoconvert: true, buffer_duration_hns: BUFFER_HNS };
        client.initialize_client(&format, Direction::Capture, mode, None).map_err(failed)?;
        let event = client.set_get_eventhandle().map_err(failed)?;
        let reader = client.get_audiocaptureclient().map_err(failed)?;
        client.start_stream().map_err(failed)?;
        shared.running();
        let mut bytes = vec![0u8; START_FRAMES * FRAME_BYTES];
        let mut samples = vec![0f32; START_FRAMES * CHANNELS];
        // wasapi reports a failed wait as a timeout. A wait that keeps returning long before its
        // time is a failed one.
        let mut early = 0u32;
        let result = loop {
            if shared.stopping() {
                break Ok(());
            }
            shared.beat();
            let started = Instant::now();
            if event.wait_for_event(CAPTURE_WAIT_MS).is_err() {
                if started.elapsed() < Duration::from_millis(u64::from(CAPTURE_WAIT_MS / 2)) {
                    early += 1;
                    if early >= MAX_EARLY_WAITS {
                        break Err(AppAudioError::Failed("waiting for the application's audio failed".into()));
                    }
                } else {
                    early = 0;
                }
            } else {
                early = 0;
            }
            // Read after a timeout too: a capture whose device or process is gone reports it here.
            let read = drain(&reader, &mut bytes, &mut samples, shared, on_data);
            if let Err(e) = read {
                break Err(e);
            }
        };
        let _ = client.stop_stream();
        result
    }
}

/// Read every packet the system has, convert it and hand it on.
fn drain(
    reader: &wasapi::AudioCaptureClient,
    bytes: &mut Vec<u8>,
    samples: &mut Vec<f32>,
    shared: &CaptureShared,
    on_data: &mut OnData,
) -> Result<(), AppAudioError> {
    loop {
        let frames = usize::try_from(reader.get_next_packet_size().map_err(failed)?.unwrap_or(0)).unwrap_or(usize::MAX);
        if frames == 0 {
            return Ok(());
        }
        if frames > MAX_FRAMES {
            return Err(AppAudioError::Failed(format!("the system sent a packet of {frames} frames")));
        }
        if bytes.len() < frames * FRAME_BYTES {
            bytes.resize(frames * FRAME_BYTES, 0);
            samples.resize(frames * CHANNELS, 0.0);
        }
        let (got, info) = reader.read_from_device(bytes).map_err(failed)?;
        let count = usize::try_from(got).unwrap_or(0).saturating_mul(CHANNELS).min(samples.len());
        if info.flags.silent {
            // A packet marked silent carries no samples to read.
            samples.iter_mut().take(count).for_each(|s| *s = 0.0);
        } else {
            for (s, b) in samples.iter_mut().zip(bytes.as_chunks::<4>().0).take(count) {
                *s = f32::from_le_bytes(*b);
            }
        }
        if let Some(packet) = samples.get(..count) {
            on_data(packet);
            shared.packet();
        }
    }
}

struct Session {
    pid: u32,
    active: bool,
    meter: Option<AudioMeterInformation>,
    _events: Option<EventRegistration>,
    seen: u64,
}

struct WinWatcher {
    enumerator: DeviceEnumerator,
    _devices: Option<DeviceEventRegistration>,
    wake: Wake,
    /// Sessions by their instance id, kept across scans so each registers for state changes once.
    sessions: BTreeMap<String, Session>,
    system: System,
    /// The session processes the process table was last read for.
    pids: BTreeSet<u32>,
    /// A session appeared since the process table was last read. A process that reuses an ended
    /// process's id has a session of its own, so its details are read again.
    new_session: bool,
    scan: u64,
}

impl WinWatcher {
    /// Read the sessions of every active playback endpoint.
    fn read_sessions(&mut self) -> Result<(), AppAudioError> {
        self.scan = self.scan.wrapping_add(1);
        let devices = self.enumerator.get_device_collection(Direction::Render).map_err(failed)?;
        let count = devices.get_nbr_devices().map_err(failed)?;
        for i in 0..count {
            let Ok(device) = devices.get_device_at_index(i) else { continue };
            let Ok(manager) = device.get_iaudiosessionmanager() else { continue };
            let Ok(list) = manager.get_audiosessionenumerator() else { continue };
            let n = list.get_count().unwrap_or(0);
            for j in 0..n {
                let Ok(control) = list.get_session(j) else { continue };
                let Ok(state) = control.get_state() else { continue };
                if state == SessionState::Expired {
                    continue;
                }
                // 0 is the system sounds session, and a session shared by several processes has no
                // single process id.
                let pid = match control.get_process_id() {
                    Ok(pid) if pid != 0 => pid,
                    _ => continue,
                };
                let Ok(id) = control.get_session_instance_identifier() else { continue };
                let scan = self.scan;
                let wake = &self.wake;
                let new_session = &mut self.new_session;
                let session = self.sessions.entry(id).or_insert_with(|| {
                    *new_session = true;
                    let mut events = EventCallbacks::new();
                    let w = wake.clone();
                    events.set_state_callback(move |_| w.wake());
                    Session {
                        pid,
                        active: false,
                        meter: control.get_audiometerinformation().ok(),
                        _events: control.register_session_notification(events).ok(),
                        seen: scan,
                    }
                });
                session.pid = pid;
                session.active = state == SessionState::Active;
                session.seen = scan;
            }
        }
        let scan = self.scan;
        self.sessions.retain(|_, s| s.seen == scan);
        Ok(())
    }

    /// Bring the process table up to date when the session processes changed or a session
    /// appeared.
    fn read_processes(&mut self) {
        let pids: BTreeSet<u32> = self.sessions.values().map(|s| s.pid).collect();
        if pids == self.pids && !self.new_session {
            return;
        }
        // Names and parents of every process, then program paths for the session processes.
        self.system.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::nothing());
        let list: Vec<Pid> = pids.iter().map(|p| Pid::from_u32(*p)).collect();
        self.system.refresh_processes_specifics(ProcessesToUpdate::Some(&list), false, ProcessRefreshKind::nothing().with_exe(UpdateKind::Always));
        self.pids = pids;
        self.new_session = false;
    }

    fn exe_name(&self, pid: u32) -> Option<String> {
        self.system.process(Pid::from_u32(pid)).map(|p| p.name().to_string_lossy().into_owned())
    }

    /// The topmost process above `pid` whose program file has the name `key`.
    fn target(&self, pid: u32, key: &AppKey) -> u32 {
        let mut top = pid;
        for _ in 0..MAX_PARENT_DEPTH {
            let parent = self.system.process(Pid::from_u32(top)).and_then(|p| p.parent()).map(|p| p.as_u32());
            match parent {
                Some(p) if p != top && self.exe_name(p).and_then(|n| AppKey::from_exe(&n)).is_some_and(|k| k.name == key.name) => top = p,
                _ => break,
            }
        }
        top
    }
}

impl Watcher for WinWatcher {
    fn scan(&mut self) -> Result<Vec<AppInfo>, AppAudioError> {
        self.read_sessions()?;
        self.read_processes();
        // Applications by program file name and path, and the session each one's target comes
        // from: a playing session before a silent one, then the lowest process id.
        let mut apps: BTreeMap<(String, String), AppInfo> = BTreeMap::new();
        let mut chosen: BTreeMap<(String, String), (bool, u32)> = BTreeMap::new();
        for s in self.sessions.values() {
            let Some(exe) = self.exe_name(s.pid) else { continue };
            let Some(key) = AppKey::from_exe(&exe) else { continue };
            let path = self.system.process(Pid::from_u32(s.pid)).and_then(|p| p.exe()).map(|p| p.display().to_string()).unwrap_or_default();
            let target = self.target(s.pid, &key);
            let name = exe.rsplit_once('.').map_or(exe.as_str(), |(stem, _)| stem).to_string();
            let group = (key.name.clone(), path.to_lowercase());
            let better = chosen.get(&group).is_none_or(|&(active, pid)| (s.active, std::cmp::Reverse(s.pid)) > (active, std::cmp::Reverse(pid)));
            if better {
                chosen.insert(group.clone(), (s.active, s.pid));
            }
            let entry =
                apps.entry(group).or_insert_with(|| AppInfo { key, name, path, pids: Vec::new(), target_pid: target, playing: false, slot: None });
            if !entry.pids.contains(&s.pid) {
                entry.pids.push(s.pid);
            }
            entry.playing |= s.active;
            // A capture takes one process tree. When separate instances of a program run, it
            // takes the playing one, and a source captures again when another instance starts
            // playing instead.
            if better {
                entry.target_pid = target;
            }
        }
        Ok(apps
            .into_values()
            .map(|mut a| {
                a.pids.sort_unstable();
                a
            })
            .collect())
    }

    fn peaks(&mut self, apps: &[AppInfo], out: &mut dyn FnMut(usize, f32)) {
        for (i, app) in apps.iter().enumerate() {
            let peak = self
                .sessions
                .values()
                .filter(|s| app.pids.contains(&s.pid))
                .filter_map(|s| s.meter.as_ref().and_then(|m| m.get_peak_value().ok()))
                .fold(None, |acc: Option<f32>, p| Some(acc.map_or(p, |a| a.max(p))));
            if let Some(p) = peak {
                out(i, p);
            }
        }
    }
}
