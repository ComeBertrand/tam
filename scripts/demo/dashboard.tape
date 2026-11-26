# The dashboard in the demo world: a screenshot and a short GIF.
#
#   scripts/demo/setup.sh && vhs scripts/demo/dashboard.tape
#
# Run from the repository's root, after setup.sh (see README.md).

Output scripts/demo/dashboard.gif

Set Shell "bash"
Set FontSize 14
Set Width 1680
Set Height 860
Set Padding 16
Set TypingSpeed 80ms
Env COLORTERM "truecolor"

Hide
Type ". scripts/demo/env.sh && clear"
Enter
Sleep 500ms
Show

Type "tam"
Enter
Sleep 3s
Screenshot scripts/demo/dashboard.png

# Down to the blocked task (past the chats and the shells), and allow
# its command.
Type@600ms "jjjjjj"
Sleep 1.5s
Type "a"
Sleep 2s
Type "y"
Sleep 4s

# The task across two repositories: the pane shows its member asking a
# question.
Type@600ms "jjjjj"
Sleep 2.5s

# Attention first, then back.
Type "t"
Sleep 2.5s
Type "t"
Sleep 1.5s

Type "q"
Sleep 500ms
