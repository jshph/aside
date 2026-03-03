class Aside < Formula
  desc "Record mic + system audio side-by-side with a memo, then distill into structured notes"
  homepage "https://github.com/jshph/aside"
  url "https://github.com/jshph/aside/archive/refs/tags/v0.1.0.tar.gz"
  sha256 "PLACEHOLDER"
  license "GPL-3.0-or-later"

  depends_on :macos
  depends_on "rust" => :build

  def install
    system "cargo", "install", *std_cargo_args
    bin.install "aside.py"
    prefix.install "SKILL.md"
    prefix.install "install.sh"
  end

  def caveats
    <<~EOS
      aside requires:

      1. Screen & System Audio Recording permission for your terminal app.
         Grant in: System Settings > Privacy & Security > Screen & System Audio Recording
         Or run: bash #{prefix}/install.sh

      2. whisper-cpp for transcription:
         brew install whisper-cpp

      3. ffmpeg for audio conversion:
         brew install ffmpeg
    EOS
  end

  test do
    assert_match "aside", shell_output("#{bin}/aside --help")
  end
end
