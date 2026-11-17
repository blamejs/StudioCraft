//! The parts of PipeWire audio capture that need no running server: reading node and default-device
//! properties from the registry, and turning a buffer's bytes into samples.

use super::{DeviceInfo, DeviceKind};

/// An audio node the PipeWire registry announced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Node {
    pub kind: DeviceKind,
    /// `node.name`, which the server keeps for a device across restarts.
    pub name: String,
    /// `node.description`, the name people see.
    pub description: String,
}

/// The side of the sound system a node's `media.class` puts it on.
pub fn node_kind(media_class: &str) -> Option<DeviceKind> {
    if media_class.starts_with("Audio/Source") {
        Some(DeviceKind::Input)
    } else if media_class.starts_with("Audio/Sink") {
        Some(DeviceKind::Output)
    } else {
        None
    }
}

/// A node from its registry properties, or `None` when it is not an audio device or has no name.
pub fn node(media_class: Option<&str>, name: Option<&str>, description: Option<&str>) -> Option<Node> {
    let kind = node_kind(media_class?)?;
    let name = name?.trim();
    if name.is_empty() {
        return None;
    }
    let description = description.map(str::trim).filter(|d| !d.is_empty()).unwrap_or(name);
    Some(Node { kind, name: name.to_owned(), description: description.to_owned() })
}

/// The key of the `default` metadata object that names the default device of `kind`.
pub fn default_key(kind: DeviceKind) -> &'static str {
    match kind {
        DeviceKind::Input => "default.audio.source",
        DeviceKind::Output => "default.audio.sink",
    }
}

/// The node name in a default-device metadata value, which is a JSON object such as
/// `{"name":"alsa_input.usb-mic"}`.
pub fn default_name(value: &str) -> Option<String> {
    let json: serde_json::Value = serde_json::from_str(value).ok()?;
    let name = json.get("name")?.as_str()?.trim();
    (!name.is_empty()).then(|| name.to_owned())
}

/// The devices of `kind` among `nodes`, each name once, with the node named `default` marked.
pub fn device_list(kind: DeviceKind, nodes: &[Node], default: Option<&str>) -> Vec<DeviceInfo> {
    let mut list: Vec<DeviceInfo> = Vec::new();
    for n in nodes.iter().filter(|n| n.kind == kind) {
        if list.iter().any(|d| d.id == n.name) {
            continue;
        }
        list.push(DeviceInfo { id: n.name.clone(), name: n.description.clone(), default: default == Some(n.name.as_str()) });
    }
    list
}

/// Pass the interleaved 32-bit float samples in the `size` bytes at `offset` in a buffer's `bytes`
/// to `deliver`, converted in `scratch`. The offset is taken modulo the buffer's length, as the
/// PipeWire chunk description says. Bytes past the end of the buffer and a part frame at the end
/// are left out, and a sample that is not a finite number becomes silence. A buffer larger than
/// `scratch`'s capacity is delivered in whole frames that fit it, so `scratch` does not grow
/// unless it cannot hold one frame.
pub fn deliver_samples(bytes: &[u8], offset: u32, size: u32, channels: u16, scratch: &mut Vec<f32>, mut deliver: impl FnMut(&[f32])) {
    if bytes.is_empty() {
        return;
    }
    let start = usize::try_from(offset).map_or(0, |o| o % bytes.len());
    let end = start.saturating_add(usize::try_from(size).unwrap_or(usize::MAX)).min(bytes.len());
    let Some(region) = bytes.get(start..end) else { return };
    let frame = 4 * usize::from(channels.max(1));
    let Some(whole) = region.get(..region.len() / frame * frame) else { return };
    let pass = (scratch.capacity().saturating_mul(4) / frame).max(1).saturating_mul(frame);
    for piece in whole.chunks(pass) {
        scratch.clear();
        scratch.extend(piece.as_chunks::<4>().0.iter().map(|b| {
            let s = f32::from_le_bytes(*b);
            if s.is_finite() { s } else { 0.0 }
        }));
        deliver(scratch);
    }
}

/// A stream state change, as far as capture cares about it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StreamChange {
    Error(String),
    Unconnected,
    Streaming,
    Other,
}

/// Why a capture stream stopped delivering audio, or `None` when it did not stop. `connected` tells
/// whether the stream has left its first unconnected state (it is connecting, paused or
/// streaming); a stream that has not started connecting has not ended.
pub fn stream_failure(change: &StreamChange, connected: bool) -> Option<String> {
    match change {
        // The server's own text is left out: the window translates fixed messages only.
        StreamChange::Error(_) => Some("the audio stream failed".into()),
        StreamChange::Unconnected if connected => Some("the audio stream ended".into()),
        StreamChange::Unconnected | StreamChange::Streaming | StreamChange::Other => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(kind: DeviceKind, name: &str, description: &str) -> Node {
        Node { kind, name: name.into(), description: description.into() }
    }

    #[test]
    fn media_classes_sort_nodes_into_inputs_and_outputs() {
        assert_eq!(node_kind("Audio/Source"), Some(DeviceKind::Input));
        assert_eq!(node_kind("Audio/Source/Virtual"), Some(DeviceKind::Input));
        assert_eq!(node_kind("Audio/Sink"), Some(DeviceKind::Output));
        assert_eq!(node_kind("Stream/Input/Audio"), None);
        assert_eq!(node_kind("Video/Source"), None);
        assert_eq!(node_kind(""), None);
    }

    #[test]
    fn a_node_needs_a_class_and_a_name() {
        assert_eq!(node(Some("Audio/Source"), Some("mic"), Some("USB microphone")), Some(n(DeviceKind::Input, "mic", "USB microphone")));
        assert_eq!(node(Some("Audio/Sink"), Some("spk"), None), Some(n(DeviceKind::Output, "spk", "spk")));
        assert_eq!(node(Some("Audio/Sink"), Some("spk"), Some("  ")), Some(n(DeviceKind::Output, "spk", "spk")));
        assert_eq!(node(None, Some("mic"), None), None);
        assert_eq!(node(Some("Audio/Source"), None, Some("Microphone")), None);
        assert_eq!(node(Some("Audio/Source"), Some(" "), None), None);
        assert_eq!(node(Some("Midi/Bridge"), Some("midi"), None), None);
    }

    #[test]
    fn default_devices_are_read_from_the_metadata_value() {
        assert_eq!(default_key(DeviceKind::Input), "default.audio.source");
        assert_eq!(default_key(DeviceKind::Output), "default.audio.sink");
        assert_eq!(default_name(r#"{"name":"alsa_input.usb-mic"}"#).as_deref(), Some("alsa_input.usb-mic"));
        assert_eq!(default_name(r#"{ "name" : "a" , "other": 1 }"#).as_deref(), Some("a"));
        assert_eq!(default_name(r#"{"name":""}"#), None);
        assert_eq!(default_name(r#"{"name":7}"#), None);
        assert_eq!(default_name(r#"{"nick":"a"}"#), None);
        assert_eq!(default_name("alsa_input.usb-mic"), None);
        assert_eq!(default_name(""), None);
    }

    #[test]
    fn the_list_holds_each_device_of_the_kind_once_with_the_default_marked() {
        let nodes = [
            n(DeviceKind::Input, "mic", "Microphone"),
            n(DeviceKind::Output, "spk", "Speakers"),
            n(DeviceKind::Input, "line", "Line in"),
            n(DeviceKind::Input, "mic", "Microphone again"),
        ];
        let list = device_list(DeviceKind::Input, &nodes, Some("line"));
        assert_eq!(list.len(), 2);
        assert_eq!(list[0], DeviceInfo { id: "mic".into(), name: "Microphone".into(), default: false });
        assert_eq!(list[1], DeviceInfo { id: "line".into(), name: "Line in".into(), default: true });
        let outputs = device_list(DeviceKind::Output, &nodes, Some("line"));
        assert_eq!(outputs, vec![DeviceInfo { id: "spk".into(), name: "Speakers".into(), default: false }]);
        assert!(device_list(DeviceKind::Input, &[], Some("mic")).is_empty());
    }

    fn bytes(samples: &[f32]) -> Vec<u8> {
        samples.iter().flat_map(|s| s.to_le_bytes()).collect()
    }

    /// The samples `deliver_samples` passes on, each delivery as one list.
    fn delivered(b: &[u8], offset: u32, size: u32, channels: u16, scratch: &mut Vec<f32>) -> Vec<Vec<f32>> {
        let mut out = Vec::new();
        deliver_samples(b, offset, size, channels, scratch, |s| out.push(s.to_vec()));
        out
    }

    #[test]
    fn samples_come_from_the_chunk_inside_the_buffer() {
        let b = bytes(&[0.5, -0.5, 0.25, -0.25]);
        let mut scratch = Vec::with_capacity(64);
        assert_eq!(delivered(&b, 0, 16, 2, &mut scratch), [vec![0.5, -0.5, 0.25, -0.25]]);
        assert_eq!(delivered(&b, 8, 8, 2, &mut scratch), [vec![0.25, -0.25]]);
        // A size past the end of the buffer reads to the end.
        assert_eq!(delivered(&b, 8, u32::MAX, 2, &mut scratch), [vec![0.25, -0.25]]);
        // An offset is taken modulo the buffer's length.
        assert_eq!(delivered(&b, 16, 8, 2, &mut scratch), [vec![0.5, -0.5]]);
        assert_eq!(delivered(&b, 24, 8, 2, &mut scratch), [vec![0.25, -0.25]]);
        assert_eq!(delivered(&b, 64, 16, 2, &mut scratch), [vec![0.5, -0.5, 0.25, -0.25]]);
        // u32::MAX % 16 is 15, which leaves less than a frame.
        assert!(delivered(&b, u32::MAX, u32::MAX, 2, &mut scratch).is_empty());
        assert!(delivered(&[], 16, 16, 2, &mut scratch).is_empty());
    }

    #[test]
    fn a_part_frame_is_left_out() {
        let b = bytes(&[0.1, 0.2, 0.3]);
        let mut scratch = Vec::with_capacity(64);
        assert_eq!(delivered(&b, 0, 12, 2, &mut scratch), [vec![0.1, 0.2]]);
        assert_eq!(delivered(&b, 0, 11, 1, &mut scratch), [vec![0.1, 0.2]]);
        // Zero channels is read as one.
        assert_eq!(delivered(&b, 0, 12, 0, &mut scratch).concat().len(), 3);
    }

    #[test]
    fn a_sample_that_is_not_a_number_is_silence() {
        let b = bytes(&[f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 1.0]);
        let mut scratch = Vec::with_capacity(64);
        assert_eq!(delivered(&b, 0, 16, 1, &mut scratch), [vec![0.0, 0.0, 0.0, 1.0]]);
    }

    #[test]
    fn the_scratch_buffer_is_reused_between_buffers() {
        let b = bytes(&[0.0; 512]);
        let mut scratch = Vec::with_capacity(512);
        let cap = scratch.capacity();
        for _ in 0..4 {
            assert_eq!(delivered(&b, 0, 2048, 2, &mut scratch).concat().len(), 512);
        }
        assert_eq!(scratch.capacity(), cap);
    }

    #[test]
    fn a_buffer_larger_than_the_scratch_buffer_is_delivered_in_whole_frames() {
        let samples: Vec<f32> = (0..30u8).map(f32::from).collect();
        let b = bytes(&samples);
        let mut scratch = Vec::with_capacity(8);
        let cap = scratch.capacity();
        let parts = delivered(&b, 0, u32::MAX, 3, &mut scratch);
        assert!(parts.len() > 1);
        assert!(parts.iter().all(|p| p.len() % 3 == 0 && p.len() <= cap), "{parts:?}");
        assert_eq!(parts.concat(), samples);
        assert_eq!(scratch.capacity(), cap, "the scratch buffer does not grow");
        // A scratch buffer that cannot hold one frame grows to one frame.
        let mut tiny = Vec::new();
        assert_eq!(delivered(&b, 0, 24, 3, &mut tiny), [vec![0.0, 1.0, 2.0], vec![3.0, 4.0, 5.0]]);
    }

    #[test]
    fn an_error_or_an_end_after_connecting_stops_the_capture() {
        assert_eq!(stream_failure(&StreamChange::Error("device busy".into()), false).as_deref(), Some("the audio stream failed"));
        assert_eq!(stream_failure(&StreamChange::Error(String::new()), true).as_deref(), Some("the audio stream failed"));
        assert_eq!(stream_failure(&StreamChange::Unconnected, true).as_deref(), Some("the audio stream ended"));
        assert_eq!(stream_failure(&StreamChange::Unconnected, false), None);
        assert_eq!(stream_failure(&StreamChange::Streaming, true), None);
        assert_eq!(stream_failure(&StreamChange::Other, true), None);
    }
}
