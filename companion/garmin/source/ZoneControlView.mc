import Toybox.Graphics;
import Toybox.Lang;
import Toybox.System;
import Toybox.WatchUi;

//! Driving one zone.
//!
//! The controls are a native `WatchUi.ActionMenu`, not something drawn here.
//! That matters: the action menu IS the widget Garmin's own music player
//! uses — the vertical strip on the right with the highlighted item named on
//! the left. Because it is a system component it renders in the device's own
//! style, scrolls with UP/DOWN, activates with START, and dismisses with
//! BACK, all without a line of drawing code.
//!
//! An earlier cut hand-painted an imitation of that strip. It was chunkier
//! than the real thing, needed its own hit testing, and would have drifted
//! from the platform on every firmware update. Reaching for the system
//! widget is both less code and more faithful.
//!
//! This view therefore only shows state: which zone, what it is playing, and
//! whether the last command worked.
class ZoneControlView extends WatchUi.View {
    private var _zone as Zone;
    private var _error as String?;
    private var _pending as Boolean = false;

    public function initialize(zone as Zone) {
        View.initialize();
        _zone = zone;
        // The system's own "there are actions here" affordance, in whatever
        // form this product uses.
        if (View has :setActionMenuIndicator) {
            View.setActionMenuIndicator({ :enabled => true });
        }
    }

    public function getZone() as Zone {
        return _zone;
    }

    public function send(action as String) as Void {
        if (action.equals(UhcApi.ACTION_PLAY_PAUSE)) {
            _zone.state = _zone.isPlaying() ? "paused" : "playing";
        } else if (action.equals(UhcApi.ACTION_VOLUME_UP)) {
            _zone.volume += 1.0;
        } else if (action.equals(UhcApi.ACTION_VOLUME_DOWN)) {
            _zone.volume -= 1.0;
        }
        _error = null;
        _pending = true;
        WatchUi.requestUpdate();
        new ControlRequest(method(:onControlDone)).start(_zone.id, action);
    }

    public function onControlDone(error as Number?) as Void {
        _pending = false;
        if (error != null) {
            _error = (error == UhcApi.ERR_UNAUTHORIZED)
                ? WatchUi.loadResource(Rez.Strings.Unauthorized) as String
                : WatchUi.loadResource(Rez.Strings.Unreachable) as String;
        }
        WatchUi.requestUpdate();
    }

    public function onUpdate(dc as Graphics.Dc) as Void {
        dc.setColor(Graphics.COLOR_WHITE, Graphics.COLOR_BLACK);
        dc.clear();

        var w = dc.getWidth();
        var h = dc.getHeight();
        var inset = (w * 0.16).toNumber();

        // Zone name, wrapped: truncation was ambiguous between rooms sharing
        // a prefix, and a marquee would redraw for as long as you look.
        var name = new WatchUi.TextArea({
            :text => _zone.name,
            :color => Graphics.COLOR_WHITE,
            :font => [Graphics.FONT_MEDIUM, Graphics.FONT_SMALL, Graphics.FONT_TINY],
            :justification => Graphics.TEXT_JUSTIFY_CENTER | Graphics.TEXT_JUSTIFY_VCENTER,
            :locX => inset,
            :locY => (h * 0.22).toNumber(),
            :width => w - inset * 2,
            :height => (h * 0.26).toNumber()
        });
        name.draw(dc);

        var status = _error != null
            ? _error
            : (_zone.isPlaying() ? "Playing" : "Paused");
        dc.setColor(
            _error != null ? Graphics.COLOR_RED : Graphics.COLOR_LT_GRAY,
            Graphics.COLOR_TRANSPARENT
        );
        dc.drawText(
            w / 2, (h * 0.56).toNumber(),
            Graphics.FONT_SMALL,
            status,
            Graphics.TEXT_JUSTIFY_CENTER | Graphics.TEXT_JUSTIFY_VCENTER
        );

        if (_zone.hasVolume) {
            dc.setColor(Graphics.COLOR_WHITE, Graphics.COLOR_TRANSPARENT);
            dc.drawText(
                w / 2, (h * 0.70).toNumber(),
                Graphics.FONT_NUMBER_MILD,
                _zone.volume.format("%d"),
                Graphics.TEXT_JUSTIFY_CENTER | Graphics.TEXT_JUSTIFY_VCENTER
            );
        }
    }
}

//! Opens the system action menu and turns its selections into commands.
class ZoneControlDelegate extends WatchUi.BehaviorDelegate {
    private var _view as ZoneControlView;

    public function initialize(view as ZoneControlView) {
        BehaviorDelegate.initialize();
        _view = view;
    }

    //! Products differ in how the action menu is summoned — a dedicated
    //! button on some, a touch area on others. `Styles` says which, so this
    //! does not hard-code either.
    //! START opens the actions too. On a remote the whole point is to act,
    //! so the primary button should not be inert.
    public function onSelect() as Boolean {
        showActions();
        return true;
    }

    public function onActionMenu() as Boolean {
        showActions();
        return true;
    }

    private function showActions() as Void {
        var zone = _view.getZone();
        var menu = new WatchUi.ActionMenu({});

        menu.addItem(new WatchUi.ActionMenuItem(
            { :label => zone.isPlaying() ? "Pause" : "Play" },
            UhcApi.ACTION_PLAY_PAUSE
        ));
        menu.addItem(new WatchUi.ActionMenuItem(
            { :label => "Next" }, UhcApi.ACTION_NEXT
        ));
        menu.addItem(new WatchUi.ActionMenuItem(
            { :label => "Previous" }, UhcApi.ACTION_PREVIOUS
        ));

        // Only offered when the zone actually has a volume to move: a
        // fixed-output renderer should not show a control that cannot work.
        if (zone.hasVolume) {
            menu.addItem(new WatchUi.ActionMenuItem(
                { :label => "Volume up" }, UhcApi.ACTION_VOLUME_UP
            ));
            menu.addItem(new WatchUi.ActionMenuItem(
                { :label => "Volume down" }, UhcApi.ACTION_VOLUME_DOWN
            ));
        }

        WatchUi.showActionMenu(menu, new ZoneActionDelegate(_view));
    }
}

class ZoneActionDelegate extends WatchUi.ActionMenuDelegate {
    private var _view as ZoneControlView;

    public function initialize(view as ZoneControlView) {
        ActionMenuDelegate.initialize();
        _view = view;
    }

    public function onSelect(item as WatchUi.ActionMenuItem) as Void {
        var action = item.getId();
        if (action instanceof String) {
            _view.send(action);
        }
    }
}
