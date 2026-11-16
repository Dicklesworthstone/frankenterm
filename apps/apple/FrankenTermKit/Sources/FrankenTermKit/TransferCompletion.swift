import Foundation

/// What a device that finished importing a transfer can tell the Mac that
/// sent it. `workspaceDigest` is SHA-256 of the decrypted manifest: only the
/// Mac that sealed the transfer and a device that opened it know it, since
/// the animation shows ciphertext alone.
public struct TransferArrival: Sendable, Equatable {
    public let envelopeDigest: Data, workspaceDigest: Data
    public init(envelopeDigest: Data, workspaceDigest: Data) {
        self.envelopeDigest = envelopeDigest; self.workspaceDigest = workspaceDigest
    }
}

#if canImport(MultipeerConnectivity) && canImport(CryptoKit)
import CryptoKit
import MultipeerConnectivity

/// How a phone tells the Mac that sent a transfer that it arrived, so the Mac
/// can say so and stop its animation. Local link only: while it plays, the Mac
/// advertises the Bonjour service `_ft-xfer._tcp` tagged with the envelope
/// digest's first 8 bytes. Once its import has committed, the phone invites
/// the matching Mac with a proof that it opened this transfer:
/// HMAC-SHA256(key: workspace digest, "FTDONE1" + envelope digest). No
/// workspace data crosses, and the Mac declines every invitation, so no
/// session ever forms.
public enum TransferCompletion {
    public static let serviceType = "ft-xfer"

    static func tag(_ arrival: TransferArrival) -> String { TransferCodeFrame.digestPrefix(arrival.envelopeDigest) }

    static func proof(_ arrival: TransferArrival) -> Data {
        let message = Data("FTDONE1".utf8) + arrival.envelopeDigest
        return Data(HMAC<SHA256>.authenticationCode(for: message, using: SymmetricKey(data: arrival.workspaceDigest)))
    }
}

/// The Mac's side: advertises while its animation plays and reports the first
/// invitation that proves this transfer arrived.
@MainActor
public final class TransferCompletionListener: NSObject, MCNearbyServiceAdvertiserDelegate {
    private let proof: Data
    private let advertiser: MCNearbyServiceAdvertiser
    private var onArrival: (() -> Void)?

    public init(arrival: TransferArrival, onArrival: @escaping @MainActor () -> Void) {
        proof = TransferCompletion.proof(arrival)
        advertiser = MCNearbyServiceAdvertiser(peer: MCPeerID(displayName: "FrankenTerm Mac"),
                                               discoveryInfo: ["t": TransferCompletion.tag(arrival)],
                                               serviceType: TransferCompletion.serviceType)
        self.onArrival = onArrival
        super.init()
        advertiser.delegate = self
        advertiser.startAdvertisingPeer()
    }

    /// Stops advertising; no later invitation is reported.
    public func stop() {
        onArrival = nil
        advertiser.stopAdvertisingPeer()
        advertiser.delegate = nil
    }

    private func received(_ context: Data) {
        guard context == proof, let onArrival else { return }
        stop()
        onArrival()
    }

    public nonisolated func advertiser(_ advertiser: MCNearbyServiceAdvertiser, didReceiveInvitationFromPeer peerID: MCPeerID,
                                       withContext context: Data?, invitationHandler: @escaping (Bool, MCSession?) -> Void) {
        invitationHandler(false, nil)
        guard let context, context.count == 32 else { return }
        Task { @MainActor in self.received(context) }
    }

    public nonisolated func advertiser(_ advertiser: MCNearbyServiceAdvertiser, didNotStartAdvertisingPeer error: any Error) {}
}

/// The phone's side: finds the Mac that sent `arrival` and invites it once
/// with the proof. True once that Mac has answered the invitation (it always
/// declines); false when no such Mac answered within `timeout`.
@MainActor
public final class TransferCompletionNotice: NSObject, MCNearbyServiceBrowserDelegate, MCSessionDelegate {
    private let arrival: TransferArrival
    private let peer = MCPeerID(displayName: "FrankenTerm iPhone")
    private let browser: MCNearbyServiceBrowser
    private let session: MCSession
    private var invited: MCPeerID?
    private var finish: ((Bool) -> Void)?

    private init(arrival: TransferArrival) {
        self.arrival = arrival
        browser = MCNearbyServiceBrowser(peer: peer, serviceType: TransferCompletion.serviceType)
        session = MCSession(peer: peer, securityIdentity: nil, encryptionPreference: .required)
        super.init()
    }

    /// Cancelling the calling task ends the search at once, returning false.
    public static func send(_ arrival: TransferArrival, timeout: Duration = .seconds(15)) async -> Bool {
        let notice = TransferCompletionNotice(arrival: arrival)
        return await withTaskCancellationHandler {
            await withCheckedContinuation { continuation in
                notice.finish = { continuation.resume(returning: $0) }
                guard !Task.isCancelled else { notice.done(false); return }
                notice.browser.delegate = notice; notice.session.delegate = notice
                notice.browser.startBrowsingForPeers()
                Task { @MainActor in
                    try? await Task.sleep(for: timeout)
                    notice.done(false)
                }
            }
        } onCancel: {
            Task { @MainActor in notice.done(false) }
        }
    }

    private func done(_ answered: Bool) {
        guard let finish else { return }
        self.finish = nil
        browser.stopBrowsingForPeers(); browser.delegate = nil
        session.disconnect(); session.delegate = nil
        finish(answered)
    }

    /// MCPeerID is an immutable identifier that MultipeerConnectivity hands
    /// out on its own queue; it is only compared and passed back here.
    private struct Peer: @unchecked Sendable { let id: MCPeerID }

    private func found(_ peer: Peer, tag: String?) {
        guard finish != nil, invited == nil, tag == TransferCompletion.tag(arrival) else { return }
        invited = peer.id
        browser.invitePeer(peer.id, to: session, withContext: TransferCompletion.proof(arrival), timeout: 10)
    }

    private func changed(_ peer: Peer, to state: MCSessionState) {
        // The Mac declines: the invitation reached it and was answered.
        if peer.id == invited, state == .notConnected { done(true) }
    }

    public nonisolated func browser(_ browser: MCNearbyServiceBrowser, foundPeer peerID: MCPeerID,
                                    withDiscoveryInfo info: [String: String]?) {
        let tag = info?["t"], peer = Peer(id: peerID)
        Task { @MainActor in self.found(peer, tag: tag) }
    }
    public nonisolated func browser(_ browser: MCNearbyServiceBrowser, lostPeer peerID: MCPeerID) {}
    public nonisolated func browser(_ browser: MCNearbyServiceBrowser, didNotStartBrowsingForPeers error: any Error) {
        Task { @MainActor in self.done(false) }
    }
    public nonisolated func session(_ session: MCSession, peer peerID: MCPeerID, didChange state: MCSessionState) {
        let peer = Peer(id: peerID)
        Task { @MainActor in self.changed(peer, to: state) }
    }
    public nonisolated func session(_ session: MCSession, didReceive data: Data, fromPeer peerID: MCPeerID) {}
    public nonisolated func session(_ session: MCSession, didReceive stream: InputStream, withName streamName: String,
                                    fromPeer peerID: MCPeerID) {}
    public nonisolated func session(_ session: MCSession, didStartReceivingResourceWithName resourceName: String,
                                    fromPeer peerID: MCPeerID, with progress: Progress) {}
    public nonisolated func session(_ session: MCSession, didFinishReceivingResourceWithName resourceName: String,
                                    fromPeer peerID: MCPeerID, at localURL: URL?, withError error: (any Error)?) {}
}
#endif
