import Toybox.Graphics;
import Toybox.Lang;
import Toybox.WatchUi;

//! The first thing you see: a busy state that becomes the zone list, or a
//! message that tells you what to fix.
//!
//! Garmin's guidance is to limit hierarchy depth and favour quick selection,
//! so this view exists only long enough to fetch — it replaces itself with
//! the zone menu rather than adding a level to back out of.
class LoadingView extends WatchUi.View {
    private var _message as String?;
    private var _busy as Boolean = true;

    public function initialize() {
        View.initialize();
    }

    public function onShow() as Void {
        refresh();
    }

    public function refresh() as Void {
        _busy = true;
        _message = null;
        WatchUi.requestUpdate();
        new ZonesRequest(method(:onZones)).start();
    }

    public function onZones(error as Number?, rawZones as Array?) as Void {
        _busy = false;
        if (error != null) {
            _message = messageFor(error);
            WatchUi.requestUpdate();
            return;
        }

        var zones = Zone.parseAll(rawZones);
        if (zones.size() == 0) {
            _message = WatchUi.loadResource(Rez.Strings.NoZones) as String;
            WatchUi.requestUpdate();
            return;
        }

        // switchToView, not pushView: this view has done its job and should
        // not sit in the back stack collecting a pointless BACK press.
        var menu = new ZoneMenu(zones);
        WatchUi.switchToView(menu, new ZoneMenuDelegate(zones), WatchUi.SLIDE_IMMEDIATE);
    }

    //! Each failure earns different advice, because each has a different fix.
    private function messageFor(error as Number) as String {
        if (error == UhcApi.ERR_NO_SERVER) {
            return WatchUi.loadResource(Rez.Strings.NoServer) as String;
        }
        if (error == UhcApi.ERR_UNAUTHORIZED) {
            return WatchUi.loadResource(Rez.Strings.Unauthorized) as String;
        }
        return WatchUi.loadResource(Rez.Strings.Unreachable) as String;
    }

    public function onUpdate(dc as Graphics.Dc) as Void {
        dc.setColor(Graphics.COLOR_WHITE, Graphics.COLOR_BLACK);
        dc.clear();

        var text = _busy
            ? WatchUi.loadResource(Rez.Strings.Loading) as String
            : _message;
        if (text == null) {
            return;
        }

        // A TextArea, not drawText: drawText does not wrap, and on a round
        // 454px face a sentence like "Set the server address in the app
        // settings" ran straight off both edges — caught in the simulator,
        // invisible to the compiler. TextArea wraps and will step down
        // through the supplied fonts to fit.
        var inset = (dc.getWidth() * 0.16).toNumber();
        var area = new WatchUi.TextArea({
            :text => text,
            :color => Graphics.COLOR_WHITE,
            :font => [Graphics.FONT_MEDIUM, Graphics.FONT_SMALL, Graphics.FONT_TINY],
            :justification => Graphics.TEXT_JUSTIFY_CENTER | Graphics.TEXT_JUSTIFY_VCENTER,
            :locX => inset,
            :locY => (dc.getHeight() * 0.3).toNumber(),
            :width => dc.getWidth() - inset * 2,
            :height => (dc.getHeight() * 0.4).toNumber()
        });
        area.draw(dc);

        // Build marker, small and always present: the only way to know from
        // the wrist which version is actually installed.
        dc.setColor(Graphics.COLOR_DK_GRAY, Graphics.COLOR_TRANSPARENT);
        dc.drawText(
            dc.getWidth() / 2,
            (dc.getHeight() * 0.80).toNumber(),
            Graphics.FONT_XTINY,
            "v" + UhcApi.VERSION,
            Graphics.TEXT_JUSTIFY_CENTER | Graphics.TEXT_JUSTIFY_VCENTER
        );
    }
}

//! Tap or SELECT retries; BACK leaves the app. A stuck error screen with no
//! way to retry would force a relaunch just to re-attempt a request.
class LoadingDelegate extends WatchUi.BehaviorDelegate {
    private var _view as LoadingView;

    public function initialize(view as LoadingView) {
        BehaviorDelegate.initialize();
        _view = view;
    }

    public function onSelect() as Boolean {
        _view.refresh();
        return true;
    }
}
