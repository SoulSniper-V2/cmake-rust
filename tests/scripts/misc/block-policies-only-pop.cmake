# A POLICIES-only block has no snapshot of its own: POP pops the block's
# entry, then endblock's PopPolicy pops the enclosing function's entry, and
# the function's own PopPolicy then fails.
function(f)
  block(SCOPE_FOR POLICIES)
    cmake_policy(POP)
  endblock()
  message("in f")
endfunction()
f()
message("not reached")
