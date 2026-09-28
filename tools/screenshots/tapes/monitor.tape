# Resource monitor: the CPU and network modals, reached from the menu bar by
# keyboard. termide runs in its own
# PID namespace with two demo services, so the lists show only the demo.
# docker: --cap-add SYS_ADMIN
Source tapes/_settings.tape
Hide
Type "source /opt/shots/env/setup.sh" Enter
Sleep 2s
Type "/opt/shots/env/demo-ns.sh termide" Enter
Sleep 5s
Show
Alt+M
Sleep 400ms
# Left from Bookmarks: disk modal, then the calendar on the pinned Sunday
# (rightmost column): six steps to Monday, a seventh leaves for RAM.
Left
Sleep 500ms
Left
Sleep 500ms
Left
Sleep 250ms
Left
Sleep 250ms
Left
Sleep 250ms
Left
Sleep 250ms
Left
Sleep 250ms
Left
Sleep 250ms
Left
Sleep 250ms
Sleep 1s
Left
Sleep 5s
Screenshot out/monitor.png
Left
Sleep 2s
Screenshot out/monitor-network.png
Sleep 1.5s
