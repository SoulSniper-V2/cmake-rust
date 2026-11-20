set(path [=[a/../../b/.]=])
cmake_path(NORMAL_PATH path OUTPUT_VARIABLE out)
message("[${out}]")
