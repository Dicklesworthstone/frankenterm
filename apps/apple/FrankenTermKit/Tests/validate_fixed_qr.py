#!/usr/bin/env python3
"""Independently decode production QR rasters emitted by FixedQRCodeTests.

Optional validation tool; no Python/decoder dependency enters an Apple product.
With Pillow and zxing-cpp installed in an isolated environment:

  FT_QR_VALIDATION_DIRECTORY=/tmp/ft-qr-new swift test --filter FixedQRCodeTests
  python3 Tests/validate_fixed_qr.py /tmp/ft-qr-new

The directory must be new because Swift writes artifacts without overwriting.
This script checks original and PNG-roundtripped pixels, exact decoded bytes,
ECC Q, and complete inclusion of the already-verified workspace reconstruction.
It never regenerates expected matrices or relaxes decoder errors.
"""

import argparse
import importlib.metadata
import json
from pathlib import Path

from PIL import Image
import zxingcpp


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("directory", type=Path)
    args = parser.parse_args()
    root = args.directory.resolve(strict=True)
    manifest = json.loads((root / "manifest.json").read_text())
    if (manifest["version"], manifest["moduleSide"], manifest["rasterSide"], manifest["correctionLevel"]) != (22, 105, 113, "Q"):
        raise ValueError("Unexpected QR profile")
    results = []
    names = set()
    for item in manifest["cases"]:
        name = item["name"]
        if name in names or Path(item["pgm"]).name != item["pgm"]:
            raise ValueError("Duplicate case or nonlocal image path")
        names.add(name)
        with Image.open(root / item["pgm"]) as source:
            source.load()
            if source.mode != "L" or source.size != (item["pixelSide"], item["pixelSide"]):
                raise ValueError(f"{name}: wrong grayscale geometry")
            png = root / f"{name}.png"
            with png.open("xb") as output:
                source.save(output, format="PNG")
            with Image.open(png) as image:
                image.load()
                if image.tobytes() != source.tobytes():
                    raise ValueError(f"{name}: PNG changed raster pixels")
                # Normal detector, including the smallest one-pixel-per-module
                # presentation. No pure-image shortcut, rescaling, or crop.
                decoded = zxingcpp.read_barcode(image, formats=zxingcpp.BarcodeFormat.QRCode)
                if decoded is None or not decoded.valid or decoded.error:
                    raise ValueError(f"{name}: independent decoder failed")
                if decoded.bytes != item["text"].encode("ascii") or decoded.text != item["text"]:
                    raise ValueError(f"{name}: decoded payload mismatch")
                if decoded.ec_level != "Q":
                    raise ValueError(f"{name}: decoded ECC was {decoded.ec_level!r}")
                results.append({"name": name, "textBytes": len(decoded.bytes),
                                "pixelSide": image.width, "correctionLevel": decoded.ec_level})
    recovery = set(manifest["recoveryFrameNames"])
    if not recovery or not recovery.issubset(names):
        raise ValueError("Complete workspace reconstruction frames are missing")
    report = {"decoder": "zxing-cpp", "decoderVersion": importlib.metadata.version("zxing-cpp"),
              "decodedCases": len(results), "workspaceFrames": len(recovery),
              "recoveredEnvelopeSHA256": manifest["recoveredEnvelopeSHA256"], "results": results}
    with (root / "independent-decoder-report.json").open("x") as output:
        json.dump(report, output, indent=2)
        output.write("\n")
    print(json.dumps({key: value for key, value in report.items() if key != "results"}, indent=2))


if __name__ == "__main__":
    main()
