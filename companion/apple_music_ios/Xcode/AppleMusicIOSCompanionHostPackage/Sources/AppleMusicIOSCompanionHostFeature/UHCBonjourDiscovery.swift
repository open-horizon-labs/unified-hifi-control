import Foundation
import UHCKit

/// The implementation moved to `UHCKit.UHCDiscovery` (#619).
///
/// The Watch controller needs the same capability, and two copies of a Bonjour
/// browser is the duplication that issue exists to stop — the same failure mode
/// as #611, where four surfaces each built artwork URLs their own way and all
/// four broke together.
///
/// This alias keeps the call sites in `ContentView` unchanged. `UHCDiscovery`
/// preserves the iOS behaviour exactly: it is the same `NetService` code, moved
/// rather than rewritten, and it prefers the resolved service hostname over the
/// TXT `base` record just as this type always did. (watchOS gets a separate
/// `NWBrowser` path inside `UHCKit`, because `NetService` is unavailable there.)
typealias UHCBonjourDiscovery = UHCDiscovery
