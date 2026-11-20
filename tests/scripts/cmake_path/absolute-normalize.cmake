set(path [=[a/../b]=])
cmake_path(ABSOLUTE_PATH path BASE_DIRECTORY /x//y NORMALIZE OUTPUT_VARIABLE out)
message("[${out}]")
