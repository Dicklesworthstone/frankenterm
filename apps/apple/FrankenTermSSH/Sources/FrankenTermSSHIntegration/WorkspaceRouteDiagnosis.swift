import Foundation
import FrankenTermKit
import FrankenTermSSH
#if canImport(Darwin)
import Darwin
#else
import Glibc
#endif

/// Where a route's address lives, which decides what this device needs before
/// the address can answer at all.
public enum WorkspaceRouteNetwork: Sendable, Equatable {
    /// Tailscale: 100.64.0.0/10, fd7a:115c:a1e0::/48 or a MagicDNS *.ts.net name.
    case tailnet
    /// A private or link-local address, or a .local name: same network only.
    case localNetwork
    case internet

    public init(host: String) {
        let name = host.lowercased()
        if name.hasSuffix(".ts.net") { self = .tailnet; return }
        if name.hasSuffix(".local") { self = .localNetwork; return }
        var v4 = in_addr()
        if inet_pton(AF_INET, name, &v4) == 1 {
            let address = UInt32(bigEndian: v4.s_addr), a = address >> 24, b = address >> 16 & 0xFF
            if a == 100, b & 0xC0 == 64 { self = .tailnet }
            else if a == 10 || (a == 172 && b & 0xF0 == 16) || (a == 192 && b == 168) || (a == 169 && b == 254) {
                self = .localNetwork
            } else { self = .internet }
            return
        }
        var v6 = in6_addr()
        if inet_pton(AF_INET6, name, &v6) == 1 {
            let bytes = withUnsafeBytes(of: v6) { Array($0) }
            if bytes.starts(with: [0xFD, 0x7A, 0x11, 0x5C, 0xA1, 0xE0]) { self = .tailnet }
            else if bytes[0] & 0xFE == 0xFC || (bytes[0] == 0xFE && bytes[1] & 0xC0 == 0x80) { self = .localNetwork }
            else { self = .internet }
            return
        }
        self = .internet
    }

    /// This device's own Tailscale address while its tunnel is up: an IPv4
    /// address in 100.64.0.0/10 on a tunnel interface. A carrier's CGNAT
    /// address in the same range sits on the cellular interface instead.
    public static func deviceTailnetAddress() -> String? {
        var list: UnsafeMutablePointer<ifaddrs>?
        guard getifaddrs(&list) == 0, let first = list else { return nil }
        defer { freeifaddrs(list) }
        for entry in sequence(first: first, next: { $0.pointee.ifa_next }) {
            let name = String(cString: entry.pointee.ifa_name)
            guard name.hasPrefix("utun") || name.hasPrefix("tailscale"),
                  let address = entry.pointee.ifa_addr, Int32(address.pointee.sa_family) == AF_INET else { continue }
            let text = address.withMemoryRebound(to: sockaddr_in.self, capacity: 1) { pointer -> String? in
                var bytes = [CChar](repeating: 0, count: Int(INET_ADDRSTRLEN))
                var value = pointer.pointee.sin_addr
                return inet_ntop(AF_INET, &value, &bytes, socklen_t(bytes.count)) == nil ? nil : String(cString: bytes)
            }
            if let text, WorkspaceRouteNetwork(host: text) == .tailnet { return text }
        }
        return nil
    }

    /// Whether this device has a path to the address right now: always on the
    /// internet, with Tailscale up for the tailnet, and from an interface on
    /// the same IPv4 subnet for a private address. A .local name or private
    /// IPv6 address cannot be judged, so it counts as reachable.
    public static func isLikelyReachable(_ host: String) -> Bool {
        switch WorkspaceRouteNetwork(host: host) {
        case .internet: return true
        case .tailnet: return deviceTailnetAddress() != nil
        case .localNetwork:
            var target = in_addr()
            guard inet_pton(AF_INET, host, &target) == 1 else { return true }
            var list: UnsafeMutablePointer<ifaddrs>?
            guard getifaddrs(&list) == 0, let first = list else { return true }
            defer { freeifaddrs(list) }
            for entry in sequence(first: first, next: { $0.pointee.ifa_next }) {
                guard let address = entry.pointee.ifa_addr, let mask = entry.pointee.ifa_netmask,
                      Int32(address.pointee.sa_family) == AF_INET else { continue }
                let own = address.withMemoryRebound(to: sockaddr_in.self, capacity: 1) { $0.pointee.sin_addr.s_addr }
                let bits = mask.withMemoryRebound(to: sockaddr_in.self, capacity: 1) { $0.pointee.sin_addr.s_addr }
                if bits != 0, own & bits == target.s_addr & bits { return true }
            }
            return false
        }
    }
}

/// What a connection that never reached its machine means for the user, and
/// the one change that fixes it when there is one. A refused key or a changed
/// host identity is not explained here; those keep their own messages.
public struct WorkspaceConnectionHelp: Sendable, Equatable {
    public enum Remedy: Sendable, Equatable { case turnOnTailscale, allowLocalNetwork, none }
    public let title: String, message: String, remedy: Remedy

    public init(title: String, message: String, remedy: Remedy) {
        self.title = title; self.message = message; self.remedy = remedy
    }

    /// `device` names this device in the text, such as "this iPhone".
    public init?(failure: any Error, machine: String, host: String, port: UInt16,
                 onTailnet: Bool, device: String = "this device") {
        guard let failure = failure as? NativeSSHError, failure.isUnreachable else { return nil }
        let Device = device.prefix(1).uppercased() + device.dropFirst()
        switch WorkspaceRouteNetwork(host: host) {
        case .tailnet where !onTailnet:
            title = "Turn on Tailscale"
            message = "\(machine) is on your Tailscale network, and \(device) isn't connected to Tailscale. Open Tailscale, switch it on, then try again."
            remedy = .turnOnTailscale
        case .tailnet:
            title = "\(machine) isn't answering"
            message = "\(Device) is on Tailscale, but \(machine) (\(host)) didn't answer. It may be asleep, offline or signed out of Tailscale."
            remedy = .none
        case .localNetwork:
            title = "\(machine) is on a local network"
            message = "\(host) is a local-network address, so \(device) has to be on the same Wi-Fi as \(machine). If it is, allow Local Network access for FrankenTerm in Settings."
            remedy = .allowLocalNetwork
        case .internet:
            remedy = .none
            switch failure {
            case .resolution:
                title = "Couldn't find \(machine)"
                message = "\(Device) couldn't look up \(host). Check the internet connection, then try again."
            case .connection(ECONNREFUSED):
                title = "\(machine) refused the connection"
                message = "\(host) answered, but nothing is accepting SSH on port \(port)."
            default:
                title = "\(machine) isn't answering"
                message = "\(host) didn't answer on port \(port). The machine may be off, or a firewall may block SSH from this network."
            }
        }
    }

    /// The most useful explanation among several routes' failures: one the
    /// user can fix outright first, then the first route's.
    public static func best(_ helps: [WorkspaceConnectionHelp]) -> WorkspaceConnectionHelp? {
        helps.first { $0.remedy == .turnOnTailscale } ?? helps.first { $0.remedy == .allowLocalNetwork } ?? helps.first
    }
}

/// Reaches a route the way a connection does, through the transport and the
/// host-key exchange, and stops before any key is selected or offered. It
/// needs no protected storage and never prompts.
public enum WorkspaceRouteProbe {
    public struct Result: Sendable {
        public let network: WorkspaceRouteNetwork
        /// The pinned-key decision for the key the machine presented, when the
        /// exchange got that far.
        public let hostKey: WorkspaceDocument.HostVerificationDecision?
        public let peer: NativeSSHPeer?
        public let failure: (any Error)?
    }

    public static func run(route: WorkspaceDocument.Route, document: WorkspaceDocument,
                           timeout: Duration = .seconds(8)) async -> Result {
        let network = WorkspaceRouteNetwork(host: route.host)
        let session: NativeSSHSession
        do {
            session = try await NativeSSHConnector.connect(to: NativeSSHEndpoint(host: route.host, port: route.port),
                                                           timeout: timeout)
        } catch { return .init(network: network, hostKey: nil, peer: nil, failure: error) }
        let witness = PeerWitness(transportID: session.transportID)
        var failure: (any Error)?
        do { _ = try await session.authenticate(using: witness, timeout: timeout) } catch { failure = error }
        await session.close()
        guard let peer = await witness.peer else { return .init(network: network, hostKey: nil, peer: nil, failure: failure) }
        do {
            let identity = try SSHPublicIdentity(blob: peer.hostKey)
            let decision = try document.verifyHostKey(forRouteID: route.id,
                presented: .init(algorithm: identity.algorithm, fingerprint: identity.fingerprint),
                reviewedTrustIDs: Set(document.hostTrust.map(\.id)),
                support: WorkspaceNativeSSHAuthentication.routeSupport)
            return .init(network: network, hostKey: decision, peer: peer, failure: nil)
        } catch { return .init(network: network, hostKey: nil, peer: peer, failure: error) }
    }

    /// Records the peer that completed the exchange and declines to sign in.
    private actor PeerWitness: NativeSSHAuthentication {
        struct Stopped: Error {}
        let transportID: UUID
        var peer: NativeSSHPeer?
        init(transportID: UUID) { self.transportID = transportID }
        func identity(for peer: NativeSSHPeer) async throws -> NativeSSHIdentity {
            self.peer = peer
            throw Stopped()
        }
        func signature(for challenge: NativeSSHChallenge, peer: NativeSSHPeer) async throws -> Data { throw Stopped() }
        func validate(peer: NativeSSHPeer, identity: NativeSSHIdentity) async throws { throw Stopped() }
        nonisolated func cancel(transportID: UUID) {}
    }
}
