import Tauri
import UIKit

// Window insets for the frontend's `--native-inset-*` custom properties.

extension NativeUtilsPlugin {
  /// Reports the status bar, home indicator and sensor housing insets in CSS
  /// pixels. Points already are CSS pixels at the default page zoom.
  ///
  /// These are read from the window rather than the webview: the webview
  /// fills the window, and its own `safeAreaInsets` depend on how its scroll
  /// view adjusts content, which isn't what the page needs to pad by.
  @objc func get_window_insets(_ invoke: Invoke) {
    Task { @MainActor in
      let window =
        self.webview?.window
        ?? UIApplication.shared.connectedScenes
        .compactMap { ($0 as? UIWindowScene)?.keyWindow }
        .first
      let insets = window?.safeAreaInsets ?? .zero
      invoke.resolve(
        WindowInsetsResponse(
          top: insets.top,
          right: insets.right,
          bottom: insets.bottom,
          left: insets.left
        ))
    }
  }
}
