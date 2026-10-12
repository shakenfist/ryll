# Homebrew formula for ryll, as first published to
# shakenfist/homebrew-tap. To use the tap:
#
#   brew tap shakenfist/tap
#   brew install ryll
#
# Or in one command:
#
#   brew install shakenfist/tap/ryll
#
# Nothing reads this file any more. release.yml's
# update-homebrew job edits the tap's own Formula/ryll.rb in
# place, rewriting its `version` and `sha256` lines on every
# release; the tap's url interpolates #{version}, so it needs
# no rewriting. The PLACEHOLDER_URL and PLACEHOLDER_SHA256
# values below are never substituted.

class Ryll < Formula
  desc "A Rust SPICE VDI client"
  homepage "https://github.com/shakenfist/ryll"
  version "0.1.0"
  license "Apache-2.0"

  on_macos do
    on_arm do
      url "PLACEHOLDER_URL"
      sha256 "PLACEHOLDER_SHA256"
    end
  end

  def install
    bin.install "ryll"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/ryll --version")
  end
end
