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
    public var volumeMin as Float;
    public var volumeMax as Float;
    public var volumeStep as Float;
    //! "percentage" or "db". A dB zone runs NEGATIVE (e.g. -80.0 to 0.0),
    //! which is why nothing here assumes a floor of zero.
    public var volumeScale as String;

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
            // Real range, so a gauge cannot misrepresent the level: a Roon
            // zone may run 0-98 with a half-step rather than 0-100.
            volumeMin = asFloat(vc["min"], 0.0);
            volumeMax = asFloat(vc["max"], 100.0);
            // Zones disagree: 1.0 on some, 0.5 on others. Using the zone's
            // own step means one press moves exactly one detent.
            volumeStep = asFloat(vc["step"], 1.0);
            if (volumeStep <= 0.0) { volumeStep = 1.0; }
            volumeScale = asString(vc["scale"], "percentage");
        } else {
            hasVolume = false;
            volume = 0.0;
            volumeMin = 0.0;
            volumeMax = 100.0;
            volumeStep = 1.0;
            volumeScale = "percentage";
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
