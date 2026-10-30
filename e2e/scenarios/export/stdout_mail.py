#!/usr/bin/env python3
# :mail writes a quoted commit reply, not the uncommitted working-tree version.
from e2e import TUICR_BIN, Scenario, repos, scenario


@scenario
def main(s: Scenario) -> None:
    repo = repos.git_repo(s.dir / "repo")
    repos.write(repo / "src/main.rs", 'fn main() { println!("uncommitted"); }\n')
    out = s.dir / "stdout.txt"
    tui = s.start_cmd(
        ["sh", "-c", f"exec '{TUICR_BIN}' --no-update-check --stdout -r HEAD >'{out}'"],
        cwd=repo,
    )
    tui.wait_for("Files · 0/").keys("Escape")
    tui.cursor_on('println!\\("hello"\\)')
    tui.comment("Please use a more specific greeting.")
    tui.cmd("mail").wait_exit()
    reply = out.read_text()
    assert reply.startswith("> fix: greeting\n"), reply
    assert '> +    println!("hello");\n' in reply, reply
    assert "\nPlease use a more specific greeting.\n" in reply, reply
    assert "uncommitted" not in reply, reply
