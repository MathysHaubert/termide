# README hero: a tour of the main panels, one short scene each. Between scenes
# termide restarts on a fresh project out of frame (_next-scene.tape), so every
# scene reuses the navigation of its single-shot tape unchanged.
# docker: --cap-add SYS_ADMIN
Output out/tour.gif
Source tapes/_settings.tape
Set Framerate 12

# 1. Editor with LSP completion, outline, file manager and git status.
Hide
Type "source /opt/shots/env/setup.sh" Enter
Sleep 2s
Type "termide" Enter
Sleep 3s
Source tapes/_overview-steps.tape
Show
Sleep 1.5s
Down 3
Sleep 400ms
Type "    total = store."
Sleep 3s
Escape
Sleep 300ms

# 2. Dual-pane file manager: the project and its feature worktree.
Source tapes/_next-scene.tape
Down 3
Right
Sleep 800ms
Down 4
Right
Sleep 800ms
Down 4
Alt+u
Sleep 400ms
Ctrl+g
Sleep 600ms
Backspace 60
Type "/tmp/demo/inventory-csv-export"
Enter
Sleep 1.5s
Down 5
Right
Sleep 800ms
Down 2
Alt+j
Sleep 1s
Show
Sleep 1s
Down
Sleep 500ms
Down
Sleep 500ms
Down
Sleep 1.5s

# 3. Integrated terminal: the project's CLI and tests.
Source tapes/_next-scene.tape
Alt+t
Sleep 1.5s
Source tapes/_widen.tape
Show
Sleep 300ms
Type "python3 -m inventory.cli report --store north"
Enter
Sleep 1s
Type "python3 -m unittest -v 2>&1 | tail -4"
Enter
Sleep 2.5s

# 4. Git log: the commit graph with a merged branch and a worktree branch.
Source tapes/_next-scene.tape
Alt+c
Sleep 2s
Source tapes/_widen.tape
Show
Sleep 1s
Down 2
Sleep 600ms
Down 2
Sleep 2s

# 5. Rendered Markdown with an embedded Mermaid flowchart.
Source tapes/_next-scene.tape
Down 8
Type "v"
Sleep 2s
Source tapes/_widen.tape
Show
Sleep 2s
Down 8
Sleep 2s

# 6. SQLite database viewer.
Source tapes/_next-scene.tape
Down 3
Right
Sleep 800ms
Down
Enter
Sleep 2s
Source tapes/_widen.tape
Show
Sleep 1.5s
Down 3
Sleep 1.5s

# 7. Hex view of a binary file.
Source tapes/_next-scene.tape
Down 3
Right
Sleep 800ms
Down 2
Type "v"
Sleep 2s
Source tapes/_widen.tape
Show
Sleep 2.5s

# 8. Resource monitor, in a PID namespace with the demo services only (see
# monitor.tape for the walk from the menu bar to the CPU modal).
Hide
Alt+q
Sleep 800ms
Enter
Sleep 1s
Type "source /opt/shots/env/setup.sh" Enter
Sleep 2s
Type "/opt/shots/env/demo-ns.sh termide" Enter
Sleep 5s
Alt+M
Sleep 400ms
Left
Sleep 500ms
Left
Sleep 500ms
Left@250ms 7
Sleep 1s
Left
Sleep 4s
Show
Sleep 2s
Left
Sleep 2.5s

# 9. Coding agent against the scripted mock model, through a permission card.
Hide
Escape
Sleep 300ms
Escape
Sleep 300ms
Source tapes/_next-scene.tape
Alt+a
Sleep 2s
Source tapes/_widen.tape
Show
Sleep 500ms
Type "Flag critical low-stock items in the reorder report, then run the tests"
Sleep 500ms
Enter
Sleep 10s
Type "1"
Sleep 6s
