set(path [=[a//b]=])
cmake_path(APPEND path c "" OUTPUT_VARIABLE out)
message("[${out}]")
