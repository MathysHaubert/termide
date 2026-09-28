# Dual-pane file manager: the project with folders expanded on the left, the
# feature branch's worktree on the right.
Source tapes/_settings.tape
Source tapes/_start.tape
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
Sleep 500ms
Screenshot out/file-manager.png
Sleep 1s
