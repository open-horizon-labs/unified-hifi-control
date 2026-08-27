import Foundation
import Testing
@testable import UHCKit

/// Bonjour hands back resolved addresses as packed `sockaddr` blobs. Reading
/// them is where the mDNS fix lives, so it is pinned here rather than trusted.
private func ipv4(_ a: UInt8, _ b: UInt8, _ c: UInt8, _ d: UInt8, port: UInt16) -> Data {
    var sa = sockaddr_in()
    sa.sin_len = UInt8(MemoryLayout<sockaddr_in>.size)
    sa.sin_family = sa_family_t(AF_INET)
    sa.sin_port = port.bigEndian
    sa.sin_addr.s_addr = UInt32(d) << 24 | UInt32(c) << 16 | UInt32(b) << 8 | UInt32(a)
    return withUnsafeBytes(of: &sa) { Data($0) }
}

private func ipv6(_ text: String, port: UInt16) -> Data {
    var sa = sockaddr_in6()
    sa.sin6_len = UInt8(MemoryLayout<sockaddr_in6>.size)
    sa.sin6_family = sa_family_t(AF_INET6)
    sa.sin6_port = port.bigEndian
    _ = text.withCString { inet_pton(AF_INET6, $0, &sa.sin6_addr) }
    return withUnsafeBytes(of: &sa) { Data($0) }
}

@Test func readsAnIPv4Address() {
    #expect(UHCDiscovery.numericHost(from: [ipv4(192, 168, 1, 2, port: 8088)]) == "192.168.1.2")
}

@Test func prefersIPv4OverIPv6() {
    let addresses = [ipv6("2001:db8::1", port: 8088), ipv4(192, 168, 1, 2, port: 8088)]
    #expect(UHCDiscovery.numericHost(from: addresses) == "192.168.1.2")
}

@Test func bracketsIPv6WhenThatIsAllThereIs() {
    #expect(UHCDiscovery.numericHost(from: [ipv6("2001:db8::1", port: 8088)]) == "[2001:db8::1]")
}

/// A link-local address needs a scope id that cannot survive a URL string, so
/// taking one would produce a base URL that never connects.
@Test func skipsLinkLocalIPv6() {
    #expect(UHCDiscovery.numericHost(from: [ipv6("fe80::1", port: 8088)]) == nil)
}

@Test func toleratesGarbage() {
    #expect(UHCDiscovery.numericHost(from: [Data([0x01, 0x02])]) == nil)
    #expect(UHCDiscovery.numericHost(from: []) == nil)
}
