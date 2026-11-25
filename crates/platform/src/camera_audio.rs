//! A camera's own microphone: the audio input that belongs to the same physical device as a camera.
//!
//! A camera with a built-in microphone shows up as a camera and, separately, as an audio input.
//! [`microphone_for`] finds that input for a camera: first by the system's id for the physical
//! device both belong to (its container id on Windows, its USB serial on Linux), then by name.
//!
//! On Windows the container ids come from the system's device information, and the root
//! container, which every built-in device shares, does not count as a match. On Linux a camera's
//! node is found under `/dev/v4l/by-id`, and a PipeWire input's node name carries the same USB
//! serial. On macOS only the names are compared.

use std::sync::atomic::{AtomicU8, Ordering};
use std::thread::Thread;
use std::time::{Duration, Instant};

use crate::audio::{self, DeviceInfo, DeviceKind};
use crate::camera::{self, CameraInfo, best_match};

/// An audio input, with the id of the physical device it belongs to when the system reports one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Input {
    pub device: DeviceInfo,
    pub container: Option<String>,
}

/// The most names compared, and the most devices read from the system for one search.
const MAX_DEVICES: usize = 256;

/// The words of a device name: lowercase runs of letters and digits.
fn words(name: &str) -> Vec<String> {
    name.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(str::to_lowercase).collect()
}

/// How closely `input`'s name matches a camera named by `camera` words: 0 for the same name,
/// 1 when the input's name holds the camera's whole name, `None` otherwise.
fn name_rank(camera: &[String], input: &str) -> Option<u8> {
    if camera.is_empty() {
        return None;
    }
    let input = words(input);
    if input == camera {
        return Some(0);
    }
    input.windows(camera.len()).any(|w| w == camera).then_some(1)
}

/// Words that name a microphone in an input's name, lowercase.
const MICROPHONE_WORDS: [&str; 6] = ["microphone", "mic", "mikrofon", "micro", "micrófono", "microfone"];

/// Whether `name` has a word that names a microphone.
fn names_a_microphone(name: &str) -> bool {
    words(name).iter().any(|w| MICROPHONE_WORDS.contains(&w.as_str()))
}

/// The one index in `ranked` (index and rank) with the lowest rank; `None` when two share it.
fn unique_best<R: Ord + Copy>(ranked: &[(usize, R)]) -> Option<usize> {
    let best = ranked.iter().map(|&(_, r)| r).min()?;
    let mut at_best = ranked.iter().filter(|&&(_, r)| r == best);
    match (at_best.next(), at_best.next()) {
        (Some(&(i, _)), None) => Some(i),
        _ => None,
    }
}

/// The index in `inputs` of the microphone of the camera named `camera_name`, whose physical
/// device has the id `camera_container` when the system reports one.
///
/// When inputs share the camera's container id, one of them is chosen: the only one, or else
/// the one input whose name names a microphone, then by how well its name matches the camera's
/// name. When two of them are equally good, none is chosen. Without an input in the container,
/// the input whose name is the camera's name is chosen, or else the input whose name holds the
/// camera's name. When two inputs match a name equally well, none is chosen. When the camera's
/// container id is known, inputs in another container are not matched by name.
pub fn microphone_index(camera_name: &str, camera_container: Option<&str>, inputs: &[Input]) -> Option<usize> {
    let inputs = inputs.get(..inputs.len().min(MAX_DEVICES)).unwrap_or_default();
    let camera = words(camera_name);
    let want = camera_container.filter(|c| !c.is_empty());
    if let Some(want) = want {
        let same: Vec<(usize, (bool, u8))> = inputs
            .iter()
            .enumerate()
            .filter(|(_, i)| i.container.as_deref().is_some_and(|c| c.eq_ignore_ascii_case(want)))
            .map(|(n, i)| (n, (!names_a_microphone(&i.device.name), name_rank(&camera, &i.device.name).unwrap_or(2))))
            .collect();
        if !same.is_empty() {
            return unique_best(&same);
        }
    }
    // When the camera's device is known, an input of another known device is not its microphone.
    let ranked: Vec<(usize, u8)> = inputs
        .iter()
        .enumerate()
        .filter(|(_, input)| want.is_none() || input.container.as_deref().is_none_or(str::is_empty))
        .filter_map(|(i, input)| Some((i, name_rank(&camera, &input.device.name)?)))
        .collect();
    unique_best(&ranked)
}

/// The audio input that is the microphone of the camera stored in a camera source's settings as
/// `key` ([`CameraInfo::key`]). `None` when the camera is not plugged in or no input matches it.
/// It lists the system's cameras and audio inputs, so it runs off the window's thread.
pub fn microphone_for(key: &str) -> Option<DeviceInfo> {
    let cameras = camera::cameras();
    // The camera the capture of a source with this key streams.
    let cam = best_match(&cameras, &CameraInfo::parse_key(key)).and_then(|i| cameras.get(i))?;
    let devices = audio::devices(DeviceKind::Input);
    let devices = devices.get(..devices.len().min(MAX_DEVICES)).unwrap_or_default();
    let containers = imp::input_containers(devices);
    let inputs: Vec<Input> =
        devices.iter().enumerate().map(|(i, d)| Input { device: d.clone(), container: containers.get(i).cloned().flatten() }).collect();
    let i = microphone_index(&cam.name, imp::camera_container(cam).as_deref(), &inputs)?;
    inputs.into_iter().nth(i).map(|i| i.device)
}

/// The device node a Linux camera id names: the id itself (`/dev/video0`), or the path at the end
/// of a description (`Video4Linux Device @ /dev/video0`).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn node_path(id: &str) -> Option<&str> {
    id.find("/dev/").and_then(|i| id.get(i..)).map(str::trim_end)
}

/// The USB serial in a `/dev/v4l/by-id` link name (`usb-<serial>-video-index<n>`).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn by_id_serial(link: &str) -> Option<&str> {
    let (serial, index) = link.rsplit_once("-video-index")?;
    (!serial.is_empty() && !index.is_empty() && index.bytes().all(|b| b.is_ascii_digit())).then_some(serial)
}

/// The USB serial in a PipeWire input node name (`alsa_input.usb-<serial>-<interface>.<profile>`).
/// Only the profile after the last period is removed, so a serial may contain periods.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn node_serial(node: &str) -> Option<&str> {
    let rest = node.strip_prefix("alsa_input.")?;
    let device = rest.rsplit_once('.').map_or(rest, |(d, _)| d);
    let serial = match device.rsplit_once('-') {
        Some((serial, iface)) if !iface.is_empty() && iface.bytes().all(|b| b.is_ascii_digit()) => serial,
        _ => device,
    };
    (!serial.is_empty()).then_some(serial)
}

#[cfg(windows)]
mod imp {
    use std::sync::Arc;
    use std::time::Duration;

    use crossbeam_queue::ArrayQueue;
    use windows::Devices::Enumeration::{DeviceClass, DeviceInformation, DeviceInformationUpdate, DeviceWatcher};
    use windows::Foundation::{IPropertyValue, TypedEventHandler};
    use windows::core::{HSTRING, IInspectable, Interface};
    use windows_collections::IIterable;

    use super::{Listing, MAX_DEVICES};
    use crate::audio::DeviceInfo;
    use crate::camera::CameraInfo;

    /// How long the system may take to list one class of devices.
    const LISTING: Duration = Duration::from_secs(3);

    /// The container every built-in device belongs to.
    const ROOT: &str = "00000000-0000-0000-ffff-ffffffffffff";

    /// The device property that holds a device's container id.
    const CONTAINER_ID: &str = "System.Devices.ContainerId";

    /// A device the system lists: its id, its name and its container id.
    type Listed = (String, String, Option<String>);

    /// The container id of a device, lowercase; `None` for the root container.
    fn container(info: &DeviceInformation) -> Option<String> {
        let value = info.Properties().ok()?.Lookup(&HSTRING::from(CONTAINER_ID)).ok()?;
        let guid = value.cast::<IPropertyValue>().ok()?.GetGuid().ok()?;
        let id = format!("{guid:?}").to_ascii_lowercase();
        (id != ROOT).then_some(id)
    }

    /// The devices of `class` with their container ids, read by a device watcher until it has
    /// listed them all. The watcher's callback sends each device through a bounded lock-free queue
    /// and records the end of the listing in an atomic, and this thread takes the devices out once
    /// the listing is complete. Empty when they cannot be listed.
    fn listed(class: DeviceClass) -> Vec<Listed> {
        let Ok(_com) = comedy::com::ComApartmentScope::init_mta() else { return Vec::new() };
        let Ok(filter) = DeviceInformation::GetAqsFilterFromDeviceClass(class) else { return Vec::new() };
        let wanted = IIterable::<HSTRING>::from(vec![HSTRING::from(CONTAINER_ID)]);
        let Ok(watcher) = DeviceInformation::CreateWatcherAqsFilterAndAdditionalProperties(&filter, &wanted) else { return Vec::new() };
        let found = Arc::new(ArrayQueue::<Listed>::new(MAX_DEVICES));
        let listing = Arc::new(Listing::new());
        let done = Arc::clone(&listing);
        let ended = Arc::clone(&listing);
        let into = Arc::clone(&found);
        // The watcher lists the devices only to an app that handles the added, removed and
        // updated events.
        let handlers = watcher
            .Added(&TypedEventHandler::<DeviceWatcher, DeviceInformation>::new(move |_, info| {
                // A device past the queue's bound is not read; the search compares at most
                // MAX_DEVICES of them.
                if !into.is_full()
                    && let Ok(info) = info.ok()
                {
                    let id = info.Id().map(|s| s.to_string()).unwrap_or_default();
                    let name = info.Name().map(|s| s.to_string()).unwrap_or_default();
                    let _ = into.push((id, name, container(info)));
                }
                Ok(())
            }))
            .and_then(|_| watcher.Removed(&TypedEventHandler::<DeviceWatcher, DeviceInformationUpdate>::new(|_, _| Ok(()))))
            .and_then(|_| watcher.Updated(&TypedEventHandler::<DeviceWatcher, DeviceInformationUpdate>::new(|_, _| Ok(()))))
            .and_then(|_| {
                watcher.EnumerationCompleted(&TypedEventHandler::<DeviceWatcher, IInspectable>::new(move |_, _| {
                    done.end(true);
                    Ok(())
                }))
            })
            .and_then(|_| {
                watcher.Stopped(&TypedEventHandler::<DeviceWatcher, IInspectable>::new(move |_, _| {
                    ended.end(false);
                    Ok(())
                }))
            });
        if handlers.is_err() || watcher.Start().is_err() {
            return Vec::new();
        }
        let complete = listing.complete_within(LISTING);
        let _ = watcher.Stop();
        if !complete {
            return Vec::new();
        }
        super::drained(&found)
    }

    /// The container id of `camera`, whose id is its device link.
    pub(super) fn camera_container(camera: &CameraInfo) -> Option<String> {
        listed(DeviceClass::VideoCapture).into_iter().find(|(id, _, _)| id.eq_ignore_ascii_case(&camera.id)).and_then(|(_, _, c)| c)
    }

    /// The container id of each input, found by its name among the system's audio inputs.
    pub(super) fn input_containers(inputs: &[DeviceInfo]) -> Vec<Option<String>> {
        let listed: Vec<(String, Option<String>)> = listed(DeviceClass::AudioCapture).into_iter().map(|(_, name, c)| (name, c)).collect();
        inputs.iter().map(|d| super::container_by_name(&listed, &d.name)).collect()
    }
}

/// Everything `queue` holds, taken out in the order it was pushed.
#[cfg_attr(not(windows), allow(dead_code))]
fn drained<T>(queue: &crossbeam_queue::ArrayQueue<T>) -> Vec<T> {
    std::iter::from_fn(|| queue.pop()).take(queue.capacity()).collect()
}

/// A listing that has not ended yet.
const PENDING: u8 = 0;
/// A listing that listed every device.
const COMPLETE: u8 = 1;
/// A listing that stopped before it listed every device.
const STOPPED: u8 = 2;

/// How a device listing ended. The system's callback thread records the end in an atomic and
/// wakes the thread that waits for it.
#[cfg_attr(not(windows), allow(dead_code))]
struct Listing {
    state: AtomicU8,
    waiter: Thread,
}

#[cfg_attr(not(windows), allow(dead_code))]
impl Listing {
    /// A listing that the calling thread waits for.
    fn new() -> Self {
        Self { state: AtomicU8::new(PENDING), waiter: std::thread::current() }
    }

    /// Records that the listing ended, complete or stopped, and wakes the waiting thread. Only
    /// the first end counts, so a stop that follows the completion leaves it complete.
    fn end(&self, complete: bool) {
        let to = if complete { COMPLETE } else { STOPPED };
        if self.state.compare_exchange(PENDING, to, Ordering::AcqRel, Ordering::Acquire).is_ok() {
            self.waiter.unpark();
        }
    }

    /// Waits up to `timeout` for the listing to end. True when it listed every device.
    fn complete_within(&self, timeout: Duration) -> bool {
        let deadline = Instant::now().checked_add(timeout);
        loop {
            match self.state.load(Ordering::Acquire) {
                COMPLETE => return true,
                STOPPED => return false,
                _ => {}
            }
            let left = deadline.map_or(Duration::MAX, |d| d.saturating_duration_since(Instant::now()));
            if left.is_zero() {
                return false;
            }
            std::thread::park_timeout(left);
        }
    }
}

/// The container id of the one device named `name` in `listed` (names with container ids). A
/// name that several devices share gives none, since the device it means is not known.
#[cfg_attr(not(windows), allow(dead_code))]
fn container_by_name(listed: &[(String, Option<String>)], name: &str) -> Option<String> {
    let mut named = listed.iter().filter(|(n, _)| n == name);
    match (named.next(), named.next()) {
        (Some((_, c)), None) => c.clone(),
        _ => None,
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use super::{MAX_DEVICES, by_id_serial, node_path, node_serial};
    use crate::audio::DeviceInfo;
    use crate::camera::CameraInfo;

    /// Where udev links each camera node under a name that holds the camera's USB serial.
    const BY_ID: &str = "/dev/v4l/by-id";

    /// The USB serial of the camera whose node `camera.id` names.
    pub(super) fn camera_container(camera: &CameraInfo) -> Option<String> {
        let node = std::fs::canonicalize(node_path(&camera.id)?).ok()?;
        std::fs::read_dir(BY_ID).ok()?.take(MAX_DEVICES).flatten().find_map(|link| {
            let name = link.file_name();
            let serial = by_id_serial(name.to_str()?)?;
            (std::fs::canonicalize(link.path()).ok()? == node).then(|| serial.to_string())
        })
    }

    /// The USB serial in each input's PipeWire node name.
    pub(super) fn input_containers(inputs: &[DeviceInfo]) -> Vec<Option<String>> {
        inputs.iter().map(|d| node_serial(&d.id).map(str::to_string)).collect()
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
mod imp {
    use crate::audio::DeviceInfo;
    use crate::camera::CameraInfo;

    pub(super) fn camera_container(_camera: &CameraInfo) -> Option<String> {
        None
    }

    pub(super) fn input_containers(inputs: &[DeviceInfo]) -> Vec<Option<String>> {
        vec![None; inputs.len()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(name: &str, container: Option<&str>) -> Input {
        Input { device: DeviceInfo { id: format!("id of {name}"), name: name.into(), default: false }, container: container.map(str::to_string) }
    }

    #[test]
    fn the_input_in_the_cameras_container_is_its_microphone() {
        let inputs = [
            input("Microphone Array (Realtek Audio)", Some("aaaa")),
            input("Microphone (USB Audio Device)", Some("BBBB")),
            input("Line In (Realtek Audio)", Some("aaaa")),
        ];
        // The input's name says nothing about the camera; the container id matches in any case.
        assert_eq!(microphone_index("HD Webcam", Some("bbbb"), &inputs), Some(1));
        // Of two inputs in the container, the one whose name names a microphone is chosen.
        assert_eq!(microphone_index("HD Webcam", Some("aaaa"), &inputs), Some(0));
    }

    #[test]
    fn of_several_inputs_in_the_container_the_microphone_is_chosen() {
        let inputs =
            [input("Line In (USB Capture)", Some("cam")), input("Mic In (USB Capture)", Some("cam")), input("Microphone (Realtek)", Some("pc"))];
        // The line input is listed first; the microphone is chosen in any case.
        assert_eq!(microphone_index("USB Capture", Some("cam"), &inputs), Some(1));
        // Without a microphone word, the input whose name matches the camera's better decides.
        let inputs = [input("Digital Audio Interface (USB Capture)", Some("cam")), input("USB Capture", Some("cam"))];
        assert_eq!(microphone_index("USB Capture", Some("cam"), &inputs), Some(1));
        // Two equally good inputs in the container choose none, and no input elsewhere is taken.
        let inputs = [input("Line 1", Some("cam")), input("Line 2", Some("cam")), input("USB Capture", None)];
        assert_eq!(microphone_index("USB Capture", Some("cam"), &inputs), None);
        let inputs = [input("Microphone 1 (USB Capture)", Some("cam")), input("Microphone 2 (USB Capture)", Some("cam"))];
        assert_eq!(microphone_index("USB Capture", Some("cam"), &inputs), None);
    }

    #[test]
    fn the_container_id_wins_over_a_name() {
        let inputs = [input("HD Webcam", Some("other")), input("USB Audio", Some("cam"))];
        assert_eq!(microphone_index("HD Webcam", Some("cam"), &inputs), Some(1));
    }

    #[test]
    fn without_a_container_match_the_name_decides() {
        let inputs =
            [input("Microphone Array (Realtek Audio)", None), input("Microphone (HD Pro Webcam C920)", None), input("Studio Mic", Some("elsewhere"))];
        assert_eq!(microphone_index("HD Pro Webcam C920", Some("not listed"), &inputs), Some(1));
        assert_eq!(microphone_index("HD Pro Webcam C920", None, &inputs), Some(1));
        // A camera whose device is unknown matches an input of a known device by name.
        let inputs = [input("Microphone (HD Pro Webcam C920)", Some("elsewhere"))];
        assert_eq!(microphone_index("HD Pro Webcam C920", None, &inputs), Some(0));
        // An empty container id is not a match for inputs without one.
        assert_eq!(microphone_index("HD Pro Webcam C920", Some(""), &[input("Studio Mic", Some(""))]), None);
    }

    #[test]
    fn a_known_camera_does_not_take_the_microphone_of_another_known_device() {
        // Two cameras of the same model; this one's microphone is not listed, the other's is.
        let inputs = [input("Microphone (HD Webcam)", Some("other camera")), input("Microphone Array (Realtek Audio)", None)];
        assert_eq!(microphone_index("HD Webcam", Some("this camera"), &inputs), None);
        // An input whose device is not known still matches by name.
        let inputs = [input("Microphone (HD Webcam)", Some("other camera")), input("HD Webcam", None)];
        assert_eq!(microphone_index("HD Webcam", Some("this camera"), &inputs), Some(1));
    }

    #[test]
    fn the_same_name_is_preferred_to_a_name_that_holds_it() {
        let inputs = [input("HD Pro Webcam C920 Analog Stereo", None), input("hd pro webcam  C920", None)];
        assert_eq!(microphone_index("HD Pro Webcam C920", None, &inputs), Some(1));
    }

    #[test]
    fn names_match_by_whole_words_ignoring_case_and_punctuation() {
        assert_eq!(microphone_index("Cam Link", None, &[input("Microphone (2- cam-link)", None)]), Some(0));
        // "Cam" is not the start of "Camera".
        assert_eq!(microphone_index("Cam", None, &[input("Camera Microphone", None)]), None);
        // A camera whose name has no letters or digits matches nothing by name.
        assert_eq!(microphone_index("()", None, &[input("()", None)]), None);
        assert_eq!(microphone_index("", None, &[input("Microphone", None)]), None);
    }

    #[test]
    fn two_equally_good_names_choose_nothing() {
        let inputs = [input("Microphone (USB Camera)", None), input("Line (USB Camera)", None)];
        assert_eq!(microphone_index("USB Camera", None, &inputs), None);
        // A better name still decides.
        let inputs = [input("Microphone (USB Camera)", None), input("USB Camera", None), input("Line (USB Camera)", None)];
        assert_eq!(microphone_index("USB Camera", None, &inputs), Some(1));
    }

    #[test]
    fn no_inputs_or_no_match_give_none() {
        assert_eq!(microphone_index("HD Webcam", Some("cam"), &[]), None);
        assert_eq!(microphone_index("HD Webcam", Some("cam"), &[input("Studio Mic", Some("mic"))]), None);
    }

    #[test]
    fn only_the_first_inputs_up_to_the_limit_are_compared() {
        let mut inputs: Vec<Input> = (0..MAX_DEVICES).map(|n| input(&format!("Mic {n}"), None)).collect();
        inputs.push(input("HD Webcam", Some("cam")));
        assert_eq!(microphone_index("HD Webcam", Some("cam"), &inputs), None);
    }

    #[test]
    fn devices_sent_from_another_thread_arrive_in_order_up_to_the_limit() {
        let queue = std::sync::Arc::new(crossbeam_queue::ArrayQueue::new(MAX_DEVICES));
        let sender = std::sync::Arc::clone(&queue);
        // The device watcher's callback runs on a thread of the system's; one past the limit is
        // not taken.
        let refused = std::thread::spawn(move || (0..=MAX_DEVICES).filter(|&n| sender.push(n).is_err()).count()).join().unwrap();
        assert_eq!(refused, 1);
        assert_eq!(drained(&queue), (0..MAX_DEVICES).collect::<Vec<_>>());
        assert!(queue.is_empty());
        assert_eq!(drained(&queue), Vec::<usize>::new());
    }

    #[test]
    fn a_listing_completed_on_another_thread_wakes_the_waiting_thread() {
        let listing = std::sync::Arc::new(Listing::new());
        let ender = std::sync::Arc::clone(&listing);
        // The device watcher's callbacks run on a thread of the system's.
        let ending = std::thread::spawn(move || ender.end(true));
        assert!(listing.complete_within(Duration::MAX));
        ending.join().unwrap();
    }

    #[test]
    fn only_the_first_end_of_a_listing_counts() {
        let stopped = Listing::new();
        stopped.end(false);
        stopped.end(true);
        assert!(!stopped.complete_within(Duration::MAX));

        // The watcher stops after it completes; the listing stays complete.
        let completed = Listing::new();
        completed.end(true);
        completed.end(false);
        assert!(completed.complete_within(Duration::MAX));
    }

    #[test]
    fn a_listing_that_has_not_ended_is_not_complete() {
        assert!(!Listing::new().complete_within(Duration::ZERO));
    }

    #[test]
    fn an_input_takes_the_container_of_the_one_device_with_its_name() {
        let listed = [
            ("Microphone (HD Webcam)".to_string(), Some("cam".to_string())),
            ("Microphone (USB Audio Device)".to_string(), Some("first".to_string())),
            ("Microphone (USB Audio Device)".to_string(), Some("second".to_string())),
            ("Line In".to_string(), None),
        ];
        assert_eq!(container_by_name(&listed, "Microphone (HD Webcam)"), Some("cam".into()));
        // Two devices share the name: neither container is given.
        assert_eq!(container_by_name(&listed, "Microphone (USB Audio Device)"), None);
        assert_eq!(container_by_name(&listed, "Line In"), None);
        assert_eq!(container_by_name(&listed, "Not Listed"), None);
    }

    #[test]
    fn a_linux_camera_id_gives_its_device_node() {
        assert_eq!(node_path("/dev/video0"), Some("/dev/video0"));
        assert_eq!(node_path("Video4Linux Device @ /dev/video2"), Some("/dev/video2"));
        assert_eq!(node_path("Video4Linux Device"), None);
        assert_eq!(node_path(""), None);
    }

    #[test]
    fn a_camera_link_name_gives_its_usb_serial() {
        assert_eq!(by_id_serial("usb-046d_HD_Pro_Webcam_C920_2F4B6A4F-video-index0"), Some("usb-046d_HD_Pro_Webcam_C920_2F4B6A4F"));
        assert_eq!(by_id_serial("usb-Some-Cam_01-video-index12"), Some("usb-Some-Cam_01"));
        assert_eq!(by_id_serial("usb-046d_HD_Pro_Webcam_C920_2F4B6A4F-video-index"), None);
        assert_eq!(by_id_serial("usb-cam-video-indexA"), None);
        assert_eq!(by_id_serial("-video-index0"), None);
        assert_eq!(by_id_serial("usb-cam"), None);
    }

    #[test]
    fn a_pipewire_input_node_name_gives_its_usb_serial() {
        assert_eq!(node_serial("alsa_input.usb-046d_HD_Pro_Webcam_C920_2F4B6A4F-02.analog-stereo"), Some("usb-046d_HD_Pro_Webcam_C920_2F4B6A4F"));
        assert_eq!(node_serial("alsa_input.usb-Some-Cam_01-03.mono-fallback"), Some("usb-Some-Cam_01"));
        assert_eq!(node_serial("alsa_input.usb-cam"), Some("usb-cam"));
        // A period inside the USB identifier stays in the serial; only the profile is removed.
        let dotted = "alsa_input.usb-Acme_USB_2.0_Camera_123-02.analog-stereo";
        assert_eq!(node_serial(dotted), Some("usb-Acme_USB_2.0_Camera_123"));
        assert_eq!(node_serial(dotted), by_id_serial("usb-Acme_USB_2.0_Camera_123-video-index0"));
        assert_eq!(node_serial("alsa_output.usb-046d_Speaker-00.analog-stereo"), None);
        assert_eq!(node_serial("alsa_input."), None);
        assert_eq!(node_serial("bluez_input.00_11_22"), None);
    }

    #[test]
    fn a_camera_and_its_input_on_linux_share_the_serial() {
        let camera = by_id_serial("usb-046d_HD_Pro_Webcam_C920_2F4B6A4F-video-index0");
        let inputs = [
            Input {
                device: DeviceInfo {
                    id: "alsa_input.pci-0000_00_1f.3.analog-stereo".into(),
                    name: "Built-in Audio Analog Stereo".into(),
                    default: true,
                },
                container: node_serial("alsa_input.pci-0000_00_1f.3.analog-stereo").map(str::to_string),
            },
            Input {
                device: DeviceInfo {
                    id: "alsa_input.usb-046d_HD_Pro_Webcam_C920_2F4B6A4F-02.analog-stereo".into(),
                    name: "Webcam Audio".into(),
                    default: false,
                },
                container: node_serial("alsa_input.usb-046d_HD_Pro_Webcam_C920_2F4B6A4F-02.analog-stereo").map(str::to_string),
            },
        ];
        assert_eq!(microphone_index("HD Pro Webcam C920", camera, &inputs), Some(1));
    }
}
