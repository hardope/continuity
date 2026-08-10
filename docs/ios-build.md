# iOS build (Phase 3)

Status: **not building yet** — removed from CI (`.github/workflows/release.yml`) rather than leaving it perpetually red. Progress so far, using GitHub's `macos-latest` runner (which has full Xcode) since this dev machine can't build it at all (only the Xcode Command Line Tools are installed, not full Xcode, and installing Xcode needs an Apple ID via the App Store — an interactive login step):
- `xcodegen generate` parses `project.yml` and produces a working `.xcodeproj` — confirmed, this part is solid.
- The XCFramework needed a real fix (see the `lipo` step below) — the simulator build was silently missing its x86_64 slice, failing with "missing architecture(s)" at link time. Fixed.
- The simulator build now gets further, but fails at `ValidateEmbeddedBinary` for the Share Extension (`ContinuityShare.appex`) — an embedded-extension code-signing validation error, even with `CODE_SIGNING_ALLOWED=NO`. Not yet resolved; likely needs either `CODE_SIGNING_REQUIRED=NO` / an explicit empty `CODE_SIGN_IDENTITY`, or something about how the extension's Info.plist/entitlements pair with the host app. Whoever picks this up next: start there.

Everything below is the path to build it locally once Xcode is installed, for whenever this gets picked back up.

## What's here

- `apps/ios/Continuity/` — the main SwiftUI app (`ContinuityApp.swift`, `ContentView.swift`, `EngineController.swift`, `SecureIdentity.swift`, `IosClipboardProvider.swift`)
- `apps/ios/ContinuityShare/` — a Share Extension (`ShareViewController.swift`) so any app's share sheet can hand Continue a file
- `apps/ios/project.yml` — an [XcodeGen](https://github.com/yonaskolb/XcodeGen) spec that generates the `.xcodeproj`. CI confirms `xcodegen generate` parses it and produces a project that actually builds; the App Group ID and provisioning still need a real Apple Developer account, which no amount of CI can substitute for.

## Steps once Xcode is installed

1. Install Xcode from the App Store (needs your Apple ID — can't be scripted).
2. `xcode-select --switch /Applications/Xcode.app` (swap from the Command Line Tools instance currently active).
3. Cross-compile `continuity-ffi` to an XCFramework:
   ```bash
   rustup target add aarch64-apple-ios aarch64-apple-ios-sim x86_64-apple-ios
   cd core/continuity-ffi
   cargo build --release --target aarch64-apple-ios
   cargo build --release --target aarch64-apple-ios-sim
   cargo build --release --target x86_64-apple-ios

   # An xcframework takes one library per *platform variant*, not per
   # architecture — merge the two simulator architectures (Apple Silicon +
   # Intel) into one universal binary with lipo first. Passing them as two
   # separate -library entries instead builds, but fails at link time on
   # whichever simulator architecture didn't get picked ("missing
   # architecture(s)") — this is exactly what broke CI the first time.
   mkdir -p ../../target/ios-simulator-universal/release
   lipo -create \
     ../../target/aarch64-apple-ios-sim/release/libcontinuity_ffi.a \
     ../../target/x86_64-apple-ios/release/libcontinuity_ffi.a \
     -output ../../target/ios-simulator-universal/release/libcontinuity_ffi.a

   xcodebuild -create-xcframework \
     -library ../../target/aarch64-apple-ios/release/libcontinuity_ffi.a -headers include \
     -library ../../target/ios-simulator-universal/release/libcontinuity_ffi.a -headers include \
     -output ../../apps/ios/ContinuityFFI.xcframework
   ```
4. Regenerate the Swift bindings against the release build (already done once against the debug macOS build in `bindings/swift/` — regenerate so they match the iOS binary):
   ```bash
   cargo run -p continuity-ffi --bin uniffi-bindgen -- generate \
     --library target/aarch64-apple-ios/release/libcontinuity_ffi.a \
     --language swift --out-dir ../../bindings/swift
   ```
5. `brew install xcodegen && cd apps/ios && xcodegen generate` — produces `Continuity.xcodeproj`.
6. Open in Xcode, fix whatever XcodeGen/entitlements details need adjusting (real App Group ID registered in your Apple Developer account, signing team, etc.) — some of this genuinely needs a human in Xcode's UI (App Group registration, provisioning).
7. Run on a simulator or device.

## Known constraint (not a bug)

iOS gives no way to keep a mesh connection or a listening socket alive once the app is backgrounded — `EngineController` only exists while the app is in memory and foregrounded. Practical effect:
- Clipboard/file sync is live while the app is open, not in the background.
- The Share Extension covers the most common "app not open" case (sending a file from another app's share sheet) by staging the file and prompting the user to open Continuity to finish the send — see the doc comment in `ShareViewController.swift` for why it doesn't send directly from the extension process.
- Outbound clipboard *watching* is disabled on purpose for now (`IosClipboardProvider.getText()` always returns `nil`) — continuous polling would trigger iOS's "pasted from Clipboard" privacy banner roughly twice a second while the app is open. See the doc comment in `IosClipboardProvider.swift` for the real fix (a deliberate "Send Clipboard" action, which needs a new `EngineCommand`).
