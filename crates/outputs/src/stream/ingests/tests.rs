use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use super::*;

/// How long a test waits for something that should happen at once before it fails. Only a broken
/// build waits this long.
const GUARD: Duration = Duration::from_secs(60);

static SERVICE: Service = Service {
    id: "example",
    name: "Example",
    server: Some("rtmps://default.example/app"),
    source: "https://example.com/streaming",
    ingests: Some("https://example.com/ingests"),
    status: None,
    status_component: None,
};

static NO_LIST: Service = Service { id: "nolist", ingests: None, ..SERVICE };

fn list() -> Vec<u8> {
    serde_json::json!({ "ingests": [
        { "name": "West", "priority": 3, "availability": 1.0, "url_template_secure": "rtmps://west.example/app/{stream_key}" },
        { "name": "Down", "priority": 1, "availability": 0.0, "url_template_secure": "rtmps://down.example/app/{stream_key}" },
        { "name": "East\u{7}\n", "priority": 2, "url_template": "rtmp://east.example/app/{stream_key}" },
        { "name": "Default", "default": true, "priority": 9, "availability": 1.0,
          "url_template_secure": "rtmps://default.example/app/{stream_key}" },
        { "name": "West again", "priority": 0, "url_template_secure": "rtmps://west.example/app/{stream_key}" },
        { "priority": 4, "url_template_secure": "rtmps://NoName.example/app/{stream_key}" },
    ] })
    .to_string()
    .into_bytes()
}

fn names(list: &[IngestServer]) -> Vec<(&str, &str)> {
    list.iter().map(|s| (s.name.as_str(), s.server.as_str())).collect()
}

#[test]
fn servers_are_read_default_first_then_by_priority_without_the_key() {
    let Ok(read) = servers(&list(), None) else { panic!("the list is read") };
    assert_eq!(
        names(&read),
        [
            ("Default", "rtmps://default.example/app"),
            ("East", "rtmp://east.example/app"),
            ("West", "rtmps://west.example/app"),
            ("noname.example", "rtmps://NoName.example/app"),
        ],
        "an unavailable server is left out, an address is listed once, and a name loses its control characters"
    );
    // Without a marked default, the service's documented server comes first.
    let body = serde_json::json!({ "ingests": [
        { "name": "A", "priority": 1, "url_template_secure": "rtmps://a.example/app/{stream_key}" },
        { "name": "B", "priority": 2, "url_template_secure": "rtmps://b.example/app/{stream_key}" },
    ] })
    .to_string();
    let Ok(read) = servers(body.as_bytes(), Some("rtmps://b.example/app")) else { panic!("the list is read") };
    assert_eq!(names(&read), [("B", "rtmps://b.example/app"), ("A", "rtmps://a.example/app")]);
    // A marked default comes before the documented server, whatever their priorities.
    let body = serde_json::json!({ "ingests": [
        { "name": "A", "priority": 1, "url_template_secure": "rtmps://a.example/app/{stream_key}" },
        { "name": "B", "priority": 2, "url_template_secure": "rtmps://b.example/app/{stream_key}" },
        { "name": "C", "priority": 9, "default": true, "url_template_secure": "rtmps://c.example/app/{stream_key}" },
    ] })
    .to_string();
    let Ok(read) = servers(body.as_bytes(), Some("rtmps://a.example/app")) else { panic!("the list is read") };
    assert_eq!(names(&read), [("C", "rtmps://c.example/app"), ("A", "rtmps://a.example/app"), ("B", "rtmps://b.example/app")]);
}

#[test]
fn hostile_entries_are_left_out_and_a_huge_list_is_cut() {
    let long = format!("rtmps://long.example/{}", "a".repeat(MAX_ADDRESS));
    let body = serde_json::json!({ "ingests": [
        { "url_template_secure": "https://web.example/app/{stream_key}" },
        { "url_template_secure": "rtmps://user@evil.example/app/{stream_key}" },
        { "url_template_secure": "rtmps://q.example/app?auth={stream_key}" },
        { "url_template_secure": "rtmps://frag.example/app#x" },
        { "url_template_secure": "rtmps://other.example/{region}/{stream_key}" },
        { "url_template_secure": "rtmps://noapp.example/{stream_key}" },
        { "url_template_secure": "rtmps://sp ace.example/app" },
        { "url_template_secure": "rtmps:///app" },
        { "url_template_secure": "rtmps://bad_host.example/app" },
        { "url_template_secure": long },
        { "url_template_secure": 7 },
        { "name": "x".repeat(10 * MAX_NAME), "url_template_secure": "rtmps://ok.example:443/app/{stream_key}" },
    ] })
    .to_string();
    let Ok(read) = servers(body.as_bytes(), None) else { panic!("the list is read") };
    assert_eq!(read.len(), 1, "{read:?}");
    assert_eq!(read[0].server, "rtmps://ok.example:443/app");
    assert_eq!(read[0].name.chars().count(), MAX_NAME);
    let many: Vec<_> = (0..MAX_INGESTS + 10).map(|i| serde_json::json!({ "url_template_secure": format!("rtmps://s{i}.example/app") })).collect();
    let body = serde_json::json!({ "ingests": many }).to_string();
    let Ok(read) = servers(body.as_bytes(), None) else { panic!("the list is read") };
    assert_eq!(read.len(), MAX_SERVERS);
    assert_eq!(servers(br#"{"ingests": []}"#, None), Ok(Vec::new()));
    assert!(servers(b"", None).is_err());
    assert!(servers(b"not json", None).is_err());
    assert!(servers(br#"{"other": []}"#, None).is_err());
    assert!(servers(br#"{"ingests": {}}"#, None).is_err());
}

#[test]
fn a_request_returns_at_once_while_the_fetch_is_late() {
    let (release, wait) = mpsc::channel::<()>();
    let wait = Mutex::new(wait);
    let calls = Arc::new(AtomicU32::new(0));
    let counted = Arc::clone(&calls);
    let lists = Lists::with(Arc::new(move |url: &str, limit: u64| {
        counted.fetch_add(1, Ordering::SeqCst);
        assert_eq!((url, limit), ("https://example.com/ingests", INGEST_LIST_LIMIT));
        // The fetch answers only when the test lets it.
        lock(&wait).recv_timeout(GUARD).map_err(|e| e.to_string())?;
        Ok(list())
    }));
    let (done, finished) = mpsc::channel::<()>();
    assert_eq!(lists.state("example"), None, "nothing is fetched before it is asked for");
    assert_eq!(
        lists.request(&SERVICE, false, move || {
            let _ = done.send(());
        }),
        Ok(ListState::Loading)
    );
    assert_eq!(lists.state("example"), Some(ListState::Loading), "the caller goes on while the fetch waits");
    // A second request while the first is late starts no second fetch and does not wait either.
    let (again, at_once) = mpsc::channel::<()>();
    assert_eq!(
        lists.request(&SERVICE, true, move || {
            let _ = again.send(());
        }),
        Ok(ListState::Loading)
    );
    assert_eq!(at_once.try_recv(), Ok(()), "a request that starts no fetch is done at once");
    release.send(()).unwrap();
    finished.recv_timeout(GUARD).unwrap();
    let Some(ListState::Ready(read)) = lists.state("example") else { panic!("the list is read") };
    assert_eq!(read.first().map(|s| s.server.as_str()), Some("rtmps://default.example/app"));
    // A list that was read is kept and not fetched again, even on a retry.
    assert!(matches!(lists.request(&SERVICE, false, || {}), Ok(ListState::Ready(_))));
    assert!(matches!(lists.request(&SERVICE, true, || {}), Ok(ListState::Ready(_))));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[test]
fn a_list_that_could_not_be_read_is_fetched_again_only_on_a_retry() {
    let calls = Arc::new(AtomicU32::new(0));
    let counted = Arc::clone(&calls);
    let lists = Lists::with(Arc::new(move |_: &str, _: u64| {
        if counted.fetch_add(1, Ordering::SeqCst) == 0 { Err("the server answered HTTP 503".into()) } else { Ok(b"{\"ingests\": 1}".to_vec()) }
    }));
    let request = |lists: &Arc<Lists>, retry: bool| {
        let (done, finished) = mpsc::channel::<()>();
        let answer = lists
            .request(&SERVICE, retry, move || {
                let _ = done.send(());
            })
            .unwrap();
        finished.recv_timeout(GUARD).unwrap();
        (answer, lists.state("example"))
    };
    let failed = ListState::Failed("the server answered HTTP 503".into());
    assert_eq!(request(&lists, false), (ListState::Loading, Some(failed.clone())));
    // Asking again without a retry answers with the failure and fetches nothing.
    assert_eq!(request(&lists, false), (failed.clone(), Some(failed)));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let no_list = ListState::Failed("the list of ingest servers has no `ingests` list".into());
    assert_eq!(request(&lists, true), (ListState::Loading, Some(no_list)));
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert!(lists.request(&NO_LIST, true, || {}).is_err(), "a service without a list is refused");
    assert_eq!(lists.state("nolist"), None);
}
