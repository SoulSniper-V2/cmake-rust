set(path [=[a/b]=])
cmake_path(APPEND_STRING path .tar .gz OUTPUT_VARIABLE out)
message("[${out}]")
