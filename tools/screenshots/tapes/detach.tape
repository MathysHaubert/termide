# Detached instance: start one in the background, attach, run something,
# detach, list instances, attach again to find it all where it was.
Output out/detach.gif
Source tapes/_settings.tape
Hide
Type "source /opt/shots/env/setup.sh" Enter
Sleep 2s
Show
Sleep 500ms
Type "termide --detached" Sleep 300ms Enter
Sleep 2s
Type "termide --attach" Sleep 300ms Enter
Sleep 3s
Alt+t
Sleep 1.5s
Type "python3 -m inventory.cli report --store north" Enter
Sleep 1.5s
Screenshot out/detach-running.png
Alt+d
Sleep 2s
Type "termide --list-instances" Sleep 300ms Enter
Sleep 1.5s
Screenshot out/detach-list.png
Sleep 800ms
Type "termide --attach" Sleep 300ms Enter
Sleep 3s
Screenshot out/detach-attached.png
Sleep 1s
