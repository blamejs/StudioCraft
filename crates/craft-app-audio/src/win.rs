//! Windows: the audio sessions on every active playback endpoint, their peak meters, and process
//! loopback capture.
//!
//! - Sessions and meters: `IAudioSessionManager2` and `IAudioMeterInformation`
//!   (https://learn.microsoft.com/en-us/windows/win32/coreaudio/audio-sessions).
//! - Capture: `ActivateAudioInterfaceAsync` with `AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK`
//!   (https://learn.microsoft.com/en-us/windows/win32/api/audioclientactivationparams/ns-audioclientactivationparams-audioclient_process_loopback_params),
//!   which needs Windows build 20348 or later. The format is set to 48 kHz stereo `f32` with the
//!   system's conversion, since such a client cannot report a mix format.
//! - Packet times: `IAudioCaptureClient::GetBuffer` gives each packet's device position, its
//!   performance counter time and its flags
//!   (https://learn.microsoft.com/en-us/windows/win32/api/audioclient/nf-audioclient-iaudiocaptureclient-getbuffer).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
use wasapi::{
    AudioClient, AudioMeterInformation, DeviceEnumerator, DeviceEventCallbacks, DeviceEventRegistration, Direction, EventCallbacks,
    EventRegistration, SampleType, SessionState, StreamMode, WasapiError, WaveFormat,
};

use crate::{
    AppAudioError, AppInfo, AppKey, Backend, CAPTURE_WAIT_MS, CHANNELS, CaptureShared, OnData, PacketInfo, SAMPLE_RATE, Support, Target, Wake,
    Watcher,
};

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
/// A meter reading above this (about -60 dBFS) counts as sound.
const LOUD: f32 = 0.001;
/// A session that read as sound within this many scans (about 3 s) counts as making sound when an
/// instance is chosen.
const LOUD_SCANS: u64 = 3;
/// `E_ACCESSDENIED`.
const E_ACCESSDENIED: i32 = i32::from_ne_bytes(0x8007_0005_u32.to_ne_bytes());

const UNSUPPORTED: &str = crate::NEEDS_NEWER_WINDOWS;

fn failed(e: impl std::fmt::Display) -> AppAudioError {
    AppAudioError::Failed(e.to_string())
}

/// The Windows build number, read once: it does not change while the app runs.
fn build() -> Option<u32> {
    static BUILD: OnceLock<Option<u32>> = OnceLock::new();
    *BUILD.get_or_init(|| System::kernel_version()?.trim().parse().ok())
}

/// Whether process `pid` exists now.
fn process_exists(pid: u32) -> bool {
    let mut system = System::new();
    let pid = Pid::from_u32(pid);
    system.refresh_processes_specifics(ProcessesToUpdate::Some(&[pid]), true, ProcessRefreshKind::nothing());
    system.process(pid).is_some()
}

/// What an activation failure means: the system refused, the target has exited, or another
/// failure.
fn activation_error(pid: u32, e: &WasapiError) -> AppAudioError {
    if let WasapiError::Windows(w) = e
        && w.code().0 == E_ACCESSDENIED
    {
        return AppAudioError::Denied(format!("the system refused to capture process {pid}: {e}"));
    }
    if !process_exists(pid) {
        return AppAudioError::NotRunning(format!("process {pid}"));
    }
    AppAudioError::Failed(format!("the system did not start the capture: {e}"))
}

/// The error of a call that sets up a capture after activation: a refusal is `Denied`, as in
/// [`activation_error`], so the source waits as long as for a refused activation.
fn setup_error(e: &WasapiError) -> AppAudioError {
    if let WasapiError::Windows(w) = e
        && w.code().0 == E_ACCESSDENIED
    {
        return AppAudioError::Denied(format!("the system refused to capture: {e}"));
    }
    failed(e)
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
            parent_targets: false,
            targets: BTreeMap::new(),
            scan: 0,
        }))
    }

    fn capture(&self, target: Target, shared: &CaptureShared, on_data: &mut OnData) -> Result<(), AppAudioError> {
        let _ = wasapi::initialize_mta();
        shared.beat();
        // `true` is PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE. The only other mode,
        // `false`, records every process except this tree.
        let mut client = AudioClient::new_application_loopback_client(target.pid, true).map_err(|e| activation_error(target.pid, &e))?;
        shared.beat();
        let format = WaveFormat::new(32, 32, SampleType::Float, SAMPLE_RATE as usize, CHANNELS, None);
        let mode = StreamMode::EventsShared { autoconvert: true, buffer_duration_hns: BUFFER_HNS };
        client.initialize_client(&format, Direction::Capture, mode, None).map_err(|e| setup_error(&e))?;
        let event = client.set_get_eventhandle().map_err(|e| setup_error(&e))?;
        let reader = client.get_audiocaptureclient().map_err(|e| setup_error(&e))?;
        client.start_stream().map_err(|e| setup_error(&e))?;
        shared.started();
        let mut bytes = vec![0u8; START_FRAMES * FRAME_BYTES];
        let mut samples = vec![0f32; START_FRAMES * CHANNELS];
        let mut flowing = false;
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
            let read = drain(&reader, &mut bytes, &mut samples, shared, on_data, &mut flowing);
            if let Err(e) = read {
                break Err(e);
            }
        };
        let _ = client.stop_stream();
        result
    }
}

/// Read every packet the system has, convert it and hand it on. The first packet marks the
/// capture running.
fn drain(
    reader: &wasapi::AudioCaptureClient,
    bytes: &mut Vec<u8>,
    samples: &mut Vec<f32>,
    shared: &CaptureShared,
    on_data: &mut OnData,
    flowing: &mut bool,
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
            let info =
                PacketInfo::from_system(info.index, info.timestamp, info.flags.data_discontinuity, info.flags.timestamp_error, info.flags.silent);
            on_data(packet, info);
            shared.packet(&info);
            if !*flowing {
                *flowing = true;
                shared.running();
            }
        }
    }
}

struct Session {
    pid: u32,
    active: bool,
    meter: Option<AudioMeterInformation>,
    _events: Option<EventRegistration>,
    seen: u64,
    /// The capture target of this session's process tree, from the last scan.
    target: u32,
    /// The scan in which the meter last read as sound, 0 when it never has.
    loud: u64,
}

impl Session {
    /// Read the meter, and remember a reading above [`LOUD`] as scan `scan`.
    fn read(&mut self, scan: u64) -> Option<f32> {
        let p = self.meter.as_ref().and_then(|m| m.get_peak_value().ok())?;
        if p > LOUD {
            self.loud = scan;
        }
        Some(p)
    }
}

/// A program file name and its lower-case path: one listed application.
type Group = (String, String);

struct WinWatcher {
    enumerator: DeviceEnumerator,
    _devices: Option<DeviceEventRegistration>,
    wake: Wake,
    /// Sessions by their instance id, kept across scans so each registers for state changes once.
    sessions: BTreeMap<String, Session>,
    system: System,
    /// The session processes the program paths were last read for.
    pids: BTreeSet<u32>,
    /// A session appeared since the process table was last read. A process that reuses an ended
    /// process's id has a session of its own, so its details are read again.
    new_session: bool,
    /// The last scan found a capture target above a session's own process.
    parent_targets: bool,
    /// The process tree each application's capture took in the last scan.
    targets: BTreeMap<Group, u32>,
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
                        target: pid,
                        loud: 0,
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
    /// appeared, and on every scan while a capture target is a parent process: a parent can exit
    /// while its child keeps the session, and Windows can give its id to a new process. Each read
    /// is one system snapshot of the processes; a parent whose id is reused within one scan
    /// (about 1 s) is still read as the parent until the next one.
    fn read_processes(&mut self) {
        let pids: BTreeSet<u32> = self.sessions.values().map(|s| s.pid).collect();
        let changed = pids != self.pids || self.new_session;
        if !changed && !self.parent_targets {
            return;
        }
        // Names and parents of every process, removing the ones that ended.
        self.system.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::nothing());
        if changed {
            // Program paths for the session processes.
            let list: Vec<Pid> = pids.iter().map(|p| Pid::from_u32(*p)).collect();
            self.system.refresh_processes_specifics(
                ProcessesToUpdate::Some(&list),
                false,
                ProcessRefreshKind::nothing().with_exe(UpdateKind::Always),
            );
        }
        self.pids = pids;
        self.new_session = false;
    }

    fn exe_name(&self, pid: u32) -> Option<String> {
        self.system.process(Pid::from_u32(pid)).map(|p| p.name().to_string_lossy().into_owned())
    }

    /// The parent of `pid`, when the table has one that started no later than `pid` did. A
    /// process that started later holds a reused id, not the parent.
    fn parent(&self, pid: u32) -> Option<u32> {
        let child = self.system.process(Pid::from_u32(pid))?;
        let parent = child.parent()?.as_u32();
        let p = self.system.process(Pid::from_u32(parent))?;
        (parent != pid && p.start_time() <= child.start_time()).then_some(parent)
    }

    /// The topmost process above `pid` whose program file has the name `key`.
    fn target(&self, pid: u32, key: &AppKey) -> u32 {
        let mut top = pid;
        for _ in 0..MAX_PARENT_DEPTH {
            match self.parent(top) {
                Some(p) if self.exe_name(p).and_then(|n| AppKey::from_exe(&n)).is_some_and(|k| k.name == key.name) => top = p,
                _ => break,
            }
        }
        top
    }

    /// The first of `targets` above `pid` in its parent chain.
    fn inside(&self, pid: u32, targets: &BTreeMap<u32, String>) -> Option<String> {
        let mut at = pid;
        for _ in 0..MAX_PARENT_DEPTH {
            at = self.parent(at)?;
            if let Some(choice) = targets.get(&at) {
                return Some(choice.clone());
            }
        }
        None
    }
}

/// One process tree of an application: whether it made sound lately, whether a stream runs.
#[derive(Clone, Copy, Default)]
struct Tree {
    loud: bool,
    active: bool,
}

/// The tree a capture of one application takes. The tree taken last time stays while it makes
/// sound, or while no other tree does, so two instances whose streams start and stop do not make
/// the source capture again and again. Otherwise the tree that made sound lately, then one with a
/// running stream, then the lowest process id.
fn choose(trees: &BTreeMap<u32, Tree>, previous: Option<u32>) -> Option<u32> {
    if let Some(p) = previous
        && let Some(t) = trees.get(&p)
        && (t.loud || !trees.iter().any(|(pid, other)| *pid != p && other.loud))
    {
        return Some(p);
    }
    trees.iter().max_by_key(|(pid, t)| (t.loud, t.active, std::cmp::Reverse(**pid))).map(|(pid, _)| *pid)
}

impl Watcher for WinWatcher {
    fn scan(&mut self) -> Result<Vec<AppInfo>, AppAudioError> {
        self.read_sessions()?;
        self.read_processes();
        // Each session's application, path and capture target.
        let mut rows: Vec<(String, Group, AppKey, String, String, u32)> = Vec::new();
        for (id, s) in &self.sessions {
            let Some(exe) = self.exe_name(s.pid) else { continue };
            let Some(key) = AppKey::from_exe(&exe) else { continue };
            let path = self.system.process(Pid::from_u32(s.pid)).and_then(|p| p.exe()).map(|p| p.display().to_string()).unwrap_or_default();
            let target = self.target(s.pid, &key);
            let name = exe.rsplit_once('.').map_or(exe.as_str(), |(stem, _)| stem).to_string();
            rows.push((id.clone(), (key.name.clone(), path.to_lowercase()), key, name, path, target));
        }
        let scan = self.scan;
        let mut parent_targets = false;
        let mut trees: BTreeMap<Group, BTreeMap<u32, Tree>> = BTreeMap::new();
        for (id, group, _, _, _, target) in &rows {
            let Some(s) = self.sessions.get_mut(id) else { continue };
            s.target = *target;
            parent_targets |= *target != s.pid;
            trees.entry(group.clone()).or_default().entry(*target).or_default();
        }
        // Where an application runs as separate instances, each instance's meter is read now, so
        // the choice goes by sound and not only by whether a stream runs.
        for (id, group, _, _, _, target) in &rows {
            let several = trees.get(group).is_some_and(|t| t.len() > 1);
            let Some(s) = self.sessions.get_mut(id) else { continue };
            if several {
                s.read(scan);
            }
            if let Some(t) = trees.get_mut(group).and_then(|t| t.get_mut(target)) {
                t.loud |= s.loud != 0 && scan.wrapping_sub(s.loud) <= LOUD_SCANS;
                t.active |= s.active;
            }
        }
        self.parent_targets = parent_targets;
        let previous = std::mem::take(&mut self.targets);
        let mut apps: BTreeMap<Group, AppInfo> = BTreeMap::new();
        for (id, group, key, name, path, _) in rows {
            let Some(s) = self.sessions.get(&id) else { continue };
            let target = match self.targets.get(&group) {
                Some(t) => *t,
                None => {
                    let t = trees.get(&group).and_then(|t| choose(t, previous.get(&group).copied())).unwrap_or(s.pid);
                    self.targets.insert(group.clone(), t);
                    t
                }
            };
            let entry = apps.entry(group).or_insert_with(|| AppInfo {
                key,
                name,
                path,
                pids: Vec::new(),
                target_pid: target,
                playing: false,
                within: None,
                slot: None,
                key_id: 0,
            });
            if !entry.pids.contains(&s.pid) {
                entry.pids.push(s.pid);
            }
            entry.playing |= s.active;
        }
        // An application whose target runs under another listed application's target has that
        // tree recorded by the other application's capture too.
        let by_target: BTreeMap<u32, String> = apps.values().map(|a| (a.target_pid, a.choice())).collect();
        let mut apps: Vec<AppInfo> = apps.into_values().collect();
        for a in &mut apps {
            a.pids.sort_unstable();
            a.within = self.inside(a.target_pid, &by_target);
        }
        Ok(apps)
    }

    fn peaks(&mut self, apps: &[AppInfo], out: &mut dyn FnMut(usize, f32)) {
        // A lane's meter reads the sessions of the process tree its capture records, so the
        // meter and the captured audio come from the same instance.
        let scan = self.scan;
        for (i, app) in apps.iter().enumerate() {
            let peak = self
                .sessions
                .values_mut()
                .filter(|s| s.target == app.target_pid && app.pids.contains(&s.pid))
                .filter_map(|s| s.read(scan))
                .fold(None, |acc: Option<f32>, p| Some(acc.map_or(p, |a| a.max(p))));
            if let Some(p) = peak {
                out(i, p);
            }
        }
    }
}
