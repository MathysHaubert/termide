// Print the CGWindowID of the on-screen window whose title is exactly the
// first argument, or exit 1. Titles are visible to this call only with the
// Screen Recording permission, so exit 2 says that permission is missing.
import CoreGraphics
import Foundation

let wanted = CommandLine.arguments.dropFirst().first ?? ""
let options: CGWindowListOption = [.optionOnScreenOnly, .excludeDesktopElements]
guard let list = CGWindowListCopyWindowInfo(options, kCGNullWindowID) as? [[String: Any]] else {
    exit(1)
}
if !CGPreflightScreenCaptureAccess() {
    FileHandle.standardError.write("screen recording permission missing\n".data(using: .utf8)!)
    exit(2)
}
for window in list {
    if let name = window[kCGWindowName as String] as? String, name == wanted,
       let id = window[kCGWindowNumber as String] as? Int {
        print(id)
        exit(0)
    }
}
exit(1)
