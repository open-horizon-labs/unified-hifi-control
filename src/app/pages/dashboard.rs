//! Dashboard page component.
//!
//! Shows service status overview using Dioxus resources for async data fetching.

use dioxus::prelude::*;

use crate::app::api::{AppStatus, HqpStatus, LmsStatus, RoonStatus};
use crate::app::components::Layout;
use crate::app::sse::use_sse;

/// Dashboard page component.
#[component]
pub fn Dashboard() -> Element {
    let sse = use_sse();

    // Use resources for async data fetching (handles SSR/client properly)
    let status = use_resource(|| async {
        crate::app::api::fetch_json::<AppStatus>("/status")
            .await
            .ok()
    });
    let mut roon = use_resource(|| async {
        crate::app::api::fetch_json::<RoonStatus>("/roon/status")
            .await
            .ok()
    });
    let mut hqp = use_resource(|| async {
        crate::app::api::fetch_json::<HqpStatus>("/hqp/status")
            .await
            .ok()
    });
    let mut lms = use_resource(|| async {
        crate::app::api::fetch_json::<LmsStatus>("/lms/status")
            .await
            .ok()
    });

    // Refresh on SSE events
    let event_count = sse.event_count;
    use_effect(move || {
        let _ = event_count();
        if sse.should_refresh_roon() || sse.should_refresh_hqp() || sse.should_refresh_lms() {
            roon.restart();
            hqp.restart();
            lms.restart();
        }
    });

    let is_loading = status.read().is_none() || roon.read().is_none();

    let status_content = if is_loading {
        rsx! {
            article { aria_busy: "true", "Loading status..." }
        }
    } else {
        let app_status = status.read().clone().flatten().unwrap_or_default();
        let roon_status = roon.read().clone().flatten().unwrap_or_default();
        let hqp_status = hqp.read().clone().flatten().unwrap_or_default();
        let lms_status = lms.read().clone().flatten().unwrap_or_default();

        rsx! {
            article {
                p { strong { "Version:" } " {app_status.version}" }
                p { strong { "Uptime:" } " {app_status.uptime_secs}s" }
                p { strong { "Event Bus Subscribers:" } " {app_status.bus_subscribers}" }
                hr {}
                table {
                    thead {
                        tr {
                            th { "Adapter" }
                            th { "Status" }
                            th { "Details" }
                        }
                    }
                    tbody {
                        // Roon row
                        tr {
                            td { "Roon" }
                            td {
                                class: if roon_status.connected { "status-ok" } else { "status-err" },
                                if roon_status.connected { "✓ Connected" } else { "✗ Disconnected" }
                            }
                            td {
                                small {
                                    if let Some(name) = &roon_status.core_name {
                                        "{name} "
                                    }
                                    if let Some(ver) = &roon_status.core_version {
                                        "v{ver}"
                                    }
                                }
                            }
                        }
                        // HQPlayer row
                        tr {
                            td { "HQPlayer" }
                            td {
                                class: if hqp_status.connected { "status-ok" } else { "status-err" },
                                if hqp_status.connected { "✓ Connected" } else { "✗ Disconnected" }
                            }
                            td {
                                small {
                                    if let Some(host) = &hqp_status.host {
                                        "{host}"
                                    }
                                }
                            }
                        }
                        // LMS row
                        tr {
                            td { "LMS" }
                            td {
                                class: if lms_status.connected { "status-ok" } else { "status-err" },
                                if lms_status.connected { "✓ Connected" } else { "✗ Disconnected" }
                            }
                            td {
                                small {
                                    if let (Some(host), Some(port)) = (&lms_status.host, lms_status.port) {
                                        "{host}:{port}"
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    };

    rsx! {
        Layout {
            title: "Dashboard".to_string(),
            nav_active: "dashboard".to_string(),

            h1 { "Dashboard" }

            section { id: "status",
                hgroup {
                    h2 { "Service Status" }
                    p { "Connection status for all adapters" }
                }
                {status_content}
            }
        }
    }
}
