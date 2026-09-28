# Tour only: hide, quit termide (Enter answers the quit confirmation that a
# running terminal raises; with no prompt it lands on an empty shell line),
# rebuild the project and configuration from scratch and start termide again.
Hide
Alt+q
Sleep 800ms
Enter
Sleep 1s
Type "source /opt/shots/env/setup.sh" Enter
Sleep 2s
Type "termide --no-lsp" Enter
Sleep 3s
