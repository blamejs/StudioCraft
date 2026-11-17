//! The registry of plug-in types, filled once at startup.

use std::sync::Arc;

use serde_json::{Value, json};

use crate::encoder::{Encoder, EncoderKind};
use crate::filter::{Filter, FilterTarget};
use crate::output::Output;
use crate::source::Source;
use crate::transition::Transition;
use crate::{Plugin, PluginError};

/// Every registered plug-in type, by kind, in registration order.
#[derive(Default, Clone)]
pub struct Registry {
    sources: Vec<Arc<dyn Source>>,
    filters: Vec<Arc<dyn Filter>>,
    transitions: Vec<Arc<dyn Transition>>,
    encoders: Vec<Arc<dyn Encoder>>,
    outputs: Vec<Arc<dyn Output>>,
}

fn add<T: ?Sized + Plugin>(list: &mut Vec<Arc<T>>, kind: &'static str, p: Arc<T>) -> Result<(), PluginError> {
    if list.iter().any(|x| x.id() == p.id()) {
        return Err(PluginError::Duplicate { kind, id: p.id().into() });
    }
    list.push(p);
    Ok(())
}

fn find<T: ?Sized + Plugin>(list: &[Arc<T>], kind: &'static str, id: &str) -> Result<Arc<T>, PluginError> {
    list.iter().find(|x| x.id() == id).cloned().ok_or_else(|| PluginError::Unknown { kind, id: id.into() })
}

fn describe<T: ?Sized + Plugin>(list: &[Arc<T>], extra: impl Fn(&T) -> Value) -> Value {
    Value::Array(
        list.iter()
            .map(|p| {
                let mut v = json!({"id": p.id(), "name": p.name(), "version": p.version(), "schema": p.schema()});
                if let (Some(o), Value::Object(e)) = (v.as_object_mut(), extra(p)) {
                    o.extend(e);
                }
                v
            })
            .collect(),
    )
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register_source(&mut self, p: Arc<dyn Source>) -> Result<(), PluginError> {
        add(&mut self.sources, "source", p)
    }

    pub fn register_filter(&mut self, p: Arc<dyn Filter>) -> Result<(), PluginError> {
        add(&mut self.filters, "filter", p)
    }

    pub fn register_transition(&mut self, p: Arc<dyn Transition>) -> Result<(), PluginError> {
        add(&mut self.transitions, "transition", p)
    }

    pub fn register_encoder(&mut self, p: Arc<dyn Encoder>) -> Result<(), PluginError> {
        add(&mut self.encoders, "encoder", p)
    }

    pub fn register_output(&mut self, p: Arc<dyn Output>) -> Result<(), PluginError> {
        add(&mut self.outputs, "output", p)
    }

    pub fn source(&self, id: &str) -> Result<Arc<dyn Source>, PluginError> {
        find(&self.sources, "source", id)
    }

    pub fn filter(&self, id: &str) -> Result<Arc<dyn Filter>, PluginError> {
        find(&self.filters, "filter", id)
    }

    pub fn transition(&self, id: &str) -> Result<Arc<dyn Transition>, PluginError> {
        find(&self.transitions, "transition", id)
    }

    pub fn encoder(&self, id: &str) -> Result<Arc<dyn Encoder>, PluginError> {
        find(&self.encoders, "encoder", id)
    }

    pub fn output(&self, id: &str) -> Result<Arc<dyn Output>, PluginError> {
        find(&self.outputs, "output", id)
    }

    pub fn sources(&self) -> &[Arc<dyn Source>] {
        &self.sources
    }

    pub fn filters(&self) -> &[Arc<dyn Filter>] {
        &self.filters
    }

    pub fn transitions(&self) -> &[Arc<dyn Transition>] {
        &self.transitions
    }

    pub fn encoders(&self) -> &[Arc<dyn Encoder>] {
        &self.encoders
    }

    pub fn outputs(&self) -> &[Arc<dyn Output>] {
        &self.outputs
    }

    /// Every plug-in with its id, name, version and schema, for scripts and agents.
    pub fn describe(&self) -> Value {
        json!({
            "sources": describe(&self.sources, |s| { let c = s.caps(); json!({"video": c.video, "audio": c.audio}) }),
            "filters": describe(&self.filters, |f| json!({"target": match f.target() { FilterTarget::Video => "video", FilterTarget::Audio => "audio" }})),
            "transitions": describe(&self.transitions, |t| json!({"instant": t.instant()})),
            "encoders": describe(&self.encoders, |e| {
                let kind = match e.kind() { EncoderKind::Video => "video", EncoderKind::Audio => "audio" };
                let mut v = json!({"kind": kind, "codec": e.codec().id(), "builtin": e.ffmpeg().is_none()});
                if let (Some(f), Some(o)) = (e.ffmpeg(), v.as_object_mut()) {
                    o.insert("ffmpeg_codec".into(), json!(f.ffmpeg_codec()));
                }
                v
            }),
            "outputs": describe(&self.outputs, |_| json!({})),
        })
    }
}

#[cfg(test)]
mod tests {
    use studiocraft_model::{Settings, SettingsSchema};

    use super::*;
    use crate::{SourceCaps, SourceInstance, SourceIo};

    struct Dummy(&'static str);
    impl Plugin for Dummy {
        fn id(&self) -> &'static str {
            self.0
        }
        fn name(&self) -> &'static str {
            "Dummy"
        }
        fn schema(&self) -> SettingsSchema {
            SettingsSchema::default()
        }
    }
    impl Source for Dummy {
        fn caps(&self) -> SourceCaps {
            SourceCaps { video: true, audio: false }
        }
        fn create(&self, _: &Settings, _: &SourceIo) -> Result<Box<dyn SourceInstance>, PluginError> {
            Err(PluginError::Unavailable("dummy".into()))
        }
    }

    #[test]
    fn registers_finds_and_refuses_duplicates() {
        let mut r = Registry::new();
        r.register_source(Arc::new(Dummy("a"))).unwrap();
        r.register_source(Arc::new(Dummy("b"))).unwrap();
        assert!(matches!(r.register_source(Arc::new(Dummy("a"))), Err(PluginError::Duplicate { .. })));
        assert_eq!(r.source("b").unwrap().id(), "b");
        assert!(matches!(r.source("c"), Err(PluginError::Unknown { kind: "source", .. })));
        let d = r.describe();
        assert_eq!(d["sources"][1]["id"], "b");
        assert_eq!(d["sources"][0]["video"], true);
        assert_eq!(d["sources"][0]["version"], 1);
    }

    impl crate::Encoder for Dummy {
        fn kind(&self) -> EncoderKind {
            EncoderKind::Video
        }
        fn codec(&self) -> crate::Codec {
            crate::Codec::H264
        }
    }

    #[test]
    fn a_built_in_encoder_is_listed_without_an_ffmpeg_codec() {
        let mut r = Registry::new();
        r.register_encoder(Arc::new(Dummy("enc"))).unwrap();
        let e = &r.describe()["encoders"][0];
        assert_eq!((&e["kind"], &e["codec"], &e["builtin"]), (&json!("video"), &json!("h264"), &json!(true)));
        assert!(e.get("ffmpeg_codec").is_none(), "{e}");
    }
}
