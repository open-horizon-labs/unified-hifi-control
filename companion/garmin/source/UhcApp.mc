import Toybox.Application;
import Toybox.Lang;
import Toybox.WatchUi;

//! Entry point. Deliberately thin: the app owns no state of its own, because
//! the zone list is authoritative on the server and the watch is a remote,
//! not a cache.
class UhcApp extends Application.AppBase {

    public function initialize() {
        AppBase.initialize();
    }

    public function getInitialView() as [WatchUi.Views] or [WatchUi.Views, WatchUi.InputDelegates] {
        var view = new LoadingView();
        return [view, new LoadingDelegate(view)];
    }

    //! Settings changed in Garmin Connect (or on-device). The base URL may
    //! have just been fixed, so re-fetch rather than leaving the user staring
    //! at the error that prompted them to edit it.
    public function onSettingsChanged() as Void {
        WatchUi.requestUpdate();
    }
}
