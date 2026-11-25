# The function call's status gets the nested error issued when its policy
# scope closes, so the call fails and -P processing stops after it.
function(f)
  cmake_policy(PUSH)
endfunction()
f()
message("not reached")
