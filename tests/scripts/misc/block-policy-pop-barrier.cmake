# block() pushes its policy entry before its variable scope, whose snapshot
# starts a new policy scope: cmake_policy(POP) cannot pop the block's entry.
message("a")
block()
  cmake_policy(POP)
  message("not reached")
endblock()
