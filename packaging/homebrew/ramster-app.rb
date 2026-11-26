# Homebrew cask for the ramster menu bar app.
#
# Lives in the tap repo github.com/wasborntobeawesome/homebrew-tap as Casks/ramster-app.rb:
#   brew install --cask wasborntobeawesome/tap/ramster-app
#
# Only publish this for releases built with a Developer ID and notarized (the
# release workflow does that when the signing secrets are set); an ad-hoc signed
# app is blocked by Gatekeeper after a cask download.
#
# On each release update version and sha256 (the ramster-<version>.app.zip line
# of the release's SHA256SUMS).
cask "ramster-app" do
  version "0.1.0"
  sha256 "REPLACE_WITH_SHA256_OF_ramster-#{version}.app.zip"

  url "https://github.com/wasborntobeawesome/ramster/releases/download/v#{version}/ramster-#{version}.app.zip"
  name "ramster"
  desc "Menu bar app that shows what is eating your RAM and frees it safely"
  homepage "https://github.com/wasborntobeawesome/ramster"

  depends_on macos: ">= :sonoma"

  app "ramster.app"

  uninstall quit: "dev.ramster.bar"

  # The background sampler may run the CLI from inside the app bundle; zap
  # removes it together with the history.
  zap launchctl: "dev.ramster.daemon",
      trash:     [
        "~/.ramster",
        "~/Library/LaunchAgents/dev.ramster.daemon.plist",
      ]
end
