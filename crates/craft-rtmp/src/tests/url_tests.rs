use crate::{Endpoint, Error, StreamKey};

#[test]
fn urls_take_apart_into_host_port_and_app() {
    let e = Endpoint::parse("rtmp://live.example.com/app").unwrap();
    assert_eq!((e.tls(), e.host(), e.port(), e.app()), (false, "live.example.com", 1935, "app"));
    assert_eq!(e.tc_url(), "rtmp://live.example.com/app");

    let e = Endpoint::parse("RTMPS://ingest.example.com:443/rtmp/").unwrap();
    assert_eq!((e.tls(), e.host(), e.port(), e.app()), (true, "ingest.example.com", 443, "rtmp"));
    assert_eq!(e.tc_url(), "rtmps://ingest.example.com:443/rtmp");

    let e = Endpoint::parse("rtmps://ingest.example.com/live2").unwrap();
    assert_eq!(e.port(), 443);

    let e = Endpoint::parse("rtmp://[::1]:1936/app/instance?token=abc").unwrap();
    assert_eq!((e.host(), e.port(), e.app()), ("::1", 1936, "app/instance?token=abc"));
    assert_eq!(e.tc_url(), "rtmp://[::1]:1936/app/instance?token=abc");

    let e = Endpoint::parse("rtmp://10.0.0.2/live").unwrap();
    assert_eq!(e.host(), "10.0.0.2");
}

#[test]
fn bad_urls_are_refused() {
    for url in [
        "http://example.com/app",
        "rtmpt://example.com/app",
        "example.com/app",
        "rtmp:///app",
        "rtmp://example.com",
        "rtmp://example.com/",
        "rtmp://example.com/?x=1",
        "rtmp://user:pass@example.com/app",
        "rtmp://example.com:0/app",
        "rtmp://example.com:70000/app",
        "rtmp://example.com:/app",
        "rtmp://example.com:x/app",
        "rtmp://::1/app",
        "rtmp://[::1/app",
        "rtmp://[::1]x/app",
        "rtmp://exa mple.com/app",
        "rtmp://example.com/app\n",
    ] {
        assert!(matches!(Endpoint::parse(url), Err(Error::BadUrl(_))), "{url}");
    }
}

#[test]
fn debug_output_leaves_out_the_query() {
    let e = Endpoint::parse("rtmp://example.com/app?token=s3cret").unwrap();
    let d = format!("{e:?}");
    assert!(!d.contains("s3cret"), "{d}");
    assert!(d.contains("app?<redacted>"), "{d}");
}

#[test]
fn keys_are_checked_and_never_printed() {
    let k = StreamKey::new("live_123?bandwidthtest=true&x=1").unwrap();
    assert_eq!(k.expose(), "live_123?bandwidthtest=true&x=1");
    assert_eq!(format!("{k:?}"), "StreamKey(<redacted>)");
    for bad in [String::new(), "a b".into(), "a\tb".into(), "ab\u{7}".into(), "k".repeat(65_536)] {
        let e = StreamKey::new(bad.clone()).unwrap_err();
        assert!(matches!(e, Error::BadKey(_)));
        if !bad.is_empty() {
            assert!(!e.to_string().contains(&bad));
        }
    }
    assert!(StreamKey::new("k".repeat(65_535)).is_ok());
}
