import Toybox.Application;
import Toybox.Communications;
import Toybox.Lang;

//! The whole server contract, in one place.
//!
//! v1 deliberately adds NOTHING to the server: it speaks the controller API
//! that already exists for hardware knobs, verified against both `v3` and a
//! running 3.7.0-alpha build:
//!
//!   GET  /zones    -> {"zones":[{zone_id, zone_name, source, state,
//!                                volume_control:{value,min,max,step,is_muted}}]}
//!   POST /control  -> {zone_id, action}  where action is one of
//!                     play_pause | next | previous | volume_up | volume_down
//!
//! Roughly 3.2 KB for ten zones against a 768 KB app budget, so size is a
//! non-issue; the reason this fetches on open and after an action rather than
//! polling on a timer is Bluetooth latency and battery, not memory.
//!
//! Monkey C has no argument binding on a `Method`, so each in-flight request
//! is a small object that holds its own callback. That is the idiomatic shape
//! here, not ceremony.
module UhcApi {

    // Actions, matching the server's vocabulary exactly. Constants so a typo
    // is a compile error rather than a 400 the user experiences as "nothing
    // happened".
    const ACTION_PLAY_PAUSE = "play_pause";
    const ACTION_NEXT = "next";
    const ACTION_PREVIOUS = "previous";
    const ACTION_VOLUME_UP = "volume_up";
    const ACTION_VOLUME_DOWN = "volume_down";

    // Distinguishable failures. Each earns different advice: a bad address is
    // a settings problem, a rejected token is a server problem, and anything
    // else is usually "the phone wandered off".
    const ERR_NO_SERVER = 1;
    const ERR_UNREACHABLE = 2;
    const ERR_UNAUTHORIZED = 3;

    //! The configured base URL without a trailing slash, or null if unset.
    function baseUrl() as String? {
        var raw = Application.Properties.getValue("baseUrl");
        if (raw == null || !(raw instanceof String)) {
            return null;
        }
        var url = trim(raw);
        while (url.length() > 0 && url.substring(url.length() - 1, url.length()).equals("/")) {
            url = url.substring(0, url.length() - 1);
        }
        return url.length() == 0 ? null : url;
    }

    //! Monkey C's String has no trim(), so this is it.
    function trim(value as String) as String {
        var start = 0;
        var end = value.length();
        while (start < end && isBlank(value.substring(start, start + 1))) {
            start += 1;
        }
        while (end > start && isBlank(value.substring(end - 1, end))) {
            end -= 1;
        }
        return value.substring(start, end);
    }

    function isBlank(ch as String) as Boolean {
        return ch.equals(" ") || ch.equals("\t") || ch.equals("\n") || ch.equals("\r");
    }

    //! Request headers. An empty token must send NO Authorization header
    //! rather than an empty one — a blank bearer is rejected by some proxies,
    //! which would surface as a server fault the user cannot act on.
    function headers(
        contentType as Communications.HttpRequestContentType?
    ) as Dictionary {
        var result = { "Accept" => "application/json" } as Dictionary;
        if (contentType != null) {
            result["Content-Type"] = contentType;
        }
        var token = Application.Properties.getValue("token");
        if (token != null && token instanceof String && trim(token).length() > 0) {
            result["Authorization"] = "Bearer " + trim(token);
        }
        return result;
    }

    //! Map a response code onto the three failures worth telling apart. From
    //! the wrist, the difference between a DNS failure and a 500 is not
    //! actionable, so both are "unreachable".
    function classify(responseCode as Number) as Number? {
        if (responseCode == 200) {
            return null;
        }
        if (responseCode == 401 || responseCode == 403) {
            return ERR_UNAUTHORIZED;
        }
        return ERR_UNREACHABLE;
    }
}

//! GET /zones. The callback receives (errorOrNull, arrayOfRawZonesOrNull).
class ZonesRequest {
    private var _callback as Method;

    public function initialize(callback as Method) {
        _callback = callback;
    }

    public function start() as Void {
        var base = UhcApi.baseUrl();
        if (base == null) {
            _callback.invoke(UhcApi.ERR_NO_SERVER, null);
            return;
        }
        Communications.makeWebRequest(
            base + "/zones",
            null,
            {
                :method => Communications.HTTP_REQUEST_METHOD_GET,
                :headers => UhcApi.headers(null),
                :responseType => Communications.HTTP_RESPONSE_CONTENT_TYPE_JSON
            },
            method(:onResponse)
        );
    }

    public function onResponse(responseCode as Number, data as Dictionary?) as Void {
        var failure = UhcApi.classify(responseCode);
        if (failure != null) {
            _callback.invoke(failure, null);
            return;
        }
        if (data == null || !(data instanceof Dictionary)) {
            _callback.invoke(UhcApi.ERR_UNREACHABLE, null);
            return;
        }
        var zones = data["zones"];
        if (zones == null || !(zones instanceof Array)) {
            // A 200 with an unfamiliar body is a server we do not understand,
            // not an empty house. Saying "no zones" there would be a lie.
            _callback.invoke(UhcApi.ERR_UNREACHABLE, null);
            return;
        }
        _callback.invoke(null, zones);
    }
}

//! POST /control. The callback receives (errorOrNull); the reply body carries
//! nothing worth reading, so callers refresh rather than trusting it.
class ControlRequest {
    private var _callback as Method;

    public function initialize(callback as Method) {
        _callback = callback;
    }

    public function start(zoneId as String, action as String) as Void {
        var base = UhcApi.baseUrl();
        if (base == null) {
            _callback.invoke(UhcApi.ERR_NO_SERVER);
            return;
        }
        Communications.makeWebRequest(
            base + "/control",
            { "zone_id" => zoneId, "action" => action },
            {
                :method => Communications.HTTP_REQUEST_METHOD_POST,
                :headers => UhcApi.headers(Communications.REQUEST_CONTENT_TYPE_JSON),
                :responseType => Communications.HTTP_RESPONSE_CONTENT_TYPE_JSON
            },
            method(:onResponse)
        );
    }

    public function onResponse(responseCode as Number, data as Dictionary?) as Void {
        _callback.invoke(UhcApi.classify(responseCode));
    }
}

//! GET /now_playing?zone_id=... — what a zone is actually playing.
//! Separate from /zones because the zone list only needs names and state;
//! fetching track text for ten zones to show one would be wasteful over
//! Bluetooth.
class NowPlayingRequest {
    private var _callback as Method;

    public function initialize(callback as Method) {
        _callback = callback;
    }

    public function start(zoneId as String) as Void {
        var base = UhcApi.baseUrl();
        if (base == null) {
            _callback.invoke(UhcApi.ERR_NO_SERVER, null);
            return;
        }
        Communications.makeWebRequest(
            base + "/now_playing",
            { "zone_id" => zoneId },
            {
                :method => Communications.HTTP_REQUEST_METHOD_GET,
                :headers => UhcApi.headers(null),
                :responseType => Communications.HTTP_RESPONSE_CONTENT_TYPE_JSON
            },
            method(:onResponse)
        );
    }

    public function onResponse(responseCode as Number, data as Dictionary?) as Void {
        var failure = UhcApi.classify(responseCode);
        if (failure != null || data == null || !(data instanceof Dictionary)) {
            _callback.invoke(failure == null ? UhcApi.ERR_UNREACHABLE : failure, null);
            return;
        }
        _callback.invoke(null, data);
    }
}
