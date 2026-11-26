# Homebrew formula for the ramster CLI (prebuilt universal binary).
#
# Lives in the tap repo github.com/wasborntobeawesome/homebrew-tap as Formula/ramster.rb:
#   brew install wasborntobeawesome/tap/ramster
#
# On each release (see the release workflow), update:
#   version  -> the new version (without the leading "v")
#   sha256   -> the ramster-<version>-macos-universal.tar.gz line of the release's SHA256SUMS
#               (also printed in the workflow summary), or:
#               curl -sL <url> | shasum -a 256
class Ramster < Formula
  desc "See what is eating your RAM, free it safely, and let your AI agents do it too"
  homepage "https://github.com/wasborntobeawesome/ramster"
  version "0.1.0"
  url "https://github.com/wasborntobeawesome/ramster/releases/download/v#{version}/ramster-#{version}-macos-universal.tar.gz"
  sha256 "REPLACE_WITH_SHA256_OF_ramster-#{version}-macos-universal.tar.gz"
  license "MIT"

  depends_on :macos

  def install
    bin.install "ramster"
  end

  def caveats
    <<~EOS
      Optional background sampler (~5 MB RAM, starts at login):
        ramster agent install

      Let your AI agents use it (MCP):
        claude mcp add --scope user ramster -- #{opt_bin}/ramster mcp
    EOS
  end

  test do
    assert_match "ramster #{version}", shell_output("#{bin}/ramster version")
  end
end
