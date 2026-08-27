import Toybox.Graphics;
import Toybox.Lang;
import Toybox.Timer;
import Toybox.WatchUi;

//! Volume as a gauge you push a button against.
//!
//! An arc around the face, the number in the middle: UP raises, DOWN lowers,
//! BACK returns. Holding a button repeats, so a big change is one long press
//! rather than a dozen taps — which is why this is a screen of its own
//! rather than two rows in a list.
//!
//! The arc is scaled to the zone's real range, not assumed 0-100: zones here
//! run 0-98 on a half-step as readily as 0-100 on a whole one, and a dB zone
//! runs NEGATIVE (say -80.0 to 0.0). Nothing below assumes a floor of zero.
//!
//! Presses are DEBOUNCED and sent as one absolute level. Firing a request per
//! press floods Connect IQ's request queue on a fast sweep -- which is what
//! produced "can't reach the server" after a few quick presses -- and it is
//! the wrong shape anyway: we know the level the user landed on, so we send
//! that, once, when they stop.
class VolumeView extends WatchUi.View {
    private var _zone as Zone;
    private var _min as Float;
    private var _max as Float;
    private var _error as String?;
    //! Set while a level is waiting to be sent. The screen already shows the
    //! new value; this is only about the wire.
    private var _pending as Boolean = false;
    private var _timer as Timer.Timer?;

    public function initialize(zone as Zone) {
        View.initialize();
        _zone = zone;
        // The zone list carries value/min/max; fall back to a percentage
        // scale only when the provider did not say.
        _min = zone.volumeMin;
        _max = zone.volumeMax;
        if (_max <= _min) {
            _min = 0.0;
            _max = 100.0;
        }
    }

    public function adjust(up as Boolean) as Void {
        // One press moves one of the zone's own detents, not an assumed 1.0.
        var step = _zone.volumeStep;
        _zone.volume += up ? step : -step;
        if (_zone.volume < _min) { _zone.volume = _min; }
        if (_zone.volume > _max) { _zone.volume = _max; }
        _error = null;
        _pending = true;
        WatchUi.requestUpdate();

        // Restart the quiet period on every press: the send happens once the
        // user stops moving.
        if (_timer != null) {
            (_timer as Timer.Timer).stop();
        } else {
            _timer = new Timer.Timer();
        }
        (_timer as Timer.Timer).start(method(:flush), 450, false);
    }

    //! Send the level the user settled on, as one absolute command.
    public function flush() as Void {
        if (!_pending) {
            return;
        }
        _pending = false;
        new ControlRequest(method(:onDone)).startWithValue(
            _zone.id, UhcApi.ACTION_VOLUME_ABS, _zone.volume
        );
    }

    //! Leaving the screen must not lose a level still waiting on the timer.
    public function onHide() as Void {
        if (_timer != null) {
            (_timer as Timer.Timer).stop();
        }
        flush();
    }

    public function onDone(error as Number?) as Void {
        if (error != null) {
            _error = WatchUi.loadResource(Rez.Strings.Unreachable) as String;
            WatchUi.requestUpdate();
        }
    }

    public function onUpdate(dc as Graphics.Dc) as Void {
        dc.setColor(Graphics.COLOR_WHITE, Graphics.COLOR_BLACK);
        dc.clear();

        var w = dc.getWidth();
        var h = dc.getHeight();
        var cx = w / 2;
        var cy = h / 2;
        var radius = (w / 2) - 14;

        // Track: the full range, dim.
        dc.setColor(Graphics.COLOR_DK_GRAY, Graphics.COLOR_TRANSPARENT);
        dc.setPenWidth(12);
        dc.drawArc(cx, cy, radius, Graphics.ARC_CLOCKWISE, 210, 330);

        // Fill: the part below the current level.
        var span = _max - _min;
        var frac = span > 0 ? ((_zone.volume - _min) / span) : 0.0;
        if (frac < 0.0) { frac = 0.0; }
        if (frac > 1.0) { frac = 1.0; }
        if (frac > 0.001) {
            // 210 deg clockwise to 330 deg is 240 degrees of travel.
            var end = 210 - (240 * frac);
            if (end < -30) { end = -30; }
            dc.setColor(Graphics.COLOR_WHITE, Graphics.COLOR_TRANSPARENT);
            dc.drawArc(cx, cy, radius, Graphics.ARC_CLOCKWISE, 210, end.toNumber());
        }
        dc.setPenWidth(1);

        dc.setColor(Graphics.COLOR_LT_GRAY, Graphics.COLOR_TRANSPARENT);
        dc.drawText(cx, (h * 0.30).toNumber(), Graphics.FONT_XTINY,
            _zone.name,
            Graphics.TEXT_JUSTIFY_CENTER | Graphics.TEXT_JUSTIFY_VCENTER);

        dc.setColor(Graphics.COLOR_WHITE, Graphics.COLOR_TRANSPARENT);
        // Half-step zones need a decimal; whole-step ones look wrong with
        // one. dB zones are labelled, because "-42" alone reads as an error.
        var isDb = _zone.volumeScale.equals("db");
        var shown = (_zone.volumeStep < 1.0)
            ? _zone.volume.format("%.1f")
            : _zone.volume.format("%d");
        dc.drawText(cx, cy, Graphics.FONT_NUMBER_THAI_HOT, shown,
            Graphics.TEXT_JUSTIFY_CENTER | Graphics.TEXT_JUSTIFY_VCENTER);
        if (isDb) {
            dc.setColor(Graphics.COLOR_LT_GRAY, Graphics.COLOR_TRANSPARENT);
            dc.drawText(cx, (h * 0.63).toNumber(), Graphics.FONT_XTINY, "dB",
                Graphics.TEXT_JUSTIFY_CENTER | Graphics.TEXT_JUSTIFY_VCENTER);
        }

        dc.setColor(
            _error != null ? Graphics.COLOR_RED : Graphics.COLOR_DK_GRAY,
            Graphics.COLOR_TRANSPARENT
        );
        dc.drawText(cx, (h * 0.76).toNumber(), Graphics.FONT_XTINY,
            _error != null ? _error : "Volume",
            Graphics.TEXT_JUSTIFY_CENTER | Graphics.TEXT_JUSTIFY_VCENTER);
    }
}

class VolumeDelegate extends WatchUi.BehaviorDelegate {
    private var _view as VolumeView;

    public function initialize(view as VolumeView) {
        BehaviorDelegate.initialize();
        _view = view;
    }

    public function onPreviousPage() as Boolean {
        _view.adjust(true);
        return true;
    }

    public function onNextPage() as Boolean {
        _view.adjust(false);
        return true;
    }

    //! START also leaves: on a gauge the natural "done" is the primary
    //! button, and BACK works too.
    public function onSelect() as Boolean {
        WatchUi.popView(WatchUi.SLIDE_DOWN);
        return true;
    }
}
