#!/usr/bin/env python3
"""Independently open maintained Swift HPKE outputs with pyhpke.

Optional validation tool; no Python dependency enters an Apple product.
Tested with pyhpke 0.6.5 in an isolated environment:

  FT_HPKE_VALIDATION_DIRECTORY=/tmp/ft-hpke-new swift test
  python3 Tests/validate_recipient_hpke.py /tmp/ft-hpke-new

The directory must be new. Tests emit only public synthetic keys and payloads.
All RFC 9180 cryptography is delegated to maintained pyhpke; the framing checks
verify the actual complete FTAG header, authenticated packing, and archive.
"""
import argparse
import hashlib
import importlib.metadata
import json
from pathlib import Path
import zlib

from pyhpke import AEADId, CipherSuite, KDFId, KEMId


def require(condition: bool, message: str) -> None:
    if not condition:
        raise ValueError(message)


def suite() -> CipherSuite:
    return CipherSuite.new(KEMId.DHKEM_X25519_HKDF_SHA256,
                           KDFId.HKDF_SHA256, AEADId.CHACHA20_POLY1305)


def open_hpke(private_key: bytes, enc: bytes, info: bytes, aad: bytes, ciphertext: bytes) -> bytes:
    implementation = suite()
    key = implementation.kem.deserialize_private_key(private_key)
    context = implementation.create_recipient_context(enc, key, info)
    return context.open(ciphertext, aad)


def verify_raw(value: dict) -> None:
    get = lambda name: bytes.fromhex(value[name])
    clear = open_hpke(get("privateKey"), get("encapsulatedKey"), get("info"), get("aad"), get("ciphertext"))
    require(clear == get("plaintext"), "Independent HPKE plaintext mismatch")


def verify_envelope(value: dict) -> dict:
    request = bytes.fromhex(value["requestCBOR"])
    digest = hashlib.sha256(request).digest()
    # These emitted fixtures deliberately use the canonical 4-MiB request.
    require(len(request) == 58 and request[:3] == b"\x84\x01\x50" and request[19:21] == b"\x58\x20",
            "Unexpected canonical request fixture")
    require(request[53:] == b"\x1a\x00\x40\x00\x00", "Unexpected fixture request cap")
    require(request[21:53] == bytes.fromhex(value["publicKey"]), "Request public-key mismatch")
    require(digest == bytes.fromhex(value["requestDigest"]), "Request digest mismatch")
    envelope = bytes.fromhex(value["envelope"])
    require(4222 <= len(envelope) <= 4 * 1024 * 1024, "Envelope outside advertised cap")
    require(envelope[:8] == b"FTAG\x01\x02\x00\x00", "Unexpected FTAG mode/version/flags")
    ciphertext_length = int.from_bytes(envelope[24:28], "big")
    require(int.from_bytes(envelope[28:30], "big") == 80 and len(envelope) == 110 + ciphertext_length,
            "Recipient header extent mismatch")
    require(envelope[30:46] == request[3:19] and envelope[46:78] == digest, "Request binding mismatch")
    require(ciphertext_length >= 4112 and (ciphertext_length - 16) % 4096 == 0, "Invalid padded extent")
    packed = open_hpke(bytes.fromhex(value["privateKey"]), envelope[78:110],
                       b"FrankenTerm/FTAG/v1/recipient\x00" + digest,
                       b"FrankenTerm/FTAG/v1/envelope\x00" + envelope[:110], envelope[110:])
    codec, archive_length, payload_length = packed[0], int.from_bytes(packed[1:5], "big"), int.from_bytes(packed[5:9], "big")
    require(0 < archive_length <= 8 * 1024 * 1024 and 0 < payload_length <= len(packed) - 9,
            "Authenticated packing lengths invalid")
    require(len(packed) == ((9 + payload_length + 4095) // 4096) * 4096, "Excess packing padding")
    payload = packed[9:9 + payload_length]
    require(not any(packed[9 + payload_length:]), "Nonzero authenticated padding")
    if codec == 0:
        archive = payload
    elif codec == 1:
        inflater = zlib.decompressobj()
        archive = inflater.decompress(payload, archive_length + 1)
        require(inflater.eof and not inflater.unused_data and not inflater.unconsumed_tail, "Invalid zlib extent")
    else:
        raise ValueError("Unsupported authenticated codec")
    require(len(archive) == archive_length and archive == bytes.fromhex(value["archive"]), "Archive bytes mismatch")
    return {"codec": codec, "envelopeBytes": len(envelope), "archiveBytes": len(archive),
            "envelopeSHA256": hashlib.sha256(envelope).hexdigest()}



def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("directory", type=Path)
    args = parser.parse_args()
    root = args.directory.resolve(strict=True)
    fixtures = Path(__file__).resolve().parents[4] / "fixtures/ios-airgap/recipient-hpke-vectors.json"
    reference = json.loads(fixtures.read_text())
    verify_raw(reference["rfc9180"])
    for value in reference["envelopes"]:
        verify_envelope(value)
    verify_raw(json.loads((root / "swift-hpke-sender.json").read_text()))
    envelope = verify_envelope(json.loads((root / "swift-recipient-envelope.json").read_text()))
    report = {"hpkeOracle": "pyhpke", "hpkeVersion": importlib.metadata.version("pyhpke"),
              "rfc9180VectorOpened": True, "independentReferenceEnvelopesOpened": len(reference["envelopes"]),
              "swiftHPKEOpened": True, "swiftRecipientEnvelope": envelope}
    with (root / "independent-hpke-report.json").open("x") as output:
        json.dump(report, output, indent=2)
        output.write("\n")
    print(json.dumps(report, indent=2))


if __name__ == "__main__":
    main()
