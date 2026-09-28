# Coding agent against the scripted mock model: read, edit, a permission
# card for bash, then the answer.
Output out/agent.gif
Source tapes/_settings.tape
Source tapes/_start.tape
Alt+a
Sleep 2s
Source tapes/_widen.tape
Show
Sleep 500ms
Type "Flag critical low-stock items in the reorder report, then run the tests"
Sleep 500ms
Screenshot out/agent-prompt.png
Enter
Sleep 4s
Screenshot out/agent-working.png
Sleep 6s
Screenshot out/agent-permission.png
Type "1"
Sleep 10s
Screenshot out/agent-done.png
Sleep 2s
