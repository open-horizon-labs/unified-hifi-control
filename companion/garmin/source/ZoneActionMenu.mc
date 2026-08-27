import Toybox.Application;
import Toybox.Lang;
import Toybox.WatchUi;

//! Driving one zone — built entirely from system widgets.
//!
//! This is a `Menu2`, which means Garmin draws it: the device's own type,
//! spacing, highlight, scrolling and back behaviour. UP and DOWN move the
//! selection, START activates it, BACK returns. Nothing here requires the
//! touchscreen, and nothing here is hand-painted.
//!
//! Three earlier attempts drew a control screen by hand, chasing the look of
//! Garmin's music player. That screen belongs to the native media player and
//! is reachable only by being an audio-content-provider — an app that feeds
//! songs to the WATCH to play. A remote for a hi-fi in another room cannot be
//! one, so the honest move is to stop imitating a screen we cannot have and
//! use the widgets Garmin does give us. Those widgets are the platform look.
//!
//! Layout follows the SDK's menu guidance: a title for context, well under
//! seven items, and the current value carried as subtext on its own row
//! ("Show the current selection as subtext in the parent menu list item").
//!
//! Volume is two ordinary rows rather than a picker or a mode. Sitting on
//! "Volume up" and pressing START repeatedly is the eyes-free gesture the
//! whole app exists for: the selection does not move, the value updates in
//! place, and no screen has to be read.
class ZoneActionMenu extends WatchUi.Menu2 {
    private var _zone as Zone;

    // Row identifiers double as the wire actions, so a menu row cannot drift
    // from the command it claims to send.
    private var _playItem as WatchUi.MenuItem?;
    private var _volUpItem as WatchUi.MenuItem?;
    private var _volDownItem as WatchUi.MenuItem?;

    public function initialize(zone as Zone) {
        Menu2.initialize({ :title => zone.name });
        _zone = zone;

        _playItem = new WatchUi.MenuItem(
            playLabel(), statusText(), UhcApi.ACTION_PLAY_PAUSE, {}
        );
        addItem(_playItem);

        addItem(new WatchUi.MenuItem("Next", null, UhcApi.ACTION_NEXT, {}));
        addItem(new WatchUi.MenuItem("Previous", null, UhcApi.ACTION_PREVIOUS, {}));

        // Absent, not disabled, when the zone has no volume of its own: a
        // fixed-output renderer should not offer a control that cannot work.
        if (_zone.hasVolume) {
            _volUpItem = new WatchUi.MenuItem(
                "Volume up", volumeText(), UhcApi.ACTION_VOLUME_UP, {}
            );
            _volDownItem = new WatchUi.MenuItem(
                "Volume down", volumeText(), UhcApi.ACTION_VOLUME_DOWN, {}
            );
            addItem(_volUpItem);
            addItem(_volDownItem);
        }
    }

    public function getZone() as Zone {
        return _zone;
    }

    private function playLabel() as String {
        return _zone.isPlaying() ? "Pause" : "Play";
    }

    private function statusText() as String {
        return _zone.isPlaying() ? "Playing" : "Paused";
    }

    private function volumeText() as String {
        return _zone.volume.format("%d");
    }

    //! Repaint the rows whose text depends on state. Called after every
    //! command so the menu is the feedback — there is no separate status
    //! screen to look at, which is the point.
    public function refreshRows() as Void {
        if (_playItem != null) {
            _playItem.setLabel(playLabel());
            _playItem.setSubLabel(statusText());
        }
        if (_volUpItem != null) {
            _volUpItem.setSubLabel(volumeText());
        }
        if (_volDownItem != null) {
            _volDownItem.setSubLabel(volumeText());
        }
        WatchUi.requestUpdate();
    }

    //! Report a failure on the row itself rather than in a separate view —
    //! the user is looking here, and the alternative is a silent no-op.
    public function showError(message as String) as Void {
        if (_playItem != null) {
            _playItem.setSubLabel(message);
        }
        WatchUi.requestUpdate();
    }
}

class ZoneActionMenuDelegate extends WatchUi.Menu2InputDelegate {
    private var _menu as ZoneActionMenu;

    public function initialize(menu as ZoneActionMenu) {
        Menu2InputDelegate.initialize();
        _menu = menu;
    }

    public function onSelect(item as WatchUi.MenuItem) as Void {
        var action = item.getId();
        if (!(action instanceof String)) {
            return;
        }
        var zone = _menu.getZone();

        // Optimistic: paint the expected result now and reconcile on reply.
        // A Bluetooth round trip is slow enough that waiting first reads as
        // a dead button.
        if (action.equals(UhcApi.ACTION_PLAY_PAUSE)) {
            zone.state = zone.isPlaying() ? "paused" : "playing";
        } else if (action.equals(UhcApi.ACTION_VOLUME_UP)) {
            zone.volume += 1.0;
        } else if (action.equals(UhcApi.ACTION_VOLUME_DOWN)) {
            zone.volume -= 1.0;
        }
        _menu.refreshRows();

        new ControlRequest(method(:onControlReply)).start(zone.id, action);
    }

    //! Named to avoid colliding with Menu2InputDelegate's own `onDone`,
    //! which has a different signature — the compiler caught the clash.
    public function onControlReply(error as Number?) as Void {
        if (error == null) {
            return;
        }
        _menu.showError(
            error == UhcApi.ERR_UNAUTHORIZED
                ? WatchUi.loadResource(Rez.Strings.Unauthorized) as String
                : WatchUi.loadResource(Rez.Strings.Unreachable) as String
        );
    }
}
