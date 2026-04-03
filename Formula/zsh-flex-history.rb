class ZshFlexHistory < Formula
  desc "Interactive zsh history search with flex matching and mouse support"
  homepage "https://github.com/alex-903/zsh-mouse-and-flex-search"
  license "MIT"

  head "https://github.com/alex-903/zsh-mouse-and-flex-search.git", branch: "main"

  depends_on "python@3.12"

  def install
    libexec.install "zsh_flex_history.py"
    libexec.install "zsh_syntax_highlighting.py"
    libexec.install "esc_mode.py"
    libexec.install "convert_zsh_history_to_db.py"
    libexec.install "cmd.sh" if File.exist?("cmd.sh")

    (bin/"zsh-flex-history").write <<~EOS
      #!/bin/bash
      exec "#{Formula["python@3.12"].opt_bin}/python3" "#{libexec}/zsh_flex_history.py" "$@"
    EOS
    (bin/"zsh-flex-history-import").write <<~EOS
      #!/bin/bash
      exec "#{Formula["python@3.12"].opt_bin}/python3" "#{libexec}/convert_zsh_history_to_db.py" "$@"
    EOS
    chmod 0755, bin/"zsh-flex-history"
    chmod 0755, bin/"zsh-flex-history-import"

    (share/"zsh-flex-history").install "zsh-flex-history.zsh"
  end

  def caveats
    <<~EOS
      Add this line to your ~/.zshrc:

        source "$(brew --prefix)/share/zsh-flex-history/zsh-flex-history.zsh"

      The hook will use the installed binary:

        #{opt_bin}/zsh-flex-history
    EOS
  end

  test do
    assert_match "--print-only", shell_output("#{bin}/zsh-flex-history --help")
  end
end
