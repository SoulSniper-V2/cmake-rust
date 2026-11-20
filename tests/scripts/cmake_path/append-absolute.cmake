set(path [=[a/b]=])
cmake_path(APPEND path /c d OUTPUT_VARIABLE out)
message("[${out}]")
