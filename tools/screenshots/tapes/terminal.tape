# Integrated terminal running the project's CLI and tests.
Source tapes/_settings.tape
Source tapes/_start.tape
Alt+t
Sleep 1.5s
Source tapes/_widen.tape
Show
Type "python3 -m inventory.cli report --store north"
Enter
Sleep 1s
Type "python3 -m unittest -v 2>&1 | tail -4"
Enter
Sleep 2s
Screenshot out/terminal.png
Sleep 1s
