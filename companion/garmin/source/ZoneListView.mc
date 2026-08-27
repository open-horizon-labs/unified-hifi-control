import Toybox.Graphics;
import Toybox.Lang;
import Toybox.WatchUi;

//! Placeholder while the interaction model is being confirmed against
//! Garmin's own guidelines. Replaced in the UI pass.
class ZoneListView extends WatchUi.View {
    public function initialize() {
        View.initialize();
    }

    public function onUpdate(dc as Graphics.Dc) as Void {
        dc.setColor(Graphics.COLOR_WHITE, Graphics.COLOR_BLACK);
        dc.clear();
        dc.drawText(
            dc.getWidth() / 2,
            dc.getHeight() / 2,
            Graphics.FONT_MEDIUM,
            WatchUi.loadResource(Rez.Strings.Loading) as String,
            Graphics.TEXT_JUSTIFY_CENTER | Graphics.TEXT_JUSTIFY_VCENTER
        );
    }
}

class ZoneListDelegate extends WatchUi.BehaviorDelegate {
    private var _view as ZoneListView;

    public function initialize(view as ZoneListView) {
        BehaviorDelegate.initialize();
        _view = view;
    }
}
