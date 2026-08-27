import Toybox.Graphics;
import Toybox.Lang;
import Toybox.Math;
import Toybox.System;
import Toybox.WatchUi;

//! One zone, driven directly by the physical buttons.
//!
//!   START (right top)   play / pause
//!   DOWN  (left bottom) next track
//!   UP    (left middle) volume
//!   BACK  (right bottom) back to the zone list
//!
//! No list to scroll, no mode to enter for the two things done most often.
//! An earlier version put these in a `Menu2`, which was properly native but
//! made volume four presses away and pause one scroll away — wrong for a
//! remote you use without looking.
//!
//! Garmin's own button-hint markers sit beside the three active buttons, via
//! the personality selectors that carry icon, position and size per product.
//! That is how a button-driven screen says what its buttons do.
class NowPlayingView extends WatchUi.View {
    private var _zone as Zone;
    private var _title as String = "";
    private var _artist as String = "";
    private var _error as String?;
    private var _loaded as Boolean = false;
    private var _art as WatchUi.BitmapResource?;
    private var _artKey as String = "";
    //! Screen width, learned at layout time. Art is requested as a fraction
    //! of it so the same code suits a 454px AMOLED and a 260px MIP.
    private var _screenW as Number = 260;

    public function initialize(zone as Zone) {
        View.initialize();
        _zone = zone;
    }

    public function onLayout(dc as Graphics.Dc) as Void {
        _screenW = dc.getWidth();
        setLayout(Rez.Layouts.NowPlayingHints(dc));
    }

    public function onShow() as Void {
        refresh();
    }

    public function getZone() as Zone {
        return _zone;
    }

    //! Pull what is playing. Called on open and after a track change, not on
    //! a timer: a periodic poll over Bluetooth costs battery for information
    //! that only changes when the user acts or the track ends.
    public function refresh() as Void {
        new NowPlayingRequest(method(:onNowPlaying)).start(_zone.id);
    }

    public function onNowPlaying(error as Number?, data as Dictionary?) as Void {
        if (error != null || data == null) {
            // Track text is a nicety; failing to get it must not make the
            // transport controls look broken.
            _loaded = true;
            WatchUi.requestUpdate();
            return;
        }
        _title = text(data["line1"]);
        _artist = text(data["line2"]);

        // Only re-request art when the image actually changed: the request
        // is a round trip through Garmin's servers, so refetching the same
        // album on every command would be slow and pointless.
        var key = text(data["image_key"]);
        if (key.length() > 0 && !key.equals(_artKey)) {
            _artKey = key;
            new ArtRequest(method(:onArt)).start(_zone.id, key, (_screenW * 0.39).toNumber());
        } else if (key.length() == 0) {
            _artKey = "";
            _art = null;
        }
        var playing = data["is_playing"];
        if (playing instanceof Boolean) {
            _zone.state = playing ? "playing" : "paused";
        }
        var vol = data["volume"];
        if (vol instanceof Float) {
            _zone.volume = vol;
        } else if (vol instanceof Number) {
            _zone.volume = vol.toFloat();
        }
        _loaded = true;
        WatchUi.requestUpdate();
    }

    public function onArt(error as Number?, bitmap) as Void {
        // Art is decoration. A failure leaves the previous image or none at
        // all, and never surfaces an error — the controls still work.
        if (error == null && bitmap != null) {
            _art = bitmap;
            WatchUi.requestUpdate();
        }
    }

    private function text(value) as String {
        return (value != null && value instanceof String) ? value : "";
    }

    public function send(action as String) as Void {
        if (action.equals(UhcApi.ACTION_PLAY_PAUSE)) {
            _zone.state = _zone.isPlaying() ? "paused" : "playing";
        }
        _error = null;
        WatchUi.requestUpdate();
        new ControlRequest(method(:onControlDone)).start(_zone.id, action);
    }

    public function onControlDone(error as Number?) as Void {
        if (error != null) {
            _error = (error == UhcApi.ERR_UNAUTHORIZED)
                ? WatchUi.loadResource(Rez.Strings.Unauthorized) as String
                : WatchUi.loadResource(Rez.Strings.Unreachable) as String;
            WatchUi.requestUpdate();
            return;
        }
        // The server is the truth about what is playing now; a skip changes
        // the track, so re-read rather than guess.
        refresh();
    }

    public function onUpdate(dc as Graphics.Dc) as Void {
        dc.setColor(Graphics.COLOR_WHITE, Graphics.COLOR_BLACK);
        dc.clear();
        View.onUpdate(dc);          // draws the button hints

        var w = dc.getWidth();
        var h = dc.getHeight();
        // The glyphs sit out at the bezel, so the text can breathe wider.
        var inset = (w * 0.205).toNumber();

        // Zone: context, small, at the top.
        dc.setColor(Graphics.COLOR_LT_GRAY, Graphics.COLOR_TRANSPARENT);
        dc.drawText(
            w / 2, (h * 0.20).toNumber(), Graphics.FONT_XTINY,
            _zone.name,
            Graphics.TEXT_JUSTIFY_CENTER | Graphics.TEXT_JUSTIFY_VCENTER
        );

        // Album art, large. On a 454px AMOLED the art is the best thing on
        // the screen, so it gets the middle of it; the text sits underneath.
        // Art gets the top half; the text keeps a fixed band beneath it, so
        // a larger cover can never squeeze the title out of existence — an
        // earlier version computed the text height as a subtraction and
        // silently produced a negative box.
        var textTop = 0.30;
        var textBottom = 0.82;
        if (_art != null) {
            var art = _art as WatchUi.BitmapResource;
            var side = art.getWidth();
            var artTop = (h * 0.125).toNumber();
            dc.drawBitmap((w / 2) - (side / 2), artTop, art);
            textTop = ((artTop + side).toFloat() / h) + 0.015;
        }
        if (textTop > textBottom - 0.10) {
            textTop = textBottom - 0.10;   // always room for one line
        }

        // Track and artist: the headline, wrapped inside the bezel.
        var body = new WatchUi.TextArea({
            :text => _loaded ? bodyText() : (WatchUi.loadResource(Rez.Strings.Loading) as String),
            :color => Graphics.COLOR_WHITE,
            :font => [Graphics.FONT_MEDIUM, Graphics.FONT_SMALL, Graphics.FONT_TINY, Graphics.FONT_XTINY],
            :justification => Graphics.TEXT_JUSTIFY_CENTER | Graphics.TEXT_JUSTIFY_VCENTER,
            :locX => inset,
            :locY => (h * textTop).toNumber(),
            :width => w - inset * 2,
            :height => (h * (textBottom - textTop)).toNumber()
        });
        body.draw(dc);

        // Action glyphs beside their buttons, matching Garmin's own player:
        // what each button does, not a caption in the middle of the screen.
        // Angles are measured from 3 o'clock, counter-clockwise, and chosen
        // to sit just inside the system hint marks.
        drawButtonGlyphs(dc);

        if (_error != null) {
            dc.setColor(Graphics.COLOR_RED, Graphics.COLOR_TRANSPARENT);
            dc.drawText(
                w / 2, (h * 0.85).toNumber(), Graphics.FONT_XTINY,
                _error,
                Graphics.TEXT_JUSTIFY_CENTER | Graphics.TEXT_JUSTIFY_VCENTER
            );
        }
    }

    private function bodyText() as String {
        if (_title.length() == 0) {
            return _zone.isPlaying() ? "Playing" : "Nothing playing";
        }
        if (_artist.length() == 0) {
            return _title;
        }
        return _title + "\n" + _artist;
    }

    //! Glyphs at the three live buttons. The play/pause glyph shows the
    //! ACTION the button performs, so it flips to two bars while playing —
    //! the same convention as Garmin's player and every transport control.
    private function drawButtonGlyphs(dc as Graphics.Dc) as Void {
        var w = dc.getWidth();
        var h = dc.getHeight();
        var r = (w * 0.443).toNumber();
        var s = (w * 0.034).toNumber();

        dc.setColor(Graphics.COLOR_WHITE, Graphics.COLOR_TRANSPARENT);
        var play = polar(w, h, r, 33.0);
        if (_zone.isPlaying()) {
            drawPause(dc, play[0], play[1], s);
        } else {
            drawPlay(dc, play[0], play[1], s);
        }

        if (_zone.hasVolume) {
            var vol = polar(w, h, r, 177.0);
            drawSpeaker(dc, vol[0], vol[1], s);
        }

        var next = polar(w, h, r, 208.0);
        drawSkip(dc, next[0], next[1], s);
    }

    //! Point on a circle around the screen centre. Degrees run
    //! counter-clockwise from 3 o'clock, matching Graphics' arc convention.
    private function polar(
        w as Number, h as Number, r as Number, degrees as Float
    ) as Array<Number> {
        var rad = degrees * Math.PI / 180.0;
        return [
            (w / 2 + r * Math.cos(rad)).toNumber(),
            (h / 2 - r * Math.sin(rad)).toNumber()
        ] as Array<Number>;
    }

    private function drawPlay(dc as Graphics.Dc, cx as Number, cy as Number, s as Number) as Void {
        var pts = [
            [cx - s, cy - s],
            [cx + s, cy],
            [cx - s, cy + s]
        ] as Array<[Numeric, Numeric]>;
        dc.fillPolygon(pts);
    }

    private function drawPause(dc as Graphics.Dc, cx as Number, cy as Number, s as Number) as Void {
        var bw = (s * 0.42).toNumber();
        dc.fillRectangle(cx - bw - bw / 2, cy - s, bw, s * 2);
        dc.fillRectangle(cx + bw / 2, cy - s, bw, s * 2);
    }

    private function drawSkip(dc as Graphics.Dc, cx as Number, cy as Number, s as Number) as Void {
        var pts = [
            [cx - s, cy - s],
            [cx + (s * 0.4).toNumber(), cy],
            [cx - s, cy + s]
        ] as Array<[Numeric, Numeric]>;
        dc.fillPolygon(pts);
        dc.fillRectangle(cx + (s * 0.6).toNumber(), cy - s, 3, s * 2);
    }

    //! Speaker: a box, a flared cone, and one sound arc.
    private function drawSpeaker(dc as Graphics.Dc, cx as Number, cy as Number, s as Number) as Void {
        var half = (s * 0.42).toNumber();
        dc.fillRectangle(cx - s, cy - half, (s * 0.5).toNumber(), half * 2);
        var pts = [
            [cx - (s * 0.5).toNumber(), cy - half],
            [cx + (s * 0.15).toNumber(), cy - s],
            [cx + (s * 0.15).toNumber(), cy + s],
            [cx - (s * 0.5).toNumber(), cy + half]
        ] as Array<[Numeric, Numeric]>;
        dc.fillPolygon(pts);
        dc.setPenWidth(2);
        dc.drawArc(cx + (s * 0.2).toNumber(), cy, (s * 0.7).toNumber(),
            Graphics.ARC_COUNTER_CLOCKWISE, 305, 55);
        dc.setPenWidth(1);
    }
}

class NowPlayingDelegate extends WatchUi.BehaviorDelegate {
    private var _view as NowPlayingView;

    public function initialize(view as NowPlayingView) {
        BehaviorDelegate.initialize();
        _view = view;
    }

    //! START — play/pause.
    public function onSelect() as Boolean {
        _view.send(UhcApi.ACTION_PLAY_PAUSE);
        return true;
    }

    //! DOWN — next track.
    public function onNextPage() as Boolean {
        _view.send(UhcApi.ACTION_NEXT);
        return true;
    }

    //! UP — volume. A separate screen, as on Garmin's own player, because a
    //! gauge you hold a button against is the fastest way to move a level.
    public function onPreviousPage() as Boolean {
        var zone = _view.getZone();
        if (!zone.hasVolume) {
            return true;
        }
        var view = new VolumeView(zone);
        WatchUi.pushView(view, new VolumeDelegate(view), WatchUi.SLIDE_UP);
        return true;
    }
}
