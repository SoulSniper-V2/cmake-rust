set(x "expanded")
cmake_language(CALL message "value=[=[${x}]=]")
cmake_language(CALL set out "${x}-2")
message("out=${out}")
