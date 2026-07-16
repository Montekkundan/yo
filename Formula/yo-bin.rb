class YoBin < Formula
  desc "Personal AI terminal assistant powered by Vercel AI Gateway"
  homepage "https://github.com/montekkundan/yo"
  version "1.3.5"

  if OS.mac?
    url "https://github.com/Montekkundan/yo/releases/download/1.3.5/yo-1.3.5-x86_64-apple-darwin.tar.gz"
    sha256 "d670c52ec3bedfbe97bd89f9c9550065dab1560592bd7a841b362e982ae66b3d"
  elsif OS.linux?
    url "https://github.com/Montekkundan/yo/releases/download/1.3.5/yo-1.3.5-x86_64-unknown-linux-musl.tar.gz"
    sha256 "a6588f772ec3428ea30df47b5c2b523a840500944209a71584ee5c0d4533e150"
  end

  def install
    bin.install "yo"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/yo --version")
  end
end
