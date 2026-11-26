# Homebrew formula for ramster: the CLI plus the menu bar app.
#
# Lives in the tap repo github.com/wasborntobeawesome/homebrew-tap as Formula/ramster.rb:
#   brew install wasborntobeawesome/tap/ramster
#
# Works without an Apple Developer ID: Homebrew does not quarantine what a
# formula installs, so Gatekeeper never blocks the ad-hoc signed binaries.
# (A cask would be quarantined; see ramster-app.rb.)
#
# On each release, update the version in both URLs and the two sha256 values
# from the release's SHA256SUMS (also printed in the release workflow summary):
#   ramster-<version>-macos-universal.tar.gz  -> sha256 below
#   ramster-<version>.app.zip                 -> resource "app" sha256
class Ramster < Formula
  desc "See what is eating your RAM, free it safely, and let your AI agents do it too"
  homepage "https://github.com/wasborntobeawesome/ramster"
  url "https://github.com/wasborntobeawesome/ramster/releases/download/v0.1.0/ramster-0.1.0-macos-universal.tar.gz"
  sha256 "REPLACE_WITH_SHA256_OF_ramster-0.1.0-macos-universal.tar.gz"
  license "MIT"

  depends_on :macos

  resource "app" do
    url "https://github.com/wasborntobeawesome/ramster/releases/download/v0.1.0/ramster-0.1.0.app.zip"
    sha256 "REPLACE_WITH_SHA256_OF_ramster-0.1.0.app.zip"
  end

  def install
    bin.install "ramster"
    resource("app").stage do
      # Homebrew may already have changed into the single top-level directory.
      src = File.directory?("ramster.app") ? "ramster.app" : "."
      (prefix/"ramster.app").install Dir["#{src}/*"]
    end
  end

  def caveats
    <<~EOS
      Menu bar app (copy it so upgrades don't move it; turn on "Launch at login" in its menu):
        rm -rf ~/Applications/ramster.app && cp -R #{opt_prefix}/ramster.app ~/Applications/
        open ~/Applications/ramster.app

      Let your AI agents use it (MCP), asks before each step:
        ramster setup

      Optional background sampler (~5 MB RAM, starts at login):
        ramster agent install
    EOS
  end

  test do
    assert_match "ramster #{version}", shell_output("#{bin}/ramster version")
    assert_path_exists prefix/"ramster.app/Contents/MacOS/ramster-bar"
  end
end
