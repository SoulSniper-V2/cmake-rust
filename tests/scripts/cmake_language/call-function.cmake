function(probe one two)
  message("${one}|${two}")
endfunction()
cmake_language(CALL probe "a;b" [[c]])
