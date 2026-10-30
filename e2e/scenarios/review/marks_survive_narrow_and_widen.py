#!/usr/bin/env python3
# Extends #912: the whole range and a single commit can hold independent marks for the same file.
from e2e import Scenario, repos, scenario


@scenario
def main(s: Scenario) -> None:
    r = repos.fresh(s.dir / "repo")
    repos.run(r, "git", "init", "-q", "-b", "main", str(r))
    repos.write(r / "a.txt", "one\n")
    repos.write(r / "b.txt", "x\n")
    repos.git(r, "add", "-A")
    repos.git(r, "commit", "-qm", "c1")
    repos.write(r / "a.txt", "one\ntwo\n")
    repos.write(r / "b.txt", "x2\n")
    repos.git(r, "commit", "-qam", "c2")
    repos.write(r / "a.txt", "one\ntwo\nthree\n")
    repos.git(r, "commit", "-qam", "c3")
    tui = s.start("-r", "HEAD~2..HEAD", cwd=r)
    tui.wait_for("Files · 0/2").keys("Escape")
    tui.cursor_on("three").keys("r").wait_for("Files · 1/2").expect(r"✓ a\.txt")
    tui.keys(")").wait_for(r"Commit Message \(").wait_for("Files · 0/")
    tui.cursor_on("two").keys("r").wait_for("Files · 1/").expect(r"✓ a\.txt")
    tui.keys(")").wait_gone(r"Commit Message \(").settle()
    tui.expect("Files · 1/2")
    tui.keys(")").wait_for(r"Commit Message \(").wait_for("Files · 1/")
    tui.cursor_on(r"✓ a\.txt").keys("r").wait_for("Files · 0/")
    tui.keys(")").wait_gone(r"Commit Message \(").wait_for("Files · 1/2")
    tui.save()
    tui.quit()
