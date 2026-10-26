//! Decisions the Controllers page makes about one controller, kept free of rendering so they can
//! be tested directly. The server enforces the same rules; the page only decides what to offer.

use crate::device_settings::PowerSupport;
use crate::firmware_catalog::{firmware_version_is_newer, FirmwareChannel, FirmwareTarget};

/// What the page can say about a controller's firmware.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateState {
    /// The page cannot tell, with the reason to show.
    Unknown(&'static str),
    UpToDate,
    /// A newer image for the controller's own channel is on the bridge.
    Available(String),
}

/// The firmware feed to ask about for this controller: its catalog slug and its own channel.
/// `None` when it has not reported a type and a version in the published grammar.
pub fn update_feed(
    device_type: Option<&str>,
    running: Option<&str>,
) -> Option<(&'static str, &'static str)> {
    let target = FirmwareTarget::parse(device_type?)?;
    let channel = FirmwareChannel::for_version(running?)?;
    Some((target.slug(), channel.slug()))
}

/// What asking the bridge for a controller's feed produced. Absence of an answer is never read as
/// "nothing newer": only an image the bridge actually reported can make a controller up to date.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeedState {
    /// No answer yet.
    Loading,
    /// The bridge holds this version on the controller's channel.
    Image(String),
    /// The bridge has no image for it, or the request failed.
    Unavailable,
}

pub fn update_state(
    device_type: Option<&str>,
    running: Option<&str>,
    feed: &FeedState,
) -> UpdateState {
    let Some(running_version) = running else {
        return UpdateState::Unknown("Version not reported");
    };
    if device_type.and_then(FirmwareTarget::parse).is_none() {
        return UpdateState::Unknown("Controller type not reported");
    }
    if update_feed(device_type, running).is_none() {
        return UpdateState::Unknown("Local build, not on a release channel");
    }
    match feed {
        FeedState::Loading => UpdateState::Unknown("Checking for updates…"),
        FeedState::Unavailable => UpdateState::Unknown(
            "Update check unavailable: no image on this Bridge, or the check failed",
        ),
        FeedState::Image(version) if firmware_version_is_newer(version, running_version) => {
            UpdateState::Available(version.clone())
        }
        FeedState::Image(_) => UpdateState::UpToDate,
    }
}

/// Link to the controller's own web page, from the address it reported. Only a literal IP address
/// becomes a link, so a hostile report can never put arbitrary text into an `href`.
pub fn device_page_url(ip: Option<&str>) -> Option<String> {
    let address: std::net::IpAddr = ip?.trim().parse().ok()?;
    Some(match address {
        std::net::IpAddr::V4(v4) => format!("http://{v4}/"),
        std::net::IpAddr::V6(v6) => format!("http://[{v6}]/"),
    })
}

/// Which of the shared display and power controls to show for this controller.
pub fn power_support(device_type: Option<&str>) -> PowerSupport {
    // An unknown controller keeps the full form: that is what the page showed before types existed.
    device_type
        .and_then(FirmwareTarget::parse)
        .map_or(PowerSupport::Full, crate::device_settings::power_support)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_controller_asks_the_feed_for_its_own_type_and_channel() {
        assert_eq!(
            update_feed(Some("hiphi-frame"), Some("2.8.0-alpha.4")),
            Some(("frame", "alpha"))
        );
        assert_eq!(
            update_feed(Some("halo"), Some("2.8.0")),
            Some(("halo", "stable"))
        );
        assert_eq!(update_feed(Some("frame"), Some("2.8.0-dev")), None);
        assert_eq!(update_feed(None, Some("2.8.0")), None);
        assert_eq!(update_feed(Some("frame"), None), None);
        assert_eq!(update_feed(Some("../frame"), Some("2.8.0")), None);
    }

    #[test]
    fn an_update_is_offered_only_for_a_newer_image_on_the_same_channel() {
        let state = |version: &str| {
            update_state(
                Some("frame"),
                Some("2.8.0-alpha.4"),
                &FeedState::Image(version.into()),
            )
        };
        assert_eq!(
            state("2.8.0-alpha.5"),
            UpdateState::Available("2.8.0-alpha.5".into())
        );
        assert_eq!(state("2.8.0-alpha.4"), UpdateState::UpToDate);
        assert_eq!(state("2.8.0-alpha.3"), UpdateState::UpToDate);
    }

    #[test]
    fn a_missing_answer_is_never_reported_as_up_to_date() {
        // Empty cache, failed request, request still in flight: none of these is a freshness claim.
        for feed in [FeedState::Loading, FeedState::Unavailable] {
            let state = update_state(Some("frame"), Some("2.8.0-alpha.4"), &feed);
            assert!(
                matches!(state, UpdateState::Unknown(_)),
                "{feed:?} -> {state:?}"
            );
        }
    }

    #[test]
    fn unknowns_say_why_instead_of_offering_an_update() {
        assert!(matches!(
            update_state(Some("frame"), None, &FeedState::Image("9.9.9".into())),
            UpdateState::Unknown(_)
        ));
        assert!(matches!(
            update_state(None, Some("2.8.0"), &FeedState::Image("9.9.9".into())),
            UpdateState::Unknown(_)
        ));
        assert!(matches!(
            update_state(
                Some("future-model"),
                Some("2.8.0"),
                &FeedState::Image("9.9.9".into())
            ),
            UpdateState::Unknown(_)
        ));
        assert!(matches!(
            update_state(
                Some("frame"),
                Some("2.8.0-dev"),
                &FeedState::Image("9.9.9".into())
            ),
            UpdateState::Unknown(_)
        ));
    }

    #[test]
    fn only_a_literal_address_becomes_a_device_link() {
        assert_eq!(
            device_page_url(Some("192.168.1.40")),
            Some("http://192.168.1.40/".into())
        );
        assert_eq!(
            device_page_url(Some(" 10.0.0.2 ")),
            Some("http://10.0.0.2/".into())
        );
        assert_eq!(
            device_page_url(Some("fe80::1")),
            Some("http://[fe80::1]/".into())
        );
        for bad in [
            "",
            "—",
            "javascript:alert(1)",
            "evil.example/x",
            "1.2.3.4/../x",
        ] {
            assert_eq!(device_page_url(Some(bad)), None, "{bad}");
        }
        assert_eq!(device_page_url(None), None);
    }

    #[test]
    fn power_controls_follow_the_controller_and_default_to_the_full_form() {
        assert_eq!(power_support(Some("frame")), PowerSupport::None);
        assert_eq!(power_support(Some("hiphi-rlcd")), PowerSupport::ArtOnly);
        assert_eq!(power_support(Some("dial")), PowerSupport::Full);
        assert_eq!(power_support(None), PowerSupport::Full);
        assert_eq!(power_support(Some("future-model")), PowerSupport::Full);
    }
}
