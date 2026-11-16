import Foundation

/// The exact algorithms negotiated by one SSH transport. The two traffic
/// directions are independent; checking only the outbound cipher or MAC does
/// not establish compliance for data received from the server.
public struct SSHTransportAlgorithms: Hashable, Sendable {
    public let keyExchange: String
    public let outboundCipher: String, inboundCipher: String
    public let outboundMAC: String, inboundMAC: String

    public init(keyExchange: String, outboundCipher: String, inboundCipher: String,
                outboundMAC: String, inboundMAC: String) {
        self.keyExchange = keyExchange
        self.outboundCipher = outboundCipher; self.inboundCipher = inboundCipher
        self.outboundMAC = outboundMAC; self.inboundMAC = inboundMAC
    }

    /// Actual linked capability, not merely an advertised marker. This also
    /// rejects empty or synthetic method evidence before credentials are used.
    public func isSupported(keyExchangeAlgorithms: Set<String>, ciphers: Set<String>, macs: Set<String>) -> Bool {
        keyExchangeAlgorithms.contains(keyExchange)
            && ciphers.contains(outboundCipher) && ciphers.contains(inboundCipher)
            && macs.contains(outboundMAC) && macs.contains(inboundMAC)
    }
}

/// Mandatory literal SSH transport algorithm restrictions. Nil leaves a
/// dimension unconstrained; an empty array deliberately permits no algorithm.
/// Relative OpenSSH expressions are resolved or refused by the source reader,
/// never interpreted against guessed desktop defaults here.
public struct WorkspaceSSHTransportAlgorithmPolicy: Sendable, Equatable, Codable {
    public static let maximumAlgorithms = 64
    public let keyExchangeAlgorithms: [String]?
    public let ciphers: [String]?
    public let macs: [String]?
    public var isConstrained: Bool { keyExchangeAlgorithms != nil || ciphers != nil || macs != nil }

    public init(keyExchangeAlgorithms: [String]? = nil, ciphers: [String]? = nil, macs: [String]? = nil) throws {
        func checked(_ values: [String]?) throws -> [String]? {
            guard let values else { return nil }
            guard values.count <= Self.maximumAlgorithms else { throw FTError.limit("trust.transport_algorithms") }
            guard Set(values).count == values.count else { throw FTError.malformed("trust.duplicate_transport_algorithm") }
            return try values.map(WorkspaceDocument.algorithm)
        }
        self.keyExchangeAlgorithms = try checked(keyExchangeAlgorithms)
        self.ciphers = try checked(ciphers); self.macs = try checked(macs)
    }

    private init(validatedKEX: [String]?, ciphers: [String]?, macs: [String]?) {
        keyExchangeAlgorithms = validatedKEX; self.ciphers = ciphers; self.macs = macs
    }

    public static let unconstrained = Self(validatedKEX: nil, ciphers: nil, macs: nil)

    public init(from decoder: any Decoder) throws {
        let fields = try decoder.container(keyedBy: CodingKeys.self)
        try self.init(keyExchangeAlgorithms: fields.decodeIfPresent([String].self, forKey: .keyExchangeAlgorithms),
                      ciphers: fields.decodeIfPresent([String].self, forKey: .ciphers),
                      macs: fields.decodeIfPresent([String].self, forKey: .macs))
    }

    /// Conjoin current aliases and historical restrictions without restoring
    /// removed algorithms. The receiver's order remains authoritative.
    public func intersecting(_ other: Self) -> Self {
        func intersection(_ lhs: [String]?, _ rhs: [String]?) -> [String]? {
            guard let lhs else { return rhs }
            guard let rhs else { return lhs }
            let accepted = Set(rhs)
            return lhs.filter(accepted.contains)
        }
        return .init(validatedKEX: intersection(keyExchangeAlgorithms, other.keyExchangeAlgorithms),
                     ciphers: intersection(ciphers, other.ciphers), macs: intersection(macs, other.macs))
    }

    public func filterKeyExchangeAlgorithms(_ qualified: [String]) -> [String] { filter(keyExchangeAlgorithms, qualified) }
    public func filterCiphers(_ qualified: [String]) -> [String] { filter(ciphers, qualified) }
    public func filterMACs(_ qualified: [String]) -> [String] { filter(macs, qualified) }

    private func filter(_ allowed: [String]?, _ qualified: [String]) -> [String] {
        guard let allowed else { return qualified }
        let supported = Set(qualified)
        return allowed.filter(supported.contains)
    }

    /// A restricted route requires a complete negotiated transport witness.
    /// Supplying only requested preferences cannot satisfy this check.
    public func permits(_ evidence: SSHTransportAlgorithms?) -> Bool {
        guard let evidence else { return !isConstrained }
        guard [evidence.keyExchange, evidence.outboundCipher, evidence.inboundCipher,
               evidence.outboundMAC, evidence.inboundMAC].allSatisfy({ (try? WorkspaceDocument.algorithm($0)) != nil }) else {
            return false
        }
        return (keyExchangeAlgorithms?.contains(evidence.keyExchange) ?? true)
            && (ciphers?.contains(evidence.outboundCipher) ?? true)
            && (ciphers?.contains(evidence.inboundCipher) ?? true)
            && (macs?.contains(evidence.outboundMAC) ?? true)
            && (macs?.contains(evidence.inboundMAC) ?? true)
    }

    /// Readiness needs a qualified algorithm in every restricted dimension.
    /// Unconstrained legacy routes remain compatible with older profiles.
    public func hasSupportedAlgorithms(keyExchangeAlgorithms: Set<String>, ciphers: Set<String>, macs: Set<String>) -> Bool {
        if isConstrained && (keyExchangeAlgorithms.isEmpty || ciphers.isEmpty || macs.isEmpty) { return false }
        func available(_ allowed: [String]?, _ supported: Set<String>) -> Bool {
            allowed.map { !supported.isDisjoint(with: $0) } ?? true
        }
        return available(self.keyExchangeAlgorithms, keyExchangeAlgorithms)
            && available(self.ciphers, ciphers) && available(self.macs, macs)
    }

    func isAtLeastAsRestrictive(as other: Self) -> Bool {
        func contains(_ lhs: [String]?, _ rhs: [String]?) -> Bool {
            guard let rhs else { return true }
            guard let lhs else { return false }
            return Set(lhs).isSubset(of: Set(rhs))
        }
        return contains(keyExchangeAlgorithms, other.keyExchangeAlgorithms)
            && contains(ciphers, other.ciphers) && contains(macs, other.macs)
    }
}
