//! The streaming services a stream destination can be set up for: each service's name and, where
//! the service documents one ingest server for every account, that server. A service that gives
//! each account or event its own server has none here, and the user copies it from the service.
//!
//! Each server comes from the page in [`Service::source`]. A service that publishes a list of its
//! ingest servers or a status summary in a machine-readable form has their addresses here too,
//! for the connection check and, for the list, the Streaming window's choice of ingest server.
//! The names are the services' own
//! (AGENTS.md 1.3), and the window shows a service's logo only where the service allows it
//! (AGENTS.md 1.2, `ASSETS.md`).

use crate::profile::Destination;

/// A streaming service with a preset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Service {
    /// Names the service in a destination's [`Destination::service`]; stable once saved.
    pub id: &'static str,
    /// The name the window shows.
    pub name: &'static str,
    /// The ingest server the service documents for every account; `None` when the service shows
    /// each account its own.
    pub server: Option<&'static str>,
    /// The service's public page that gives the server, or says where the account's server and
    /// key are shown.
    pub source: &'static str,
    /// The service's machine-readable list of its ingest servers, which a connection check reads
    /// to try another of them and the Streaming window offers as a choice of server; `None` when
    /// the service publishes none.
    pub ingests: Option<&'static str>,
    /// The service's machine-readable status summary, which a connection check reads; `None` when
    /// the service publishes none.
    pub status: Option<&'static str>,
    /// The component of [`Service::status`] that covers sending a stream to the service.
    pub status_component: Option<&'static str>,
}

/// Every service with a preset, in the order the window lists them.
pub const SERVICES: &[Service] = &[
    Service {
        id: "twitch",
        name: "Twitch",
        // The "Default" entry of the ingest list (`url_template_secure`), which the video
        // broadcast docs point to.
        server: Some("rtmps://ingest.global-contribute.live-video.net/app"),
        source: "https://ingest.twitch.tv/ingests",
        ingests: Some("https://ingest.twitch.tv/ingests"),
        // The status page's summary, documented at https://status.twitch.com/api.
        status: Some("https://status.twitch.com/api/v2/summary.json"),
        status_component: Some("Video (Broadcasting)"),
    },
    Service {
        id: "youtube",
        name: "YouTube",
        server: None,
        source: "https://support.google.com/youtube/answer/10364924",
        ingests: None,
        status: None,
        status_component: None,
    },
    Service {
        id: "facebook",
        name: "Facebook Live",
        server: Some("rtmps://rtmp-api.facebook.com/rtmp/"),
        source: "https://developers.facebook.com/docs/live-video-api/getting-started",
        ingests: None,
        status: None,
        status_component: None,
    },
    Service {
        id: "kick",
        name: "Kick",
        server: None,
        source: "https://help.kick.com/en/articles/7066931-how-to-stream-on-kick-com",
        ingests: None,
        status: None,
        status_component: None,
    },
    Service {
        id: "x",
        name: "X",
        server: None,
        source: "https://help.x.com/en/using-x/live-studio",
        ingests: None,
        status: None,
        status_component: None,
    },
    Service {
        id: "linkedin",
        name: "LinkedIn Live",
        server: None,
        source: "https://www.linkedin.com/help/linkedin/answer/128901",
        ingests: None,
        status: None,
        status_component: None,
    },
    Service {
        id: "tiktok",
        name: "TikTok LIVE",
        server: None,
        source: "https://www.tiktok.com/live/creator-networks/en/blog/how-to-go-live-on-tiktok",
        ingests: None,
        status: None,
        status_component: None,
    },
];

/// The service with id `id`; `None` for an empty or unknown id.
pub fn service(id: &str) -> Option<&'static Service> {
    SERVICES.iter().find(|s| s.id == id)
}

/// Set `d` up for the service `id` (empty for a custom server). A server that is empty or equal to
/// any preset's server is replaced by the new preset's; a service that gives each account its own
/// server clears it and turns the destination off. Every other server is kept, and so is every
/// server on a change to a custom server. Which preset filled a server in is not recorded, so a
/// server equal to a preset's counts as filled in by a preset.
pub fn set_service(d: &mut Destination, id: &str) {
    // Any preset's server counts as the old preset's, also after a change through Custom server.
    let preset_server = SERVICES.iter().filter_map(|svc| svc.server).any(|server| d.server.trim() == server);
    let replaceable = d.server.trim().is_empty() || preset_server;
    let preset = service(id);
    if let Some(next) = preset.and_then(|svc| svc.server) {
        if replaceable {
            d.server = next.to_string();
        }
    } else if preset.is_some() && replaceable {
        d.server.clear();
        d.enabled = false;
    }
    d.service = id.to_string();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_service_has_a_unique_id_a_name_and_a_public_source() {
        for (i, s) in SERVICES.iter().enumerate() {
            assert!(!s.id.is_empty() && s.id.bytes().all(|b| b.is_ascii_lowercase()), "{s:?}");
            assert!(!s.name.trim().is_empty(), "{s:?}");
            assert!(s.source.starts_with("https://"), "{s:?}");
            for url in [s.ingests, s.status].into_iter().flatten() {
                assert!(url.starts_with("https://") && !url.contains('?'), "{s:?}");
            }
            assert_eq!(s.status.is_some(), s.status_component.is_some(), "{s:?}: a status summary names its component");
            assert!(SERVICES[..i].iter().all(|o| o.id != s.id && o.name != s.name), "{s:?} is listed twice");
            assert_eq!(service(s.id), Some(s));
        }
        assert_eq!(service(""), None);
        assert_eq!(service("TWITCH"), None, "ids match as saved");
        assert_eq!(service("unknown"), None);
    }

    #[test]
    fn documented_servers_are_rtmps_addresses_with_an_application() {
        let servers: Vec<&str> = SERVICES.iter().filter_map(|s| s.server).collect();
        assert_eq!(servers.len(), 2, "{servers:?}");
        for server in servers {
            let rest = server.strip_prefix("rtmps://").unwrap_or_else(|| panic!("{server} is not RTMPS"));
            let (host, app) = rest.split_once('/').unwrap_or_else(|| panic!("{server} has no application"));
            assert!(host.contains('.') && !host.contains(':'), "{server}");
            assert!(!app.trim_end_matches('/').is_empty() && !app.contains('?'), "{server}");
        }
    }

    #[test]
    fn a_new_destination_set_up_for_a_service_takes_its_server() {
        let Some(twitch) = service("twitch") else { panic!("no twitch preset") };
        let mut d = Destination { id: "a".into(), name: "Mine".into(), ..Destination::default() };
        set_service(&mut d, "twitch");
        assert_eq!((d.name.as_str(), d.service.as_str()), ("Mine", "twitch"), "the name is kept");
        assert_eq!(Some(d.server.as_str()), twitch.server);
        assert!(d.enabled);
        let mut d = Destination::default();
        set_service(&mut d, "youtube");
        assert_eq!((d.server.as_str(), d.service.as_str()), ("", "youtube"), "the user copies the account's server");
    }

    #[test]
    fn changing_the_service_keeps_a_typed_server_and_never_keeps_another_presets() {
        let Some(twitch) = service("twitch") else { panic!("no twitch preset") };
        let Some(facebook) = service("facebook") else { panic!("no facebook preset") };
        let mut d = Destination::default();
        set_service(&mut d, "twitch");
        set_service(&mut d, "youtube");
        assert_eq!((d.server.as_str(), d.enabled, d.service.as_str()), ("", false, "youtube"), "the old preset's server is not kept");
        set_service(&mut d, "facebook");
        assert_eq!(Some(d.server.as_str()), facebook.server);
        assert!(!d.enabled, "the destination stays off until it is turned on");
        set_service(&mut d, "twitch");
        assert_eq!(Some(d.server.as_str()), twitch.server, "one preset's server replaces another's");
        set_service(&mut d, "");
        assert_eq!(Some(d.server.as_str()), twitch.server, "a custom server keeps the address");
        set_service(&mut d, "facebook");
        assert_eq!(Some(d.server.as_str()), facebook.server, "a preset's server is replaced after Custom server too");
        d.server = "rtmp://mine.example/live".into();
        d.enabled = true;
        for id in ["youtube", "twitch", ""] {
            set_service(&mut d, id);
            assert_eq!((d.server.as_str(), d.enabled), ("rtmp://mine.example/live", true), "{id}: a typed server is kept");
        }
    }
}
