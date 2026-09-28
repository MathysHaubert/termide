# Editor with pylsp: member completion on a Store.
Source tapes/_settings.tape
Hide
Type "source /opt/shots/env/setup.sh" Enter
Sleep 2s
Type "termide" Enter
Sleep 3s
Down 5
Right
Sleep 800ms
Down 4
Enter
Sleep 2s
Source tapes/_widen.tape
Down 15
Sleep 3s
Show
Type "    total = store."
Sleep 2.5s
Screenshot out/editor.png
Sleep 1s
