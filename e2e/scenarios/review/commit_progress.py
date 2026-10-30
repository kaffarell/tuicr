#!/usr/bin/env python3
# The inline pane shows review progress separately from selection, before visiting a commit.
from e2e import Scenario, repos, scenario


@scenario
def main(s: Scenario) -> None:
    repo = repos.empty_repo(s.dir / "repo")
    repos.git(repo, "commit", "--allow-empty", "-qm", "base")
    repos.write(repo / "a.txt", "first change\n")
    repos.git(repo, "add", "a.txt")
    repos.git(repo, "commit", "-qm", "first commit")
    repos.write(repo / "b.txt", "second change\n")
    repos.git(repo, "add", "b.txt")
    repos.git(repo, "commit", "-qm", "second commit")
    tui = s.start("-r", "HEAD~2..HEAD", cwd=repo)
    tui.wait_for(r"▣ .*first commit").wait_for("Files · 0/2").keys("Escape")
    tui.cursor_on("first change").keys("{").settle().keys("r").wait_for("Files · 1/2")
    tui.wait_for(r"✓ ▣ .*first commit").reject(r"[✓!] ▣ .*second commit")
    tui.cursor_on("second change").comment("Check the second file.")
    tui.wait_for(r"! ▣ .*second commit").expect(r"✓ ▣ .*first commit")
    tui.save()
    tui.quit()
