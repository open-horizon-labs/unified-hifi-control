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
//! Roughly 3.2 KB for ten zones, which is why this fetches on open and after
//! an action rather than polling on a timer: over Bluetooth a periodic
//! multi-kilobyte poll is a battery complaint, not a feature.
class UhcApi {

    // Actions, matching the server's vocabulary exactly. Kept as constants so
    // a typo is a compile error rather than a 400 the user sees as "nothing
    // happened".
    public static const ACTION_PLAY_PAUSE = "play_pause";
    public static const ACTION_NEXT = "next";
    public static const ACTION_PREVIOUS = "previous";
    public static const ACTION_VOLUME_UP = "volume_up";
    public static const ACTION_VOLUME_DOWN = "volume_down";

    // Distinguishable failures. The user gets different advice for each: a
    // bad address is a settings problem, a rejected token is a server
    // problem, and a timeout is usually "the phone wandered off".
    public static const ERR_NO_SERVER = 1;
    public static const ERR_UNREACHABLE = 2;
    public static const ERR_UNAUTHORIZED = 3;

    //! The configured base URL with any trailing slash removed, or null when
    //! the user has not set one yet.
    public static function baseUrl() as String? {
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
    private static function trim(value as String) as String {
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

    private static function isBlank(ch as String) as Boolean {
        return ch.equals(" ") || ch.equals("\t") || ch.equals("\n") || ch.equals("\r");
    }

    //! Bearer header, only when a token is configured. An empty token must
    //! send NO header rather than an empty one: a blank `Authorization` is
    //! rejected by some proxies, which would look like a server fault.
    private static function headers() as Dictionary {
        var result = {
            "Accept" => "application/json"
        };
        var token = Application.Properties.getValue("token");
        if (token != null && token instanceof String && trim(token).length() > 0) {
            result["Authorization"] = "Bearer " + trim(token);
        }
        return result;
    }

    //! GET /zones. `callback` receives (errorOrNull, zonesArrayOrNull).
    public static function fetchZones(callback as Method) as Void {
        var base = baseUrl();
        if (base == null) {
            callback.invoke(ERR_NO_SERVER, null);
            return;
        }
        var options = {
            :method => Communications.HTTP_REQUEST_METHOD_GET,
            :headers => headers(),
            :responseType => Communications.HTTP_RESPONSE_CONTENT_TYPE_JSON
        };
        Communications.makeWebRequest(
            base + "/zones",
            null,
            options,
            method(:onZones).bind(callback)
        );
    }

    //! POST /control. `callback` receives (errorOrNull) — the body carries no
    //! state worth reading, so callers refresh instead of trusting a reply.
    public static function sendControl(
        zoneId as String,
        action as String,
        callback as Method
    ) as Void {
        var base = baseUrl();
        if (base == null) {
            callback.invoke(ERR_NO_SERVER);
            return;
        }
        var options = {
            :method => Communications.HTTP_REQUEST_METHOD_POST,
            :headers => {
                "Content-Type" => Communications.REQUEST_CONTENT_TYPE_JSON,
                "Accept" => "application/json"
            },
            :responseType => Communications.HTTP_RESPONSE_CONTENT_TYPE_JSON
        };
        var token = Application.Properties.getValue("token");
        if (token != null && token instanceof String && trim(token).length() > 0) {
            options[:headers]["Authorization"] = "Bearer " + trim(token);
        }
        Communications.makeWebRequest(
            base + "/control",
            { "zone_id" => zoneId, "action" => action },
            options,
            method(:onControl).bind(callback)
        );
    }

    private static function onZones(
        callback as Method,
        responseCode as Number,
        data as Dictionary?
    ) as Void {
        var failure = classify(responseCode);
        if (failure != null) {
            callback.invoke(failure, null);
            return;
        }
        if (data == null || !(data instanceof Dictionary)) {
            callback.invoke(ERR_UNREACHABLE, null);
            return;
        }
        var zones = data["zones"];
        if (zones == null || !(zones instanceof Array)) {
            // A 200 with an unfamiliar body is a server we do not understand,
            // not an empty house. Say unreachable rather than "no zones".
            callback.invoke(ERR_UNREACHABLE, null);
            return;
        }
        callback.invoke(null, zones);
    }

    private static function onControl(
        callback as Method,
        responseCode as Number,
        data as Dictionary?
    ) as Void {
        callback.invoke(classify(responseCode));
    }

    //! Map an HTTP/BLE response code onto the three failures worth telling
    //! them apart. Anything non-200 that is not an auth rejection is
    //! "unreachable" — from the wrist, the distinction between a DNS failure
    //! and a 500 is not actionable.
    private static function classify(responseCode as Number) as Number? {
        if (responseCode == 200) {
            return null;
        }
        if (responseCode == 401 || responseCode == 403) {
            return ERR_UNAUTHORIZED;
        }
        return ERR_UNREACHABLE;
    }
}
