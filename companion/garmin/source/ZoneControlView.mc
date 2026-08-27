import Toybox.Graphics;
import Toybox.Lang;
import Toybox.WatchUi;

//! Driving one zone.
//!
//! Modelled on Garmin's own music playback screen: previous / play-pause /
//! next as on-screen controls you select, laid out as a row across the
//! middle of a round display.
//!
//! ONE DELIBERATE DEVIATION from Garmin's model, and it is worth stating
//! plainly. Garmin puts volume behind its own screen, reached by selecting a
//! volume icon. Here UP and DOWN adjust volume directly from this screen.
//! The reason is the whole point of the app: the owner wants volume without
//! looking at his wrist. A mode switch costs a look and two presses for the
//! single most-repeated action, and nothing in Garmin's guidance requires
//! UP/DOWN to mean anything else on a custom view. Everything mandatory is
//! preserved: BACK backs out, SELECT is the primary action.
//!
//! State is optimistic. A press paints its result immediately and reconciles
//! from the server afterwards, because a Bluetooth round trip is slow enough
//! that waiting for truth before drawing would feel broken.
class ZoneControlView extends WatchUi.View {
    private var _zone as Zone;
    private var _busy as Boolean = false;
    private var _error as String?;

    // Hit targets, resolved in onLayout against the real screen so this does
    // not hard-code 454x454.
    private var _cx as Number = 0;
    private var _cy as Number = 0;
    private var _buttonY as Number = 0;
    private var _buttonDx as Number = 0;
    private var _buttonR as Number = 0;

    public function initialize(zone as Zone) {
        View.initialize();
        _zone = zone;
    }

    public function onLayout(dc as Graphics.Dc) as Void {
        var w = dc.getWidth();
        var h = dc.getHeight();
        _cx = w / 2;
        _cy = h / 2;
        _buttonY = (h * 0.56).toNumber();
        _buttonDx = (w * 0.27).toNumber();
        // Comfortably above the ~44px minimum for a fingertip on a 454px face.
        _buttonR = (w * 0.11).toNumber();
    }

    public function getZone() as Zone {
        return _zone;
    }

    //! Fire a control action, painting the expected outcome first.
    public function send(action as String) as Void {
        if (action.equals(UhcApi.ACTION_PLAY_PAUSE)) {
            _zone.state = _zone.isPlaying() ? "paused" : "playing";
        }
        _busy = true;
        _error = null;
        WatchUi.requestUpdate();
        new ControlRequest(method(:onControlDone)).start(_zone.id, action);
    }

    public function onControlDone(error as Number?) as Void {
        _busy = false;
        if (error != null) {
            // The optimistic paint was a guess and the guess was wrong. Say
            // so rather than leaving a false "playing" on screen.
            _error = (error == UhcApi.ERR_UNAUTHORIZED)
                ? WatchUi.loadResource(Rez.Strings.Unauthorized) as String
                : WatchUi.loadResource(Rez.Strings.Unreachable) as String;
        }
        WatchUi.requestUpdate();
    }

    public function onUpdate(dc as Graphics.Dc) as Void {
        dc.setColor(Graphics.COLOR_WHITE, Graphics.COLOR_BLACK);
        dc.clear();

        // Zone name, wrapped rather than truncated or scrolled.
        //
        // Truncation was ambiguous ("Bathroom …" could be two different
        // rooms). A marquee is what Garmin does for TRACK titles — long,
        // changing text — but this is a static label on a screen you are
        // holding open, and a marquee means a repeating timer redrawing the
        // screen for as long as you look at it. Two lines costs nothing and
        // is readable at a glance, which is the whole job.
        var nameInset = (dc.getWidth() * 0.17).toNumber();
        var name = new WatchUi.TextArea({
            :text => _zone.name,
            :color => Graphics.COLOR_WHITE,
            :font => [Graphics.FONT_MEDIUM, Graphics.FONT_SMALL, Graphics.FONT_TINY],
            :justification => Graphics.TEXT_JUSTIFY_CENTER | Graphics.TEXT_JUSTIFY_VCENTER,
            :locX => nameInset,
            :locY => (_cy * 0.18).toNumber(),
            :width => dc.getWidth() - nameInset * 2,
            :height => (_cy * 0.62).toNumber()
        });
        name.draw(dc);

        // Status line: an error outranks state, because it is the thing the
        // user has to act on.
        var status = _error != null
            ? _error
            : (_zone.isPlaying() ? "Playing" : "Paused");
        dc.setColor(
            _error != null ? Graphics.COLOR_RED : Graphics.COLOR_LT_GRAY,
            Graphics.COLOR_TRANSPARENT
        );
        // Below the name block (which reaches 0.80 * _cy) and above the
        // transport row — a two-line zone name used to collide with this.
        dc.drawText(
            _cx, (_cy * 0.90).toNumber(),
            Graphics.FONT_TINY,
            status,
            Graphics.TEXT_JUSTIFY_CENTER | Graphics.TEXT_JUSTIFY_VCENTER
        );

        drawTransport(dc);
        drawVolume(dc);
    }

    //! Previous / play-pause / next, in Garmin's order.
    private function drawTransport(dc as Graphics.Dc) as Void {
        dc.setColor(Graphics.COLOR_WHITE, Graphics.COLOR_TRANSPARENT);
        var r = _buttonR;

        // Previous: two back-pointing triangles.
        drawTriangle(dc, _cx - _buttonDx, _buttonY, r * 0.5, false);
        // Next: two forward-pointing triangles.
        drawTriangle(dc, _cx + _buttonDx, _buttonY, r * 0.5, true);

        // Play/pause, drawn larger because it is the primary action.
        if (_zone.isPlaying()) {
            var barW = (r * 0.22).toNumber();
            var barH = (r * 0.9).toNumber();
            dc.fillRectangle(_cx - barW * 2, _buttonY - barH / 2, barW, barH);
            dc.fillRectangle(_cx + barW, _buttonY - barH / 2, barW, barH);
        } else {
            drawTriangle(dc, _cx, _buttonY, r * 0.72, true);
        }
    }

    private function drawTriangle(
        dc as Graphics.Dc,
        cx as Number,
        cy as Number,
        size as Float,
        forward as Boolean
    ) as Void {
        var s = size.toNumber();
        var tip = forward ? cx + s : cx - s;
        var back = forward ? cx - s : cx + s;
        // fillPolygon wants fixed-length [x, y] pairs, not a loose Array of
        // Arrays — the type checker is strict about the difference.
        var points = [
            [back, cy - s],
            [tip, cy],
            [back, cy + s]
        ] as Array<[Numeric, Numeric]>;
        dc.fillPolygon(points);
    }

    //! Volume, shown only when the zone has any. A fixed-output renderer must
    //! not display a volume it cannot change.
    private function drawVolume(dc as Graphics.Dc) as Void {
        if (!_zone.hasVolume) {
            return;
        }
        dc.setColor(Graphics.COLOR_LT_GRAY, Graphics.COLOR_TRANSPARENT);
        dc.drawText(
            _cx, (_cy * 1.56).toNumber(),
            Graphics.FONT_TINY,
            "−   " + _zone.volume.format("%d") + "   +",
            Graphics.TEXT_JUSTIFY_CENTER | Graphics.TEXT_JUSTIFY_VCENTER
        );
    }

    //! Which on-screen control, if any, a tap landed on.
    public function hitTest(x as Number, y as Number) as String? {
        if ((y - _buttonY).abs() > _buttonR * 1.6) {
            return null;
        }
        if ((x - _cx).abs() <= _buttonR * 1.4) {
            return UhcApi.ACTION_PLAY_PAUSE;
        }
        if ((x - (_cx - _buttonDx)).abs() <= _buttonR * 1.4) {
            return UhcApi.ACTION_PREVIOUS;
        }
        if ((x - (_cx + _buttonDx)).abs() <= _buttonR * 1.4) {
            return UhcApi.ACTION_NEXT;
        }
        return null;
    }
}

class ZoneControlDelegate extends WatchUi.BehaviorDelegate {
    private var _view as ZoneControlView;

    public function initialize(view as ZoneControlView) {
        BehaviorDelegate.initialize();
        _view = view;
    }

    //! SELECT is the primary action, and for a music remote that is
    //! play/pause — matching Garmin's own screen, where play is the centre.
    public function onSelect() as Boolean {
        _view.send(UhcApi.ACTION_PLAY_PAUSE);
        return true;
    }

    //! UP/DOWN as volume: the deliberate deviation documented on the view.
    public function onNextPage() as Boolean {
        return adjustVolume(UhcApi.ACTION_VOLUME_DOWN);
    }

    public function onPreviousPage() as Boolean {
        return adjustVolume(UhcApi.ACTION_VOLUME_UP);
    }

    private function adjustVolume(action as String) as Boolean {
        if (!_view.getZone().hasVolume) {
            return true;   // swallow it: paging away from a control screen
        }                  // would be worse than doing nothing
        _view.send(action);
        return true;
    }

    public function onTap(event as WatchUi.ClickEvent) as Boolean {
        var coords = event.getCoordinates();
        var action = _view.hitTest(coords[0], coords[1]);
        if (action == null) {
            return false;
        }
        _view.send(action);
        return true;
    }
}
