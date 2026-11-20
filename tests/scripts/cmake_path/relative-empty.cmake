set(path [=[a/]=])
cmake_path(RELATIVE_PATH path BASE_DIRECTORY a OUTPUT_VARIABLE out)
message("[${out}]")
