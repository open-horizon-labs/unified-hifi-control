import Toybox.Lang;

//! One zone, reduced to what a watch can act on.
//!
//! The server sends more than this (source, dsp, library tabs, output ids).
//! Parsing only what we draw keeps the peak memory of a ten-zone response
//! close to the size of the strings we actually show.
class Zone {
    public var id as String;
    public var name as String;
    public var state as String;          // "playing" | "paused" | "stopped" | ...
    public var hasVolume as Boolean;
    public var volume as Float;

    public function initialize(raw as Dictionary) {
        id = asString(raw["zone_id"], "");
        name = asString(raw["zone_name"], "Zone");
        state = asString(raw["state"], "stopped");

        // `volume_control` is absent for fixed-volume renderers, which is a
        // real case (an OpenHome endpoint with no volume of its own). Those
        // zones must still be controllable for transport, just without the
        // volume affordance — hence a flag rather than a default of 0.
        var vc = raw["volume_control"];
        if (vc != null && vc instanceof Dictionary && vc["value"] != null) {
            hasVolume = true;
            volume = asFloat(vc["value"], 0.0);
        } else {
            hasVolume = false;
            volume = 0.0;
        }
    }

    public function isPlaying() as Boolean {
        return state.equals("playing");
    }

    //! Parse a `/zones` payload into Zone objects, skipping anything that
    //! lacks an id — an unusable row is worse than a missing one, because
    //! tapping it would silently do nothing.
    public static function parseAll(rawZones as Array) as Array<Zone> {
        var zones = [] as Array<Zone>;
        for (var i = 0; i < rawZones.size(); i += 1) {
            var raw = rawZones[i];
            if (raw instanceof Dictionary && raw["zone_id"] != null) {
                zones.add(new Zone(raw));
            }
        }
        return zones;
    }

    private static function asString(value, fallback as String) as String {
        if (value != null && value instanceof String) {
            return value;
        }
        return fallback;
    }

    private static function asFloat(value, fallback as Float) as Float {
        if (value instanceof Float) {
            return value;
        }
        if (value instanceof Number) {
            return value.toFloat();
        }
        return fallback;
    }
}
