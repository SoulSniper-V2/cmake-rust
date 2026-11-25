# while() expands its condition before the while() context is pushed, so an
# expansion error is reported from the enclosing context (here the call of
# f() inside foreach()), not "at <file>:<line> (while)".
function(f)
  set(i 0)
  while(i LESS 2 AND "\q")
    math(EXPR i "${i}+1")
    message("i=${i}")
  endwhile()
endfunction()
foreach(x a)
  f()
endforeach()
message("after")
