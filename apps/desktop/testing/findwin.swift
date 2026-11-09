import CoreGraphics
import Foundation
// List on-screen windows and print "<id> <pid> <owner> <w>x<h>" for each,
// so a window can be captured by id regardless of z-order.
let opts = CGWindowListOption(arrayLiteral: .optionOnScreenOnly, .excludeDesktopElements)
guard let windows = CGWindowListCopyWindowInfo(opts, kCGNullWindowID) as? [[String: Any]] else {
    exit(1)
}
for w in windows {
    let owner = w[kCGWindowOwnerName as String] as? String ?? "?"
    let id = w[kCGWindowNumber as String] as? Int ?? -1
    let pid = w[kCGWindowOwnerPID as String] as? Int ?? -1
    let b = w[kCGWindowBounds as String] as? [String: Any] ?? [:]
    let width = b["Width"] as? Double ?? 0
    let height = b["Height"] as? Double ?? 0
    let x = b["X"] as? Double ?? 0
    let y = b["Y"] as? Double ?? 0
    print("\(id) \(pid) \(owner) \(Int(width))x\(Int(height)) @\(Int(x)),\(Int(y))")
}
