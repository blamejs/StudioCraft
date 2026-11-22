//! Hardware acceleration: the graphics adapter the window draws with, the choices the Settings
//! window lists, and what the window opens after its device was lost.
//!
//! The window's device belongs to the window and cannot be replaced while it runs. After a loss
//! the app saves everything and starts again; [`startup`] then opens the saved choice, or a
//! software adapter once the device was lost [`RecoveryPolicy`]'s number of times within its
//! window.

use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use studiocraft_render::device::{self, AdapterKey, Candidate, Notice, Preference, Recovery, RecoveryPolicy};

use crate::i18n::Msg;

/// The device losses the preferences keep: as many as move drawing to software.
pub const KEPT_LOSSES: usize = 3;

/// The time now in whole seconds since 1970, the unit of `Prefs::graphics_losses`.
pub fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// The adapter preference the window opens with: `saved`, or [`Preference::Software`] when the
/// device was lost [`KEPT_LOSSES`] times within the recovery window as of `now`, with the notice
/// that says so. Losses older than the window, or later than `now` (a clock set back), do not
/// count.
pub fn startup(saved: &Preference, losses: &[u64], now: u64) -> (Preference, Option<Notice>) {
    let mut recovery = Recovery::new(saved.clone(), policy());
    let mut notice = None;
    for t in recent(losses, now) {
        if let Some(n) = recovery.lost(Duration::from_secs(t)).notice {
            notice = Some(n);
        }
    }
    (recovery.preference().clone(), notice)
}

/// How many losses within the recovery window move drawing to software, and that window.
fn policy() -> RecoveryPolicy {
    RecoveryPolicy { max_losses: KEPT_LOSSES as u32, ..RecoveryPolicy::default() }
}

/// The losses in `losses` within the recovery window as of `now`. Losses later than `now` (a
/// clock set back) do not count.
fn recent(losses: &[u64], now: u64) -> impl Iterator<Item = u64> + '_ {
    let window = policy().window.as_secs();
    losses.iter().copied().filter(move |t| *t <= now && now - *t <= window)
}

/// Whether the window's device was lost as many times as [`startup`] needs to move drawing to
/// software, within the recovery window as of `now`.
pub fn repeated_losses(losses: &[u64], now: u64) -> bool {
    recent(losses, now).count() >= KEPT_LOSSES
}

/// Record a device loss at `now`, keeping the newest [`KEPT_LOSSES`] in time order. Losses later
/// than `now` (a clock set back) are dropped, so they cannot push out the new ones.
pub fn record_loss(losses: &mut Vec<u64>, now: u64) {
    losses.retain(|t| *t <= now);
    losses.push(now);
    losses.sort_unstable();
    let extra = losses.len().saturating_sub(KEPT_LOSSES);
    losses.drain(..extra);
}

/// The window's wgpu setup: the instance over the backends `craft-gpu-device` uses (with
/// `WGPU_BACKEND` still honored), then the platform's start-up choices from `craft_gpu::configure`,
/// then the adapter `preference` asks for, and a device with that adapter's own limits, so an
/// adapter below wgpu's default limits (an older or software one) opens. What the user should be
/// told about the choice is put in `notices` when the window opens.
///
/// On Windows `configure` sets the DirectX 12 shader compiler and replaces the backends with the
/// ones its DirectX 12 probe found usable (DirectX 12, or OpenGL where DirectX 12 has no usable
/// GPU), so a driver that crashes while a backend starts is never loaded; `WGPU_BACKEND` still
/// wins there. On Intel Macs it leaves out wgpu's indirect-call validation. The adapter choice
/// stays the app's on every platform: it is set after `configure`, and the saved Graphics adapter
/// setting names the adapter.
pub fn wgpu_setup(preference: Preference, notices: Arc<OnceLock<Vec<Notice>>>) -> egui_wgpu::WgpuSetup {
    let mut setup = egui_wgpu::WgpuSetupCreateNew::without_display_handle();
    // Before `configure`, whose DirectX 12 probe checks each adapter against these limits.
    setup.device_descriptor = Arc::new(|adapter: &wgpu::Adapter| wgpu::DeviceDescriptor {
        label: Some("studiocraft window"),
        required_limits: adapter.limits(),
        ..Default::default()
    });
    let desc = device::native_descriptor();
    setup.instance_descriptor.backends = wgpu::Backends::from_env().unwrap_or(desc.backends);
    setup.instance_descriptor.flags = desc.flags;
    setup.instance_descriptor.backend_options = desc.backend_options;
    craft_gpu::configure(&mut setup);
    setup.native_adapter_selector = Some(Arc::new(move |adapters: &[wgpu::Adapter], surface: Option<&wgpu::Surface<'_>>| {
        let (adapter, selection) = device::pick(adapters, surface, &preference)?;
        let _ = notices.set(selection.notices);
        Ok(adapter)
    }));
    setup.into()
}

/// The adapter choices the Settings window lists: the automatic choice, power saving, software,
/// and each adapter in `listed` once.
pub fn choices(listed: &[Candidate]) -> Vec<Preference> {
    let mut out = vec![Preference::Auto, Preference::LowPower, Preference::Software];
    for c in listed {
        let p = Preference::Adapter(AdapterKey::of(c));
        if !out.contains(&p) {
            out.push(p);
        }
    }
    out
}

/// The name of an adapter choice in the interface language. A named adapter that is not listed
/// now says so.
pub fn label(choice: &Preference, listed: &[Candidate]) -> String {
    match choice {
        Preference::Auto => tl!("Automatic (fastest)").to_string(),
        Preference::LowPower => tl!("Power saving").to_string(),
        Preference::Software => tl!("Software (slower)").to_string(),
        Preference::Adapter(key) => {
            let name = format!("{} ({})", key.name, key.backend);
            if listed.iter().any(|c| key.matches(c)) { name } else { craft_i18n::fmt(tl!("{device} (not connected)"), &[("device", &name)]) }
        }
    }
}

/// A notice about the adapter choice, translated when it is shown.
pub fn notice_msg(n: &Notice) -> Msg {
    let n = studiocraft_studio::gpu_notice(n);
    Msg::new(n.template, n.args)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: u64 = 60;

    #[test]
    fn three_recent_losses_open_in_software_and_old_ones_do_not() {
        let now = 100_000;
        assert_eq!(startup(&Preference::Auto, &[], now), (Preference::Auto, None));
        let recent = [now - 5 * MIN, now - 2 * MIN, now];
        assert_eq!(startup(&Preference::LowPower, &recent, now), (Preference::Software, Some(Notice::SoftwareAfterLosses(3))));
        // two losses are not enough
        assert_eq!(startup(&Preference::LowPower, &recent[1..], now).0, Preference::LowPower);
        // losses more than ten minutes before now no longer count
        assert_eq!(startup(&Preference::Auto, &recent, now + 11 * MIN).0, Preference::Auto);
        // a loss stamped after now (the clock was set back) does not count
        assert_eq!(startup(&Preference::Auto, &[now - MIN, now, now + MIN], now).0, Preference::Auto);
        // software asked for stays software, without a notice
        assert_eq!(startup(&Preference::Software, &recent, now), (Preference::Software, None));
    }

    #[test]
    fn the_newest_losses_are_kept() {
        let mut losses = Vec::new();
        for t in [10, 20, 30, 30, 40, 50] {
            record_loss(&mut losses, t);
        }
        assert_eq!(losses, [30, 40, 50]);
        let mut hostile = vec![u64::MAX; 1000];
        record_loss(&mut hostile, 1);
        assert_eq!(hostile, [1], "losses after now (a clock set back) are dropped");
        // so three new losses after the clock moved back still open in software
        let mut losses = vec![9_000, 9_100, 9_200];
        for t in [1_000, 1_010, 1_020] {
            record_loss(&mut losses, t);
        }
        assert_eq!(losses, [1_000, 1_010, 1_020]);
        assert_eq!(startup(&Preference::Auto, &losses, 1_030).0, Preference::Software);
    }

    fn candidate(name: &str, backend: wgpu::Backend) -> Candidate {
        Candidate {
            name: name.into(),
            backend,
            device_type: wgpu::DeviceType::DiscreteGpu,
            vendor: 0x10de,
            device: 0x2684,
            pci_bus_id: "0000:01:00.0".into(),
            driver: String::new(),
            driver_info: String::new(),
            features: 10,
            usable: true,
        }
    }

    #[test]
    fn the_choices_list_each_adapter_once_and_name_a_missing_one() {
        let gpu = candidate("GPU", wgpu::Backend::Vulkan);
        let listed = vec![gpu.clone(), gpu.clone(), candidate("GPU", wgpu::Backend::Dx12)];
        let c = choices(&listed);
        assert_eq!(c.len(), 5, "{c:?}");
        assert_eq!(c[..3], [Preference::Auto, Preference::LowPower, Preference::Software]);
        assert_eq!(label(&c[3], &listed), "GPU (vulkan)");
        assert_eq!(label(&c[0], &listed), tl!("Automatic (fastest)"));
        let gone = Preference::Adapter(AdapterKey::of(&candidate("Old GPU", wgpu::Backend::Metal)));
        assert_eq!(label(&gone, &listed), craft_i18n::fmt(tl!("{device} (not connected)"), &[("device", "Old GPU (metal)")]));
    }

    #[test]
    fn the_window_setup_picks_through_the_preference_and_keeps_its_notices() {
        let mut desc = wgpu::InstanceDescriptor::new_without_display_handle();
        desc.backends = wgpu::Backends::NOOP;
        desc.backend_options.noop = wgpu::NoopBackendOptions { enable: true, ..Default::default() };
        let instance = wgpu::Instance::new(desc);
        let adapters = pollster::block_on(instance.enumerate_adapters(wgpu::Backends::NOOP));
        let notices = Arc::new(OnceLock::new());
        let egui_wgpu::WgpuSetup::CreateNew(setup) = wgpu_setup(Preference::Auto, Arc::clone(&notices)) else { panic!("a new setup") };
        let select = setup.native_adapter_selector.unwrap();
        assert!(select(&adapters, None).is_ok());
        // the noop adapter is a software one, which Auto did not ask for
        assert_eq!(notices.get(), Some(&vec![Notice::SoftwareOnly]));
        assert!(select(&[], None).is_err(), "no adapter is an error the window reports");
        // the device asks for the adapter's own limits, so an adapter below wgpu's defaults opens
        let descriptor = (setup.device_descriptor)(&adapters[0]);
        assert_eq!(descriptor.required_limits, adapters[0].limits());
        assert!(pollster::block_on(adapters[0].request_device(&descriptor)).is_ok());
    }
}
