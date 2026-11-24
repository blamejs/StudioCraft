//! The ingest servers a streaming service publishes in a machine-readable list
//! ([`Service::ingests`]), for choosing a destination's server.
//!
//! A list is fetched only when asked for ([`Lists::request`]), on a thread of its own, over HTTPS
//! with no redirect and no proxy, reading at most [`INGEST_LIST_LIMIT`] bytes within
//! [`PROBE_TIMEOUT`]. The caller does not wait: it reads [`Lists::state`] again, and the function
//! given to [`Lists::request`] runs once the fetch has ended. A list that was read is kept for the
//! session; a list that could not be read is fetched again on the next request.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

use serde_json::Value;
use studiocraft_model::services::Service;

use super::diagnose::{INGEST_LIST_LIMIT, MAX_INGESTS, PROBE_TIMEOUT, host_of};
use super::lock;

/// The most servers kept from one list.
pub const MAX_SERVERS: usize = 128;
/// The longest server name kept, in characters; a longer name is cut.
pub const MAX_NAME: usize = 128;
/// The longest server address kept, in bytes; an entry with a longer one is left out.
pub const MAX_ADDRESS: usize = 1024;

/// One ingest server of a service's list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IngestServer {
    /// The name the service gives the server, such as the region it is in.
    pub name: String,
    /// The server's `rtmps://` or `rtmp://` address, without the stream key.
    pub server: String,
}

/// Where the list of one service is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ListState {
    /// The list is being fetched.
    Loading,
    /// The servers of the list, the service's default first.
    Ready(Arc<[IngestServer]>),
    /// The list could not be fetched or read, in English.
    Failed(String),
}

/// Fetches the body of a URL, at most the given number of bytes. Tests pass one that answers as
/// they choose.
pub(crate) type Fetch = dyn Fn(&str, u64) -> Result<Vec<u8>, String> + Send + Sync;

/// The lists of the services' ingest servers in this process, by service id.
pub struct Lists {
    fetch: Arc<Fetch>,
    states: Mutex<HashMap<&'static str, ListState>>,
}

static LISTS: LazyLock<Arc<Lists>> = LazyLock::new(|| Lists::with(Arc::new(|url, limit| super::diagnose::fetch(url, limit, PROBE_TIMEOUT))));

/// The process's lists of ingest servers, fetched from the services.
pub fn lists() -> Arc<Lists> {
    Arc::clone(&LISTS)
}

impl Lists {
    pub(crate) fn with(fetch: Arc<Fetch>) -> Arc<Lists> {
        Arc::new(Lists { fetch, states: Mutex::new(HashMap::new()) })
    }

    /// Starts fetching the list of `service` unless it is being fetched or has been read, and
    /// returns its state at once. `done` runs on the fetch's thread when the fetch ends, and at
    /// once when no fetch starts. A service that publishes no list is an error.
    pub fn request(self: &Arc<Self>, service: &'static Service, done: impl FnOnce() + Send + 'static) -> Result<ListState, String> {
        let Some(url) = service.ingests else {
            return Err(format!("{} publishes no list of its ingest servers", service.name));
        };
        let mut states = lock(&self.states);
        if let Some(s @ (ListState::Loading | ListState::Ready(_))) = states.get(service.id) {
            let s = s.clone();
            drop(states);
            done();
            return Ok(s);
        }
        states.insert(service.id, ListState::Loading);
        drop(states);
        let lists = Arc::clone(self);
        let id = service.id;
        let default = service.server;
        let spawned = std::thread::Builder::new().name("ingest-list".into()).spawn(move || {
            let read = (lists.fetch)(url, INGEST_LIST_LIMIT).and_then(|body| servers(&body, default));
            let state = match read {
                Ok(list) => ListState::Ready(list.into()),
                Err(e) => ListState::Failed(e),
            };
            lock(&lists.states).insert(id, state);
            done();
        });
        if let Err(e) = spawned {
            let failed = ListState::Failed(format!("the list of ingest servers could not be fetched: {e}"));
            lock(&self.states).insert(id, failed.clone());
            return Ok(failed);
        }
        Ok(ListState::Loading)
    }

    /// The state of the list of the service with id `id`; `None` when it has not been asked for.
    pub fn state(&self, id: &str) -> Option<ListState> {
        lock(&self.states).get(id).cloned()
    }
}

/// The servers of the ingest list `body`: those available, each address once, in the order of
/// their priority (lowest first) and then of the list, at most [`MAX_SERVERS`]. The entry marked
/// as the default, or else the one at `default`, comes first.
pub fn servers(body: &[u8], default: Option<&str>) -> Result<Vec<IngestServer>, String> {
    let json: Value = serde_json::from_slice(body).map_err(|e| format!("the list of ingest servers is not valid JSON: {e}"))?;
    let Some(list) = json.get("ingests").and_then(Value::as_array) else {
        return Err("the list of ingest servers has no `ingests` list".into());
    };
    let mut found: Vec<(bool, f64, IngestServer)> = Vec::new();
    for entry in list.iter().take(MAX_INGESTS) {
        let available = entry.get("availability").and_then(Value::as_f64).is_none_or(|a| a > 0.0);
        let url = entry.get("url_template_secure").or_else(|| entry.get("url_template")).and_then(Value::as_str);
        let Some(server) = url.and_then(address) else { continue };
        if !available || found.iter().any(|(_, _, s)| s.server == server) {
            continue;
        }
        let marked = entry.get("default").and_then(Value::as_bool).unwrap_or(false) || default == Some(server.as_str());
        let priority = entry.get("priority").and_then(Value::as_f64).filter(|p| p.is_finite()).unwrap_or(f64::MAX);
        let name = entry.get("name").and_then(Value::as_str).map(clean_name).filter(|n| !n.is_empty());
        let name = name.or_else(|| host_of(&server)).unwrap_or_default();
        found.push((marked, priority, IngestServer { name, server }));
    }
    // A stable sort keeps the list's order among equal priorities.
    found.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.total_cmp(&b.1)));
    Ok(found.into_iter().take(MAX_SERVERS).map(|(_, _, s)| s).collect())
}

/// The server address of an entry's URL template: the template without its `{stream_key}` at the
/// end and without a final `/`. `None` for a template that is not an `rtmp://` or `rtmps://`
/// address with a valid host and an application, or that holds a user name, a query, a fragment,
/// another placeholder, a space or a control character, or is longer than [`MAX_ADDRESS`].
fn address(template: &str) -> Option<String> {
    let server = template.strip_suffix("{stream_key}").unwrap_or(template).trim_end_matches('/');
    let scheme = if server.starts_with("rtmps://") { "rtmps://" } else { "rtmp://" };
    let rest = server.strip_prefix(scheme)?;
    let (authority, app) = rest.split_once('/')?;
    let bad = |c: char| c.is_whitespace() || c.is_control() || matches!(c, '{' | '}' | '?' | '#' | '@' | '\\');
    if server.len() > MAX_ADDRESS || app.is_empty() || server.contains(bad) || authority.is_empty() {
        return None;
    }
    host_of(server)?;
    Some(server.to_string())
}

/// `name` without control characters, trimmed, at most [`MAX_NAME`] characters.
fn clean_name(name: &str) -> String {
    let kept: String = name.chars().filter(|c| !c.is_control()).collect();
    kept.trim().chars().take(MAX_NAME).collect()
}

#[cfg(test)]
mod tests;
