# The recursion limit is checked for the if() command itself; its body is
# replayed one level up.
set(CMAKE_MAXIMUM_RECURSION_DEPTH 6)
function(r n)
  math(EXPR n "${n}+1")
  if(n)
    r(${n})
  endif()
endfunction()
r(0)
