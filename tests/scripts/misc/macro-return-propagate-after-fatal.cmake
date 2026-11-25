# RunListFile stops on the fatal error raised when the macro scope closes,
# before it handles the macro's return(PROPAGATE).
macro(m)
  cmake_policy(POP)
  block()
    return(PROPAGATE bp)
  endblock()
endmacro()
m()
