// For each on-screen window of PID: its bounds and the windows in front of it
// that intersect them (owner, layer, bounds, alpha). Front-to-back order from
// CGWindowListCopyWindowInfo; needs no Screen Recording permission.
// usage: winmap PID
import CoreGraphics
import Foundation

let pid = Int32(CommandLine.arguments[1])!
let all = (CGWindowListCopyWindowInfo([.optionOnScreenOnly], kCGNullWindowID) as? [[String: Any]]) ?? []
func rect(_ w: [String: Any]) -> CGRect {
    let b = w[kCGWindowBounds as String] as? [String: CGFloat] ?? [:]
    return CGRect(x: b["X"] ?? 0, y: b["Y"] ?? 0, width: b["Width"] ?? 0, height: b["Height"] ?? 0)
}
for (i, w) in all.enumerated() where (w[kCGWindowOwnerPID as String] as? Int32) == pid {
    let r = rect(w)
    print("window \(w[kCGWindowNumber as String] ?? "?") layer \(w[kCGWindowLayer as String] ?? "?") bounds \(r) z \(i)")
    for a in all[..<i] where rect(a).intersects(r) {
        let owner = a[kCGWindowOwnerName as String] ?? "?"
        print("  under: \(owner) layer \(a[kCGWindowLayer as String] ?? "?") bounds \(rect(a)) alpha \(a[kCGWindowAlpha as String] ?? "?")")
    }
}
