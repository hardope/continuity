import SwiftUI

@main
struct ContinuityApp: App {
    @StateObject private var controller = EngineController()

    var body: some Scene {
        WindowGroup {
            ContentView()
                .environmentObject(controller)
        }
    }
}
