//! One live connection status for Settings and the app-wide recovery notice.
use super::api::{self, HiphiPairingStatus};
use dioxus::prelude::*;

const HIPHI_STATUS_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10);

#[derive(Clone, Copy)]
pub struct CloudConnection {
    pub status: Resource<Result<HiphiPairingStatus, String>>,
    last_confirmed: Signal<Option<HiphiPairingStatus>>,
    busy: Signal<bool>,
    error: Signal<Option<String>>,
    attempted: Signal<bool>,
}

pub fn use_cloud_connection_provider() {
    let mut last_confirmed = use_signal(|| None);
    let mut status = use_resource(move || async move {
        let result = api::fetch_json::<HiphiPairingStatus>("/api/hiphi/pairing/status").await;
        if let Ok(confirmed) = &result {
            last_confirmed.set(Some(confirmed.clone()));
        }
        result
    });
    let busy = use_signal(|| false);
    let error = use_signal(|| None);
    let attempted = use_signal(|| false);
    use_future(move || async move {
        loop {
            dioxus_sdk_time::sleep(HIPHI_STATUS_POLL_INTERVAL).await;
            status.restart();
        }
    });
    use_context_provider(|| CloudConnection {
        status,
        last_confirmed,
        busy,
        error,
        attempted,
    });
}

pub fn use_cloud_connection() -> CloudConnection {
    use_context::<CloudConnection>()
}

impl CloudConnection {
    fn reconnect(mut self) {
        if (self.busy)() {
            return;
        }
        self.busy.set(true);
        self.error.set(None);
        spawn(async move {
            match api::post_json::<serde_json::Value, HiphiPairingStatus>(
                "/api/hiphi/connection/resume",
                &serde_json::json!({}),
            )
            .await
            {
                Ok(status) => {
                    self.last_confirmed.set(Some(status));
                    self.attempted.set(true);
                }
                Err(message) => self
                    .error
                    .set(api::suppress_controller_unauthorized(message)),
            }
            self.status.restart();
            self.busy.set(false);
        });
    }
}

/// Keep the notice visible on every page until connection state actually
/// changes. An accepted POST is not evidence of a working cloud connection.
#[component]
pub fn CloudConnectionNotice(#[props(default = false)] standalone: bool) -> Element {
    let connection = use_cloud_connection();
    // A failed refresh must not erase an already observed outage notice.
    let refresh_failed = matches!(&*connection.status.read(), Some(Err(_)));
    let Some(status) = (connection.last_confirmed)() else {
        return rsx! {};
    };
    let waiting = (connection.attempted)() && status.connector_state != "online";
    if !status.paired || (status.pause_reason.is_none() && !waiting) {
        return rsx! {};
    }
    rsx! {
        div { class: if standalone { "card p-5 mb-4" } else { "mt-3" },
            CloudRecoveryView {
                status, busy: (connection.busy)(), error: (connection.error)(),
                on_reconnect: move |_| connection.reconnect(),
            }
            if refresh_failed {
                p { class: "mt-2 text-sm", role: "status", "UHC couldn’t refresh the connection status. Showing the last confirmed status; checking again automatically." }
            }
        }
    }
}

#[component]
pub fn CloudRecoveryView(
    status: HiphiPairingStatus,
    busy: bool,
    error: Option<String>,
    on_reconnect: EventHandler<()>,
) -> Element {
    let permissions_changed = status.pause_reason.as_deref() == Some("permissions_changed");
    let cost_limit = status.pause_reason.as_deref() == Some("cost_limit");
    let waiting = status.pause_reason.is_none()
        && matches!(status.connector_state.as_str(), "connecting" | "offline");
    rsx! {
        div { role: "status", aria_live: "polite",
            p { class: "font-semibold",
                if waiting { "Reconnecting to HiPhi Cloud…" }
                else { "Remote control is disconnected" }
            }
            if permissions_changed {
                p { class: "mt-2 text-sm",
                    "Access to UHC’s saved connection files changed, so UHC stopped its Cloud connection. Garmin and other remote controls won’t work until you reconnect."
                }
                p { class: "mt-2 text-sm",
                    "Reconnect makes these files private to UHC again and tries the connection again. You won’t need to pair again. Local playback is unaffected."
                }
            } else if cost_limit {
                p { class: "mt-2 text-sm",
                    "Cloud activity reached a usage limit. Once the Cloud issue is resolved, you can resume the connection. Local playback is unaffected."
                }
            } else if status.connector_state == "revoked" {
                p { class: "mt-2 text-sm", "This installation’s Cloud access was removed. Open HiPhi Cloud to check its access. Local playback is unaffected." }
            } else if waiting {
                p { class: "mt-2 text-sm",
                    "UHC is trying to connect. Remote control will be available once HiPhi Cloud responds. If Cloud is unavailable, UHC will keep trying automatically."
                }
            } else {
                p { class: "mt-2 text-sm",
                    "UHC couldn’t read its saved connection files. Local playback is unaffected. Check that this device’s storage is available and UHC can access its files, or ask the person who manages this device for help."
                }
            }
        }
        if permissions_changed || cost_limit {
            button {
                r#type: "button", class: "btn btn-primary mt-3",
                disabled: busy || !status.can_resume,
                onclick: move |_| on_reconnect.call(()),
                if busy { "Reconnecting…" }
                else if permissions_changed { "Reconnect" }
                else { "Resume Cloud connection" }
            }
            if cost_limit && !status.can_resume {
                p { class: "mt-2 text-sm", "Please wait 15 minutes between attempts to resume after a usage limit." }
            }
        }
        if let Some(message) = error {
            div { class: "mt-3", role: "alert",
                p { class: "status-err",
                    "UHC couldn’t reconnect. If it cannot update the files, check this device’s access settings or ask its administrator for help."
                }
                details { class: "mt-2 text-sm text-secondary",
                    summary { "Technical details" }
                    p { class: "mt-2 break-all", "{message}" }
                }
            }
        }
    }
}

#[cfg(all(test, feature = "server"))]
mod tests {
    use super::*;

    #[derive(Props, Clone, PartialEq)]
    struct PreviewProps {
        status: HiphiPairingStatus,
        busy: bool,
        error: Option<String>,
    }
    fn preview(props: PreviewProps) -> Element {
        rsx! { CloudRecoveryView { status: props.status, busy: props.busy, error: props.error, on_reconnect: |_| {} } }
    }

    fn render(reason: &str, can_resume: bool, busy: bool, error: Option<String>) -> String {
        let status = HiphiPairingStatus {
            paired: true,
            installation_id: Some("example-installation".into()),
            connector_state: "paused".into(),
            pause_reason: Some(reason.into()),
            can_resume,
        };
        let mut dom = VirtualDom::new_with_props(
            preview,
            PreviewProps {
                status,
                busy,
                error,
            },
        );
        dom.rebuild_in_place();
        dioxus::ssr::render(&dom)
    }

    #[test]
    fn permission_notice_explains_impact_and_offers_reconnect_without_pairing() {
        let html = render("permissions_changed", true, false, None);
        assert!(html.contains("Remote control is disconnected"));
        assert!(html.contains("Garmin and other remote controls"));
        assert!(html.contains("You won’t need to pair again"));
        assert!(html.contains(">Reconnect</button>"));
        assert!(!html.contains("disabled"));
        assert!(html.contains(r#"aria-live="polite""#));
        assert!(!html.contains("replay") && !html.contains("chmod"));
    }

    #[test]
    fn recovery_view_handles_busy_damaged_and_failed_states_honestly() {
        let html = render("permissions_changed", true, true, None);
        assert!(html.contains("disabled") && html.contains("Reconnecting…"));
        let html = render("safety_state_unavailable", false, false, None);
        assert!(!html.contains("<button"));
        assert!(!html.contains("Access to UHC’s saved connection files changed"));
        let html = render(
            "permissions_changed",
            true,
            false,
            Some("Example access failure".into()),
        );
        assert!(html.contains(r#"role="alert""#));
        assert!(html.contains("Technical details") && html.contains("Example access failure"));
        assert!(!html.contains("Connected to HiPhi Cloud"));
    }
}
