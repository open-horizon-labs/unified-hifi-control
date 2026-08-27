import Toybox.Application;
import Toybox.Lang;
import Toybox.WatchUi;

//! Zone picker.
//!
//! Garmin's guidance calls this a "selection menu": a header for context, one
//! row per choice, and the current state shown as subtext. That is exactly
//! the shape here — the zone name is the choice, and what it is doing is the
//! subtext, so you can tell a playing room from a silent one without opening
//! it.
//!
//! The guidance also suggests around seven items per list. A ten-zone house
//! exceeds that, but the alternative — grouping into submenus — would add a
//! level of hierarchy to every single interaction, which the same guidance
//! warns against more strongly. Server order is the user's own configured
//! zone order, which is a more meaningful sort than alphabetical.
class ZoneMenu extends WatchUi.Menu2 {
    public function initialize(zones as Array<Zone>) {
        Menu2.initialize({ :title => WatchUi.loadResource(Rez.Strings.ZonesTitle) as String });
        for (var i = 0; i < zones.size(); i += 1) {
            var zone = zones[i];
            addItem(
                new WatchUi.MenuItem(
                    zone.name,
                    subtitleFor(zone),
                    i,          // identifier: index into the zones array
                    {}
                )
            );
        }
    }

    //! Subtext is state, in words rather than jargon. "Playing" earns its
    //! place; "stopped" and "paused" both read as idle to a user glancing at
    //! a list, so only the meaningful distinction is drawn.
    private function subtitleFor(zone as Zone) as String {
        if (zone.isPlaying()) {
            return "Playing";
        }
        if (zone.state.equals("paused")) {
            return "Paused";
        }
        return "Idle";
    }
}

class ZoneMenuDelegate extends WatchUi.Menu2InputDelegate {
    private var _zones as Array<Zone>;

    public function initialize(zones as Array<Zone>) {
        Menu2InputDelegate.initialize();
        _zones = zones;
    }

    public function onSelect(item as WatchUi.MenuItem) as Void {
        var index = item.getId() as Number;
        if (index < 0 || index >= _zones.size()) {
            return;
        }
        var zone = _zones[index];
        // Remember the room so the next launch lands on it: the aim is one
        // press to pause, and re-picking the same zone every time is the
        // tax that would prevent it.
        Application.Storage.setValue("lastZoneId", zone.id);

        var menu = new ZoneActionMenu(zone);
        WatchUi.pushView(menu, new ZoneActionMenuDelegate(menu), WatchUi.SLIDE_LEFT);
    }
}
