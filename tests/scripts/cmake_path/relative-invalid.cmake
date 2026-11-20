set(path [=[a/z]=])
cmake_path(RELATIVE_PATH path BASE_DIRECTORY a/../.. OUTPUT_VARIABLE out)
message("[${out}]")
